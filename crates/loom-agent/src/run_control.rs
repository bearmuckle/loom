use super::*;

pub(crate) struct PendingApproval {
    pub(crate) call: ToolCall,
}

/// Out-of-band control of a run that is executing.
///
/// A control request never waits for the run: it raises a flag and cancels the
/// in-flight model stream, and the run loop applies the transition at its next
/// checkpoint.
#[derive(Clone, Debug, Default)]
pub struct RunControl {
    inner: Arc<RunControlState>,
}

#[derive(Debug, Default)]
struct RunControlState {
    interrupt: AtomicBool,
    pause: AtomicBool,
    stream: Mutex<CancellationToken>,
}

impl RunControl {
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks the run to stop and finish as cancelled.
    pub fn request_interrupt(&self) {
        let stream = self.locked_stream();
        self.inner.interrupt.store(true, Ordering::SeqCst);
        stream.cancel();
    }

    /// Asks the run to stop at its next checkpoint and stay resumable.
    pub fn request_pause(&self) {
        let stream = self.locked_stream();
        self.inner.pause.store(true, Ordering::SeqCst);
        stream.cancel();
    }

    pub fn is_interrupt_requested(&self) -> bool {
        self.inner.interrupt.load(Ordering::SeqCst)
    }

    pub fn is_pause_requested(&self) -> bool {
        self.inner.pause.load(Ordering::SeqCst)
    }

    /// True while a requested pause or interrupt has not been applied yet.
    pub fn is_stopping(&self) -> bool {
        self.is_interrupt_requested() || self.is_pause_requested()
    }

    /// Token handed to the provider for the next model call.
    pub fn stream_token(&self) -> CancellationToken {
        self.locked_stream().clone()
    }

    /// Clears a stop request after the owner has applied it directly.
    pub fn clear_request(&self) {
        let mut stream = self.locked_stream();
        self.inner.interrupt.store(false, Ordering::SeqCst);
        self.inner.pause.store(false, Ordering::SeqCst);
        *stream = CancellationToken::new();
    }

    fn locked_stream(&self) -> std::sync::MutexGuard<'_, CancellationToken> {
        self.inner
            .stream
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Receives every agent event as it is produced.
pub type AgentEventObserver = Arc<dyn Fn(&AgentEvent) + Send + Sync>;

/// Events produced by one run operation, and whether the run still has work.
#[derive(Clone, Debug)]
pub struct RunProgress {
    pub events: Vec<AgentEvent>,
    pub continues: bool,
}

impl RunProgress {
    pub(crate) fn running(events: Vec<AgentEvent>) -> Self {
        Self {
            events,
            continues: true,
        }
    }

    pub(crate) fn blocked(events: Vec<AgentEvent>) -> Self {
        Self {
            events,
            continues: false,
        }
    }
}
