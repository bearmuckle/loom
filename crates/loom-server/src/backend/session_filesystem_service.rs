use super::*;

/// Owns loaded session filesystems, the set restored from disk, and the guard
/// that serializes filesystem restore.
#[derive(Default)]
pub(crate) struct SessionFilesystemService {
    filesystems: Mutex<BTreeMap<AgentSessionId, Workspace>>,
    persisted: Mutex<BTreeSet<AgentSessionId>>,
    restore: Mutex<()>,
}

impl SessionFilesystemService {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn filesystems(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<AgentSessionId, Workspace>>> {
        self.filesystems.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session filesystem manager lock was poisoned",
                true,
            )
        })
    }

    pub(crate) fn persisted(&self) -> Result<MutexGuard<'_, BTreeSet<AgentSessionId>>> {
        self.persisted.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "persisted session filesystem manager lock was poisoned",
                true,
            )
        })
    }

    pub(crate) fn restore_guard(&self) -> Result<MutexGuard<'_, ()>> {
        self.restore.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session filesystem restore lock was poisoned",
                true,
            )
        })
    }
}
