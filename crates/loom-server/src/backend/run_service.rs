use super::*;

/// Owns the live run handles and their persisted summaries.
#[derive(Default)]
pub(crate) struct RunService {
    runs: Mutex<BTreeMap<loom_core::RunId, Arc<RunHandle>>>,
    persisted_runs: Mutex<BTreeMap<loom_core::RunId, PersistedRunSummary>>,
}

impl RunService {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn runs(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<loom_core::RunId, Arc<RunHandle>>>> {
        self.runs
            .lock()
            .map_err(|_| LoomError::new(ErrorCode::Internal, "agent run lock was poisoned", true))
    }

    pub(crate) fn persisted_runs(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<loom_core::RunId, PersistedRunSummary>>> {
        self.persisted_runs.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "persisted run summary lock was poisoned",
                true,
            )
        })
    }
}
