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
        self.session_service.sessions()
    }

    pub(crate) fn workspace_records(
        &self,
    ) -> Result<MutexGuard<'_, loom_session::WorkspaceManager>> {
        self.workspace_service.records()
    }

    pub(crate) fn session_filesystems(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<AgentSessionId, Workspace>>> {
        self.session_filesystem_service.filesystems()
    }

    pub(crate) fn persisted_session_filesystems(
        &self,
    ) -> Result<MutexGuard<'_, BTreeSet<AgentSessionId>>> {
        self.session_filesystem_service.persisted()
    }

    pub(crate) fn session_repositories(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<AgentSessionId, BTreeMap<RepositoryId, SessionRepository>>>>
    {
        self.repository_service.records()
    }

    pub(crate) fn session_vcs(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<(AgentSessionId, RepositoryId), GitService>>> {
        self.repository_service.vcs()
    }

    /// Repositories this worker node has already cloned and cached.
    pub(crate) fn cached_repositories(&self) -> Result<Vec<ClonedRepository>> {
        self.repository_service.cached_repositories()
    }

    /// Existing mirror for a clone URL, when this node has cloned it before.
    pub(crate) fn cached_repository_mirror(&self, clone_url: &str) -> Result<Option<PathBuf>> {
        self.repository_service.cached_mirror(clone_url)
    }

    /// Destination mirror path for a clone URL, whether or not it exists yet.
    pub(crate) fn repository_mirror_path(&self, clone_url: &str) -> Result<PathBuf> {
        self.repository_service.mirror_path(clone_url)
    }

    /// Record a cached clone so later sessions can reuse it.
    pub(crate) fn register_cloned_repository(&self, repository: &ClonedRepository) -> Result<()> {
        self.repository_service
            .register_cloned_repository(repository)
    }

    pub(crate) fn session_task_supervisors(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<AgentSessionId, TaskSupervisor>>> {
        self.process_service.task_supervisors()
    }

    pub(crate) fn session_terminals(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<loom_core::TerminalId, AgentSessionId>>> {
        self.process_service.session_terminals()
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
        self.session_service.policies()
    }

    pub(crate) fn auto_approve_actions(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<AgentSessionId, bool>>> {
        self.session_service.auto_approve_actions()
    }

    pub(crate) fn workspace_configs(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<WorkspaceId, WorkspaceConfig>>> {
        self.workspace_service.configs()
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
