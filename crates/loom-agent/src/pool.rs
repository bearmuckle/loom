use std::{
    collections::VecDeque,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Condvar, Mutex, OnceLock, PoisonError,
        mpsc::{SyncSender, sync_channel},
    },
    thread,
};

/// Default ceiling on concurrently executing read-only tool calls.
pub(crate) const DEFAULT_READ_TOOL_CONCURRENCY: usize = 4;

/// The process-wide read-only tool pool used by agent runtimes.
///
/// A shared pool keeps the concurrency ceiling global instead of multiplying it
/// by the number of active runs, which is the point of bounding the resource.
pub(crate) fn shared_read_pool() -> Arc<BoundedPool> {
    static SHARED: OnceLock<Arc<BoundedPool>> = OnceLock::new();
    Arc::clone(SHARED.get_or_init(|| Arc::new(BoundedPool::new(DEFAULT_READ_TOOL_CONCURRENCY))))
}

type Job = Box<dyn FnOnce() + Send + 'static>;

/// A lazily-started, fixed-size worker pool.
///
/// Read-only tool calls used to run on a fresh thread per call via
/// `std::thread::scope`. That made concurrency incidental to the batch size
/// instead of an explicit, bounded resource. This pool starts at most `limit`
/// workers on demand, reuses them across batches, and queues any excess work,
/// so a single run can never spawn unbounded tool threads.
pub(crate) struct BoundedPool {
    limit: usize,
    shared: Arc<PoolShared>,
}

struct PoolShared {
    state: Mutex<PoolState>,
    work: Condvar,
}

struct PoolState {
    queue: VecDeque<Job>,
    spawned: usize,
    idle: usize,
}

impl BoundedPool {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            limit: limit.max(1),
            shared: Arc::new(PoolShared {
                state: Mutex::new(PoolState {
                    queue: VecDeque::new(),
                    spawned: 0,
                    idle: 0,
                }),
                work: Condvar::new(),
            }),
        }
    }

    /// Runs `job` on the pool, blocking until it completes.
    ///
    /// A panic inside `job` is caught and returned as an error so one failing
    /// tool cannot poison the shared pool.
    #[cfg(test)]
    pub(crate) fn execute<R: Send + 'static>(
        &self,
        job: impl FnOnce() -> R + Send + 'static,
    ) -> thread::Result<R> {
        let receiver = self.submit(job);
        self.recv(receiver)
    }

    /// Submits every job, then blocks until all of them complete. Jobs overlap
    /// up to the pool limit and results are returned in submission order.
    pub(crate) fn execute_batch<R, F>(&self, jobs: Vec<F>) -> Vec<thread::Result<R>>
    where
        R: Send + 'static,
        F: FnOnce() -> R + Send + 'static,
    {
        let receivers = jobs
            .into_iter()
            .map(|job| self.submit(job))
            .collect::<Vec<_>>();
        // A batch queues several jobs at once, so wake every parked worker
        // instead of relying on one notification per submit.
        self.shared.work.notify_all();
        receivers
            .into_iter()
            .map(|receiver| self.recv(receiver))
            .collect()
    }

    fn submit<R: Send + 'static>(
        &self,
        job: impl FnOnce() -> R + Send + 'static,
    ) -> std::sync::mpsc::Receiver<thread::Result<R>> {
        let (sender, receiver) = sync_channel::<thread::Result<R>>(1);
        let task: Job = Box::new(move || dispatch(sender, job));
        let fallback = {
            let mut state = self.lock();
            state.queue.push_back(task);
            self.ensure_worker(&mut state)
        };
        if let Some(job) = fallback {
            // The OS refused a worker thread; run the queued job inline so its
            // caller is not stranded.
            job();
        }
        self.shared.work.notify_one();
        receiver
    }

    fn recv<R>(&self, receiver: std::sync::mpsc::Receiver<thread::Result<R>>) -> thread::Result<R> {
        receiver
            .recv()
            .unwrap_or_else(|_| Err(Box::new("read-only tool pool worker disappeared")))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PoolState> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts a worker when queued work has no idle worker to pick it up.
    ///
    /// Returns a job to run on the calling thread when a worker thread could
    /// not be started, so queued work is never lost.
    fn ensure_worker(&self, state: &mut PoolState) -> Option<Job> {
        if state.queue.is_empty() || state.idle > 0 || state.spawned >= self.limit {
            return None;
        }
        state.spawned += 1;
        let shared = Arc::clone(&self.shared);
        let name = format!("loom-tool-{}", state.spawned);
        if thread::Builder::new()
            .name(name)
            .spawn(move || worker(shared))
            .is_err()
        {
            state.spawned -= 1;
            return state.queue.pop_front();
        }
        None
    }
}

fn dispatch<R: Send + 'static>(sender: SyncSender<thread::Result<R>>, job: impl FnOnce() -> R) {
    let result = catch_unwind(AssertUnwindSafe(job));
    // The receiver blocks until this send; a missing receiver means the caller
    // was cancelled, which is not an error for the pool.
    let _ = sender.send(result);
}

fn worker(shared: Arc<PoolShared>) {
    loop {
        let job = {
            let mut state = shared.state.lock().unwrap_or_else(PoisonError::into_inner);
            loop {
                if let Some(job) = state.queue.pop_front() {
                    break job;
                }
                state.idle += 1;
                let mut guard = shared
                    .work
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
                guard.idle = guard.idle.saturating_sub(1);
                state = guard;
            }
        };
        job();
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use super::*;

    #[test]
    fn a_bounded_pool_overlaps_without_exceeding_its_limit() {
        let pool = Arc::new(BoundedPool::new(2));
        let current = Arc::new(AtomicUsize::new(0));
        let max = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..6 {
            let pool = Arc::clone(&pool);
            let current = Arc::clone(&current);
            let max = Arc::clone(&max);
            handles.push(std::thread::spawn(move || {
                pool.execute(move || {
                    let active = current.fetch_add(1, Ordering::SeqCst) + 1;
                    max.fetch_max(active, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(60));
                    current.fetch_sub(1, Ordering::SeqCst);
                })
                .unwrap();
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        let observed = max.load(Ordering::SeqCst);
        assert!(observed >= 2, "work should overlap, observed {observed}");
        assert!(
            observed <= 2,
            "work must stay within the pool limit, observed {observed}"
        );
    }

    #[test]
    fn a_panicking_job_is_reported_without_stopping_the_pool() {
        let pool = BoundedPool::new(1);
        assert!(
            pool.execute(|| panic!("boom")).is_err(),
            "a panic must surface as an error"
        );
        assert_eq!(pool.execute(|| 7).unwrap(), 7);
    }
}
