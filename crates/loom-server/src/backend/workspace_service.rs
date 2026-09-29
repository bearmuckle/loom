use super::*;

/// Owns workspace records and workspace configuration.
#[derive(Default)]
pub(crate) struct WorkspaceService {
    records: Mutex<WorkspaceManager>,
    configs: Mutex<BTreeMap<WorkspaceId, WorkspaceConfig>>,
}

impl WorkspaceService {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn records(&self) -> Result<MutexGuard<'_, WorkspaceManager>> {
        self.records.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "workspace record manager lock was poisoned",
                true,
            )
        })
    }

    pub(crate) fn configs(&self) -> Result<MutexGuard<'_, BTreeMap<WorkspaceId, WorkspaceConfig>>> {
        self.configs.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "workspace config lock was poisoned",
                true,
            )
        })
    }
}
