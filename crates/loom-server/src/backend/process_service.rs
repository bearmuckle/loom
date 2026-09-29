use super::*;

/// Owns session task supervisors and the terminal-to-session mapping.
#[derive(Default)]
pub(crate) struct ProcessService {
    task_supervisors: Mutex<BTreeMap<AgentSessionId, TaskSupervisor>>,
    session_terminals: Mutex<BTreeMap<loom_core::TerminalId, AgentSessionId>>,
}

impl ProcessService {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn task_supervisors(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<AgentSessionId, TaskSupervisor>>> {
        self.task_supervisors.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session task supervisor lock was poisoned",
                true,
            )
        })
    }

    pub(crate) fn session_terminals(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<loom_core::TerminalId, AgentSessionId>>> {
        self.session_terminals.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session terminal lock was poisoned",
                true,
            )
        })
    }
}
