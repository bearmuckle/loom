use super::*;

/// Owns session lifecycle state and per-session approval settings.
#[derive(Default)]
pub(crate) struct SessionService {
    sessions: Mutex<SessionManager>,
    policies: Mutex<BTreeMap<AgentSessionId, ApprovalPolicy>>,
    auto_approve_actions: Mutex<BTreeMap<AgentSessionId, bool>>,
}

impl SessionService {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn sessions(&self) -> Result<MutexGuard<'_, SessionManager>> {
        self.sessions.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session manager lock was poisoned",
                true,
            )
        })
    }

    pub(crate) fn policies(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<AgentSessionId, ApprovalPolicy>>> {
        self.policies.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session approval policy lock was poisoned",
                true,
            )
        })
    }

    pub(crate) fn auto_approve_actions(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<AgentSessionId, bool>>> {
        self.auto_approve_actions.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session approval settings lock was poisoned",
                true,
            )
        })
    }
}
