use super::*;

impl InProcessBackend {
    pub fn connect(self: &Arc<Self>) -> InProcessConnection {
        InProcessConnection {
            backend: Arc::clone(self),
            negotiated_capabilities: Arc::new(Mutex::new(None)),
            auth: None,
        }
    }

    pub fn connect_authenticated(self: &Arc<Self>, auth: AuthSession) -> InProcessConnection {
        InProcessConnection {
            backend: Arc::clone(self),
            negotiated_capabilities: Arc::new(Mutex::new(None)),
            auth: Some(auth),
        }
    }

    pub fn provider_registry(&self) -> ProviderRegistry {
        self.providers.clone()
    }

    pub(crate) fn provider(&self, model: &ModelId) -> Result<Box<dyn ModelProvider>> {
        self.providers.create_provider(model)
    }

    pub(crate) fn provider_at(
        &self,
        model: &ModelId,
        cursor: usize,
    ) -> Result<Box<dyn ModelProvider>> {
        self.providers.create_provider_at(model, cursor)
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

    pub(crate) fn workspace_records(
        &self,
    ) -> Result<MutexGuard<'_, loom_session::WorkspaceManager>> {
        self.workspace_records.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "workspace record manager lock was poisoned",
                true,
            )
        })
    }

    pub(crate) fn session_filesystems(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<AgentSessionId, Workspace>>> {
        self.session_filesystems.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session filesystem manager lock was poisoned",
                true,
            )
        })
    }

    pub(crate) fn persisted_session_filesystems(
        &self,
    ) -> Result<MutexGuard<'_, BTreeSet<AgentSessionId>>> {
        self.persisted_session_filesystems.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "persisted session filesystem manager lock was poisoned",
                true,
            )
        })
    }

    pub(crate) fn session_repositories(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<AgentSessionId, BTreeMap<RepositoryId, SessionRepository>>>>
    {
        self.session_repositories.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session repository manager lock was poisoned",
                true,
            )
        })
    }

    pub(crate) fn session_vcs(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<(AgentSessionId, RepositoryId), GitService>>> {
        self.session_vcs.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session Git service manager lock was poisoned",
                true,
            )
        })
    }

    pub(crate) fn session_task_supervisors(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<AgentSessionId, TaskSupervisor>>> {
        self.session_task_supervisors.lock().map_err(|_| {
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

    pub(crate) fn runs(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<loom_core::RunId, Arc<RunHandle>>>> {
        self.run_service.runs()
    }

    pub(crate) fn persisted_runs(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<loom_core::RunId, PersistedRunSummary>>> {
        self.run_service.persisted_runs()
    }

    pub(crate) fn journal(&self) -> Result<MutexGuard<'_, EventJournal>> {
        self.journal.lock().map_err(|_| {
            LoomError::new(ErrorCode::Internal, "event journal lock was poisoned", true)
        })
    }

    pub(crate) fn session_policies(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<AgentSessionId, ApprovalPolicy>>> {
        self.session_policies.lock().map_err(|_| {
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

    pub(crate) fn workspace_configs(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<WorkspaceId, WorkspaceConfig>>> {
        self.workspace_configs.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "workspace config lock was poisoned",
                true,
            )
        })
    }

    pub(crate) fn resource_monitor(&self) -> Result<MutexGuard<'_, ResourceMonitor>> {
        self.resource_monitor.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "worker resource monitor lock was poisoned",
                true,
            )
        })
    }

    pub fn set_event_retention(&self, limit: usize) -> Result<()> {
        if limit == 0 {
            return Err(LoomError::invalid_request(
                "event retention limit must be greater than zero",
            ));
        }
        self.journal()?.set_retention(limit);
        Ok(())
    }

    pub fn event_retention(&self) -> Result<usize> {
        Ok(self.journal()?.retention_limit.max(1))
    }
}
