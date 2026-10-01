use std::{
    sync::{Condvar, Mutex, OnceLock, PoisonError, mpsc},
    time::Duration,
};

use loom_core::{ErrorCode, LoomError};
use tokio::{
    runtime::{Builder, Handle},
    sync::Semaphore,
};

/// Default number of agent runs that may execute concurrently on the runtime.
pub(crate) const DEFAULT_MAX_CONCURRENT_RUNS: usize = 4;
/// Default time a fresh run may wait for a concurrency slot before it is
/// rejected. This is the backpressure bound: without it an unbounded queue of
/// runs would wait forever during an outage or overload.
pub(crate) const DEFAULT_RUN_ADMISSION_TIMEOUT: Duration = Duration::from_secs(300);

/// The runtime worker entry point. Building the runtime on a dedicated,
/// non-async thread means the executor can be created from any context,
/// including from inside another Tokio runtime (the remote server).
fn runtime_handle() -> Handle {
    static RUNTIME: OnceLock<Handle> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            let (sender, receiver) = mpsc::channel();
            std::thread::Builder::new()
                .name("loom-runtime-init".to_owned())
                .spawn(move || {
                    let runtime = match Builder::new_multi_thread()
                        .worker_threads(2)
                        .thread_name("loom-runtime")
                        .enable_all()
                        .build()
                    {
                        Ok(runtime) => runtime,
                        Err(error) => {
                            let _ = sender.send(Err(error.to_string()));
                            return;
                        }
                    };
                    let _ = sender.send(Ok(runtime.handle().clone()));
                    // Keep the runtime alive for the process lifetime. A
                    // per-backend runtime would be dropped on whichever thread
                    // finishes the last run, which can be a runtime worker.
                    runtime.block_on(std::future::pending::<()>());
                })
                .expect("could not start the Loom runtime init thread");
            receiver
                .recv()
                .expect("the Loom runtime init thread exited without reporting")
                .expect("could not build the Loom run runtime")
        })
        .clone()
}

#[derive(Default)]
struct RunCompletion {
    done: Mutex<bool>,
    panicked: Mutex<Option<String>>,
    finished: Condvar,
}

impl RunCompletion {
    fn finish(&self) {
        *self.done.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.finished.notify_all();
    }

    fn panic(&self, message: impl Into<String>) {
        let mut panicked = self.panicked.lock().unwrap_or_else(PoisonError::into_inner);
        if panicked.is_none() {
            *panicked = Some(message.into());
        }
    }

    fn panicked(&self) -> Option<String> {
        self.panicked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn wait(&self) {
        let mut done = self.done.lock().unwrap_or_else(PoisonError::into_inner);
        while !*done {
            done = self
                .finished
                .wait(done)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// A handle to a run task scheduled on the executor's runtime.
pub(crate) struct RunTask {
    completion: std::sync::Arc<RunCompletion>,
}

impl RunTask {
    /// Blocks until the task (or its admission) has finished.
    pub(crate) fn wait(&self) {
        self.completion.wait();
    }

    /// Returns the panic message if the blocking body panicked.
    pub(crate) fn failure(&self) -> Option<String> {
        self.completion.panicked()
    }
}

/// Bounded executor for agent run workers.
///
/// The agents are synchronous, so each run executes on a `spawn_blocking`
/// thread. A semaphore caps how many runs execute at once and bounds the queue
/// that can wait; a waiter that exceeds the admission timeout is rejected with
/// a retryable deadline error instead of occupying a slot indefinitely.
pub(crate) struct RunExecutor {
    handle: Handle,
    permits: std::sync::Arc<Semaphore>,
    admission_timeout: Duration,
}

impl RunExecutor {
    pub(crate) fn with_defaults() -> Self {
        Self::new(DEFAULT_MAX_CONCURRENT_RUNS, DEFAULT_RUN_ADMISSION_TIMEOUT)
    }

    pub(crate) fn new(max_concurrent_runs: usize, admission_timeout: Duration) -> Self {
        Self {
            handle: runtime_handle(),
            permits: std::sync::Arc::new(Semaphore::new(max_concurrent_runs.max(1))),
            admission_timeout,
        }
    }

    /// Schedules a blocking run body once a concurrency slot is free.
    ///
    /// `task` is the run worker loop. `on_reject` is called (off the async
    /// workers) when the slot could not be acquired in time, so the owner can
    /// settle the run instead of leaving it running with no worker.
    pub(crate) fn spawn_bounded<T, R>(
        &self,
        name: impl Into<String>,
        task: T,
        on_reject: R,
    ) -> RunTask
    where
        T: FnOnce() + Send + 'static,
        R: FnOnce(LoomError) + Send + 'static,
    {
        let permits = std::sync::Arc::clone(&self.permits);
        let admission_timeout = self.admission_timeout;
        let completion = std::sync::Arc::new(RunCompletion::default());
        let task_completion = std::sync::Arc::clone(&completion);
        let name = name.into();
        let task_name = name.clone();
        self.handle.spawn(async move {
            match tokio::time::timeout(admission_timeout, permits.acquire_owned()).await {
                Ok(Ok(permit)) => {
                    let joined = tokio::task::spawn_blocking(move || {
                        let _permit = permit;
                        task();
                    })
                    .await;
                    if let Err(error) = joined {
                        task_completion.panic(error.to_string());
                    }
                }
                Ok(Err(_closed)) => {
                    task_completion.panic("run executor was closed");
                    reject(on_reject, run_admission_error(&task_name, true)).await;
                }
                Err(_elapsed) => {
                    task_completion.panic("run admission timed out");
                    reject(on_reject, run_admission_error(&task_name, false)).await;
                }
            }
            task_completion.finish();
        });
        RunTask { completion }
    }

    /// Schedules an unbounded blocking task, used for cheap per-run helpers
    /// such as the transcript-fragment flusher.
    pub(crate) fn spawn_blocking<T>(&self, _name: impl Into<String>, task: T) -> RunTask
    where
        T: FnOnce() + Send + 'static,
    {
        let completion = std::sync::Arc::new(RunCompletion::default());
        let task_completion = std::sync::Arc::clone(&completion);
        let handle = self.handle.spawn_blocking(move || {
            task();
        });
        self.handle.spawn(async move {
            if let Err(error) = handle.await {
                task_completion.panic(error.to_string());
            }
            task_completion.finish();
        });
        RunTask { completion }
    }
}

async fn reject<R: FnOnce(LoomError) + Send + 'static>(on_reject: R, error: LoomError) {
    let _ = tokio::task::spawn_blocking(move || on_reject(error)).await;
}

fn run_admission_error(name: &str, closed: bool) -> LoomError {
    let message = if closed {
        format!("{name} could not start because the run executor was closed")
    } else {
        format!("{name} waited too long for a run concurrency slot")
    };
    LoomError::new(ErrorCode::DeadlineExceeded, message, true)
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        time::Duration,
    };

    use super::*;

    #[test]
    fn runs_overlap_without_exceeding_the_concurrency_limit() {
        let executor = RunExecutor::new(2, Duration::from_secs(30));
        let current = Arc::new(AtomicUsize::new(0));
        let max = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..6 {
            let current = Arc::clone(&current);
            let max = Arc::clone(&max);
            tasks.push(executor.spawn_bounded(
                "test-run",
                move || {
                    let active = current.fetch_add(1, Ordering::SeqCst) + 1;
                    max.fetch_max(active, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(60));
                    current.fetch_sub(1, Ordering::SeqCst);
                },
                |_error| panic!("a slot should be available"),
            ));
        }
        for task in &tasks {
            task.wait();
        }
        let observed = max.load(Ordering::SeqCst);
        assert!(observed >= 2, "runs should overlap, observed {observed}");
        assert!(
            observed <= 2,
            "runs must stay within the concurrency limit, observed {observed}"
        );
    }

    #[test]
    fn a_run_that_waits_past_the_admission_timeout_is_rejected() {
        let executor = RunExecutor::new(1, Duration::from_millis(40));
        let (release_sender, release_receiver) = mpsc::channel::<()>();
        let (rejected_sender, rejected_receiver) = mpsc::channel::<LoomError>();
        let started = Arc::new(AtomicUsize::new(0));

        let blocking_started = Arc::clone(&started);
        let first = executor.spawn_bounded(
            "blocking-run",
            move || {
                blocking_started.fetch_add(1, Ordering::SeqCst);
                let _ = release_receiver.recv();
            },
            |_error| panic!("the first run should be admitted"),
        );
        while started.load(Ordering::SeqCst) == 0 {
            std::thread::sleep(Duration::from_millis(1));
        }

        let rejected_started = Arc::new(Mutex::new(false));
        let rejected_flag = Arc::clone(&rejected_started);
        let second = executor.spawn_bounded(
            "waiting-run",
            move || {
                *rejected_flag.lock().unwrap() = true;
            },
            move |error| {
                let _ = rejected_sender.send(error);
            },
        );
        let error = rejected_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("the second run should be rejected");
        assert_eq!(error.code, ErrorCode::DeadlineExceeded);
        assert!(error.retryable);
        second.wait();
        assert!(
            !*rejected_started.lock().unwrap(),
            "a rejected run must not execute"
        );

        release_sender.send(()).unwrap();
        first.wait();
    }
}
