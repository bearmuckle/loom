use super::*;

/// Owns attached session repositories and their Git services.
#[derive(Default)]
pub(crate) struct RepositoryService {
    records: Mutex<BTreeMap<AgentSessionId, BTreeMap<RepositoryId, SessionRepository>>>,
    vcs: Mutex<BTreeMap<(AgentSessionId, RepositoryId), GitService>>,
}

impl RepositoryService {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn records(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<AgentSessionId, BTreeMap<RepositoryId, SessionRepository>>>>
    {
        self.records.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session repository manager lock was poisoned",
                true,
            )
        })
    }

    pub(crate) fn vcs(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<(AgentSessionId, RepositoryId), GitService>>> {
        self.vcs.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session Git service manager lock was poisoned",
                true,
            )
        })
    }
}
