use super::*;

impl InProcessBackend {
    pub fn new() -> Arc<Self> {
        Self::with_provider_registry(ProviderRegistry::demo())
    }

    pub fn new_with_github_copilot() -> Result<Arc<Self>> {
        let credentials = github_copilot_credentials()?;
        Self::with_provider_registry_persistent_credentials(
            ProviderRegistry::configured(credentials)?,
            None,
        )
    }

    pub fn demo_with_github_copilot() -> Result<Arc<Self>> {
        let credentials = github_copilot_credentials()?;
        Self::with_provider_registry_persistent_credentials(
            ProviderRegistry::demo_with_credentials(credentials),
            None,
        )
    }

    pub fn with_models(models: Vec<ModelDescriptor>) -> Arc<Self> {
        let credentials = Arc::new(loom_providers::InMemoryCredentialStore::default());
        let providers = ProviderRegistry::with_credentials(credentials);
        for model in &models {
            let existing_provider = providers
                .list_models()
                .unwrap_or_else(|error| panic!("could not inspect provider models: {error}"))
                .iter()
                .any(|existing| existing.provider == model.provider);
            if existing_provider {
                if model.provider.as_str() == "deterministic" {
                    panic!(
                        "deterministic provider only supports model '{}'",
                        deterministic_descriptor().id.as_str()
                    );
                }
                providers
                    .add_model(&model.provider, model.clone())
                    .unwrap_or_else(|error| panic!("could not add provider model: {error}"));
                continue;
            }
            let result = if model.provider.as_str() == "deterministic" {
                providers.register(ProviderConfig::deterministic())
            } else if model.provider.as_str() == "ollama" {
                providers.register(ProviderConfig::ollama(
                    "http://127.0.0.1:11434",
                    model.id.clone(),
                ))
            } else {
                providers.register(ProviderConfig::openai_compatible(
                    model.provider.clone(),
                    "OpenAI-compatible model",
                    "http://127.0.0.1:8000/v1/chat/completions",
                    model.clone(),
                    None,
                ))
            };
            result.unwrap_or_else(|error| panic!("could not register provider model: {error}"));
        }
        Self::with_provider_registry_and_persistence(providers, None)
            .unwrap_or_else(|error| panic!("could not configure providers: {error}"))
    }

    pub fn with_openai_compatible(
        endpoint: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<ModelId>,
    ) -> Arc<Self> {
        let descriptor = openai_compatible_descriptor(model.into());
        let credentials = Arc::new(loom_providers::InMemoryCredentialStore::default());
        credentials.insert(CredentialRef::new("legacy-openai"), api_key.into());
        let providers = ProviderRegistry::with_credentials(credentials);
        let config = ProviderConfig::openai_compatible(
            "openai-compatible",
            "OpenAI-compatible model",
            endpoint,
            descriptor,
            Some(CredentialRef::new("legacy-openai")),
        );
        providers
            .register(config)
            .expect("legacy OpenAI provider configuration is valid");
        Self::with_provider_registry(providers)
    }

    pub fn with_openai_compatible_persistent(
        endpoint: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<ModelId>,
        path: impl Into<PathBuf>,
    ) -> Result<Arc<Self>> {
        let descriptor = openai_compatible_descriptor(model.into());
        let credentials = Arc::new(loom_providers::InMemoryCredentialStore::default());
        let api_key = api_key.into();
        let credential = if api_key.is_empty() {
            None
        } else {
            let reference = CredentialRef::new("ui-openai-compatible");
            credentials.insert(reference.clone(), api_key);
            Some(reference)
        };
        let providers = ProviderRegistry::with_credentials(credentials);
        providers.register(ProviderConfig::openai_compatible(
            "openai-compatible",
            "OpenAI-compatible model",
            endpoint,
            descriptor,
            credential,
        ))?;
        Self::with_provider_registry_persistent(providers, path)
    }

    pub fn with_openai_compatible_persistent_with_github_copilot(
        endpoint: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<ModelId>,
        path: impl Into<PathBuf>,
    ) -> Result<Arc<Self>> {
        let descriptor = openai_compatible_descriptor(model.into());
        let credentials = github_copilot_credentials()?;
        let api_key = api_key.into();
        let credential = if api_key.trim().is_empty() {
            None
        } else {
            let reference = CredentialRef::new("ui-openai-compatible");
            credentials.insert(reference.clone(), api_key)?;
            Some(reference)
        };
        let providers = ProviderRegistry::with_credentials(credentials);
        providers.register(ProviderConfig::openai_compatible(
            "openai-compatible",
            "OpenAI-compatible model",
            endpoint,
            descriptor,
            credential,
        ))?;
        providers.register(ProviderConfig::github_copilot(CredentialRef::new(
            GITHUB_COPILOT_CREDENTIAL_REF,
        )))?;
        Self::with_provider_registry_persistent(providers, path)
    }

    pub fn with_provider_registry(providers: ProviderRegistry) -> Arc<Self> {
        Self::with_provider_registry_and_persistence(providers, None)
            .unwrap_or_else(|error| panic!("could not configure providers: {error}"))
    }

    pub fn new_persistent(path: impl Into<PathBuf>) -> Result<Arc<Self>> {
        let providers = ProviderRegistry::demo();
        Self::with_provider_registry_and_persistence(
            providers,
            Some(FilePersistence::open_exclusive_writer(path.into())?),
        )
    }

    pub fn new_persistent_with_github_copilot(path: impl Into<PathBuf>) -> Result<Arc<Self>> {
        let credentials = github_copilot_credentials()?;
        Self::with_provider_registry_persistent_credentials(
            ProviderRegistry::configured(credentials)?,
            Some(path.into()),
        )
    }

    pub fn open_persistent(path: impl Into<PathBuf>) -> Result<Arc<Self>> {
        Self::new_persistent(path)
    }

    pub fn with_persistence(path: impl Into<PathBuf>) -> Result<Arc<Self>> {
        Self::new_persistent(path)
    }

    pub fn with_provider_registry_persistent(
        providers: ProviderRegistry,
        path: impl Into<PathBuf>,
    ) -> Result<Arc<Self>> {
        Self::with_provider_registry_and_persistence(
            providers,
            Some(FilePersistence::open_exclusive_writer(path.into())?),
        )
    }

    pub(crate) fn with_provider_registry_persistent_credentials(
        providers: ProviderRegistry,
        path: Option<PathBuf>,
    ) -> Result<Arc<Self>> {
        Self::with_provider_registry_and_persistence(
            providers,
            path.map(FilePersistence::open_exclusive_writer)
                .transpose()?,
        )
    }

    pub(crate) fn with_provider_registry_and_persistence(
        providers: ProviderRegistry,
        persistence: Option<FilePersistence>,
    ) -> Result<Arc<Self>> {
        if let Some(persistence) = &persistence {
            let credential_path = persistence.path().with_extension("credentials.json");
            providers
                .scope_api_key_credentials(Arc::new(FileCredentialStore::open(credential_path)?))?;
        }
        let (node_id, node_name) = worker_node_identity();
        let session_root_base = persistence.as_ref().map_or_else(
            || {
                std::env::temp_dir().join(format!(
                    "loom-session-roots-{node_id}-{}",
                    WorkspaceId::new()
                ))
            },
            |persistence| persistence.path().with_extension("session-roots"),
        );
        let mut supported_capabilities = vec![
            Capability::CreateAgentSession,
            Capability::ReadAgentSession,
            Capability::ControlAgentSession,
            Capability::SubscribeSessionEvents,
            Capability::SubscribeWorkspaceEvents,
            Capability::StartAgentRun,
            Capability::ReadAgentRun,
            Capability::ReadAgentRunMessages,
            Capability::ControlAgentRun,
            Capability::PauseAgentRun,
            Capability::ResumeAgentRun,
            Capability::ForkAgentSession,
            Capability::RetryFromCheckpoint,
            Capability::ApproveAgentAction,
            Capability::ListProviders,
            Capability::ConfigureProviders,
            Capability::ReadProviderHealth,
            Capability::ReadUsage,
            Capability::InspectContext,
            Capability::ReadWorkspaceConfig,
            Capability::OpenSessionTerminal,
            Capability::ControlSessionTerminal,
            Capability::ReadSessionTask,
            Capability::StartSessionTask,
            Capability::ControlSessionTask,
            Capability::ConfigureApprovalPolicy,
            Capability::ManageCheckpoints,
            Capability::ReadVcsStatus,
            Capability::ReadVcsDiff,
            Capability::ReadSessionTaskEvidence,
            Capability::ReadWorkerNodeStatus,
            Capability::JsonProtocol,
            Capability::ManageWorkspaces,
            Capability::ManageSessionRepositories,
            Capability::BrowseGitHubRepositories,
            Capability::ReadProject,
            Capability::ReadSessionFilesystem,
            Capability::WriteSessionFilesystem,
        ];
        if persistence.is_some() {
            supported_capabilities.extend([
                Capability::CreateProjectChild,
                Capability::ControlProjectChild,
                Capability::SendProjectAgentMessage,
                Capability::SendProjectBranchMessage,
                Capability::ReadProjectAgentMessages,
                Capability::CreateProjectWorktree,
                Capability::ReadProjectChildReview,
                Capability::IntegrateProjectChild,
                Capability::CleanupProjectChildWorktree,
                Capability::CreateNestedProjectChild,
            ]);
        }
        let backend = Arc::new(Self {
            node_id,
            node_name,
            sessions: Mutex::new(SessionManager::default()),
            workspace_records: Mutex::new(loom_session::WorkspaceManager::default()),
            runs: Mutex::new(BTreeMap::new()),
            persisted_runs: Mutex::new(BTreeMap::new()),
            journal: Mutex::new(EventJournal::default()),
            last_feed_pruned_sequence: AtomicU64::new(0),
            feed_bytes_since_prune: AtomicUsize::new(0),
            session_filesystems: Mutex::new(BTreeMap::new()),
            persisted_session_filesystems: Mutex::new(BTreeSet::new()),
            session_filesystem_restore: Mutex::new(()),
            session_repositories: Mutex::new(BTreeMap::new()),
            session_vcs: Mutex::new(BTreeMap::new()),
            session_task_supervisors: Mutex::new(BTreeMap::new()),
            session_policies: Mutex::new(BTreeMap::new()),
            auto_approve_actions: Mutex::new(BTreeMap::new()),
            workspace_configs: Mutex::new(BTreeMap::new()),
            session_terminals: Mutex::new(BTreeMap::new()),
            terminals: TerminalManager::new(),
            resource_monitor: Mutex::new(ResourceMonitor::default()),
            supported_capabilities: CapabilitySet::new(supported_capabilities),
            providers,
            credentials: CredentialService::new(),
            persistence,
            session_root_base,
            idempotency_store: IdempotencyStore::new(),
            admissions: AdmissionService::new(),
            self_reference: Mutex::new(Weak::new()),
            request_lifecycle: RwLock::new(0),
            persistence_failed: AtomicBool::new(false),
            state_persist_gate: Mutex::new(()),
            #[cfg(test)]
            fail_next_state_save: AtomicBool::new(false),
            #[cfg(test)]
            project_cancellation_failpoint: AtomicUsize::new(0),
        });
        *backend
            .self_reference
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Arc::downgrade(&backend);
        backend.restore_persisted()?;
        Ok(backend)
    }

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

    pub(crate) fn restore_session_filesystem(
        &self,
        session_id: AgentSessionId,
    ) -> Result<Workspace> {
        if let Some(filesystem) = self.session_filesystems()?.get(&session_id).cloned() {
            return Ok(filesystem);
        }
        let _restore_guard = self.session_filesystem_restore.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session filesystem restore lock was poisoned",
                true,
            )
        })?;
        if let Some(filesystem) = self.session_filesystems()?.get(&session_id).cloned() {
            return Ok(filesystem);
        }
        if !self.persisted_session_filesystems()?.contains(&session_id) {
            return Err(LoomError::new(
                ErrorCode::RecoveryRequired,
                format!("filesystem for session {session_id} is unavailable"),
                true,
            ));
        }
        let durable = self
            .persistence
            .as_ref()
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::RecoveryRequired,
                    format!("filesystem for session {session_id} has no persistence store"),
                    true,
                )
            })?
            .load_filesystem_record(session_id)?
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted filesystem record for session {session_id} is missing"),
                    false,
                )
            })?;
        let mut payload = durable.payload;
        let repositories = durable.repositories;
        let directories = durable.directories;
        payload["filesystem"]["checkpoints"] = json_value(durable.checkpoints)?;
        let edits = durable.edits;
        let changes = durable.changes;
        let mut persisted: PersistedSessionFilesystem = from_json(payload)?;
        persisted.filesystem.edits = edits
            .into_iter()
            .map(|edit| loom_workspace::WorkspaceEditHistory {
                id: edit.id,
                path: edit.path,
                before: edit.before,
                before_bytes: edit.before_bytes,
                after_revision: edit.after_revision,
                source: edit.source,
            })
            .collect();
        persisted.filesystem.changes = changes;
        persisted.repositories = repositories;
        persisted.directories = directories;
        if durable.session_id != session_id
            || persisted.filesystem.session_id != session_id
            || persisted.filesystem.root != durable.root
            || persisted.filesystem.control != durable.control
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                format!("persisted filesystem identity for session {session_id} is invalid"),
                false,
            ));
        }
        let session = self.sessions()?.get(session_id)?;
        let expected_root = self
            .session_root_base
            .join(session.workspace_id.to_string())
            .join(session_id.to_string())
            .join("fs");
        let canonical_expected_root = fs::canonicalize(&expected_root).map_err(|error| {
            LoomError::new(
                ErrorCode::RecoveryRequired,
                format!("session filesystem root for {session_id} is unavailable: {error}"),
                true,
            )
        })?;
        if Path::new(&persisted.filesystem.root) != canonical_expected_root {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                format!("persisted filesystem root for session {session_id} is invalid"),
                false,
            ));
        }
        let filesystem = Workspace::open_for_restore(session_id, &canonical_expected_root)?;
        for directory in &persisted.directories {
            filesystem.mount_directory(&directory.path, &directory.source)?;
        }
        filesystem.restore_state(persisted.filesystem)?;
        let owned_worktree_repository_ids = self
            .persistence
            .as_ref()
            .map(|persistence| persistence.load_project_snapshot_for_session(session_id))
            .transpose()?
            .flatten()
            .map(|project| {
                project
                    .worktrees
                    .into_iter()
                    .filter(|worktree| worktree.child_session_id == session_id)
                    .map(|worktree| worktree.child_repository_id)
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        let mut missing_worktree_repositories = Vec::new();
        for (repository_id, repository) in &persisted.repositories {
            if *repository_id != repository.id {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("repository key does not match repository {}", repository.id),
                    false,
                ));
            }
            let path = filesystem.resolve_path(&repository.path, true)?;
            if !path.exists() && owned_worktree_repository_ids.contains(repository_id) {
                // The worktree record is authoritative for recovery. A missing
                // checkout must not prevent the child session from opening;
                // the scheduler or cleanup request will reconcile its durable
                // worktree state before using the repository.
                missing_worktree_repositories.push(*repository_id);
                continue;
            }
            let service = GitService::open(path)?;
            self.session_vcs()?
                .insert((session_id, *repository_id), service);
        }
        for repository_id in &missing_worktree_repositories {
            persisted.repositories.remove(repository_id);
        }
        self.session_repositories()?
            .insert(session_id, persisted.repositories);
        if !missing_worktree_repositories.is_empty() {
            filesystem.mark_state_dirty()?;
        }
        self.session_filesystems()?
            .insert(session_id, filesystem.clone());
        self.persisted_session_filesystems()?.remove(&session_id);
        Ok(filesystem)
    }

    pub(crate) fn persisted_filesystem_record(
        &self,
        session_id: AgentSessionId,
    ) -> Result<Option<PersistedSessionFilesystem>> {
        if !self.persisted_session_filesystems()?.contains(&session_id) {
            return Ok(None);
        }
        let Some(record) = self
            .persistence
            .as_ref()
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::RecoveryRequired,
                    format!("filesystem for session {session_id} has no persistence store"),
                    true,
                )
            })?
            .load_filesystem_record(session_id)?
        else {
            return Ok(None);
        };
        let mut payload = record.payload;
        let repositories = record.repositories;
        let directories = record.directories;
        payload["filesystem"]["checkpoints"] = json_value(record.checkpoints)?;
        let edits = record.edits;
        let changes = record.changes;
        let mut persisted: PersistedSessionFilesystem = from_json(payload)?;
        persisted.filesystem.edits = edits
            .into_iter()
            .map(|edit| loom_workspace::WorkspaceEditHistory {
                id: edit.id,
                path: edit.path,
                before: edit.before,
                before_bytes: edit.before_bytes,
                after_revision: edit.after_revision,
                source: edit.source,
            })
            .collect();
        persisted.filesystem.changes = changes;
        persisted.repositories = repositories;
        persisted.directories = directories;
        if record.session_id != session_id
            || persisted.filesystem.session_id != session_id
            || persisted.filesystem.root != record.root
            || persisted.filesystem.control != record.control
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                format!("persisted filesystem identity for session {session_id} is invalid"),
                false,
            ));
        }
        Ok(Some(persisted))
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

    pub(crate) fn set_workspace_config(
        self: &Arc<Self>,
        workspace_id: WorkspaceId,
        config: WorkspaceConfig,
    ) -> Result<()> {
        let unique_urls = config
            .worker_nodes
            .iter()
            .map(|node| node.url.as_str())
            .collect::<BTreeSet<_>>();
        if config.worker_nodes.len() > 64
            || unique_urls.len() != config.worker_nodes.len()
            || config
                .worker_nodes
                .iter()
                .any(|node| node.url.trim() != node.url || !worker_node_url_is_safe(&node.url))
            || !(loom_protocol::MIN_PROJECT_AGENT_CONCURRENCY
                ..=loom_protocol::MAX_PROJECT_AGENT_CONCURRENCY)
                .contains(&config.project_agent_concurrency)
        {
            return Err(LoomError::invalid_request(
                "workspace configuration must contain at most 64 safe worker-node URLs and project agent concurrency between 1 and 16",
            ));
        }
        let current_revision = self
            .workspace_configs()?
            .get(&workspace_id)
            .map(|config| config.revision);
        if current_revision.is_some_and(|revision| revision > config.revision) {
            return Ok(());
        }
        let previous_concurrency = self
            .workspace_configs()?
            .get(&workspace_id)
            .map(|config| config.project_agent_concurrency)
            .unwrap_or_else(|| WorkspaceConfig::default().project_agent_concurrency);
        let concurrency_changed = previous_concurrency != config.project_agent_concurrency;
        let revision = config.revision;
        let previous = self.workspace_configs()?.insert(workspace_id, config);
        let sequence = self
            .journal()?
            .append_workspace(workspace_id, WorkspaceEvent::ConfigChanged { revision });
        if let Err(error) = self.persist_state() {
            let mut configs = self.workspace_configs()?;
            if let Some(previous) = previous {
                configs.insert(workspace_id, previous);
            } else {
                configs.remove(&workspace_id);
            }
            self.journal()?.discard_pending_workspace(sequence);
            return Err(error);
        }
        if concurrency_changed {
            self.reconcile_project_tasks_and_resume_queued(false)?;
        }
        Ok(())
    }

    pub(crate) fn create_session_filesystem(
        &self,
        workspace_id: WorkspaceId,
        session_id: AgentSessionId,
    ) -> Result<Workspace> {
        let root = self
            .session_root_base
            .join(workspace_id.to_string())
            .join(session_id.to_string())
            .join("fs");
        fs::create_dir_all(&root).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not create session filesystem root: {error}"),
                false,
            )
        })?;
        Workspace::open(session_id, root)
    }

    pub(crate) fn restore_persisted(self: &Arc<Self>) -> Result<()> {
        let Some(persistence) = self.persistence.clone() else {
            return Ok(());
        };
        persistence.prune_expired_idempotency_records(Timestamp::from_unix_millis(
            current_unix_millis(),
        ))?;
        let startup_started = Instant::now();
        let mut needs_persist = false;
        let Some(sessions) = persistence.load_sessions()? else {
            let mut provider_configs = persistence.load_provider_configs()?;
            let migrated = self
                .providers
                .migrate_api_key_credentials(&mut provider_configs)?;
            self.providers.restore_configs(provider_configs)?;
            self.providers
                .restore_health(persistence.load_provider_health()?)?;
            self.providers
                .restore_usage(persistence.load_provider_usage()?)?;
            *self.workspace_configs()? = persistence.load_workspace_configs()?;
            if migrated {
                self.persist_state()?;
            }
            return Ok(());
        };
        let session_settings = persistence.load_session_settings()?;
        let state = PersistedBackendState {
            sessions,
            workspace_records: persistence.load_workspaces()?.unwrap_or_default(),
            journal: persistence
                .load_feed_header()?
                .map(|feed| EventJournal {
                    next_sequence: feed.next_sequence,
                    events: Vec::new(),
                    pending_events: Vec::new(),
                    workspace_events: Vec::new(),
                    pending_workspace_events: Vec::new(),
                    retention_limit: feed.retention_limit,
                })
                .unwrap_or_default(),
            session_policies: session_settings.approval_policies,
            auto_approve_actions: session_settings.auto_approve_actions,
            provider_configs: persistence.load_provider_configs()?,
            provider_health: persistence.load_provider_health()?,
            workspace_configs: persistence.load_workspace_configs()?,
            provider_usage: persistence.load_provider_usage()?,
            idempotency: persistence
                .load_idempotency_records()?
                .into_iter()
                .map(|(id, record)| {
                    Ok((
                        id,
                        IdempotencyRecord {
                            created_at: record.created_at,
                            expires_at: record.expires_at,
                            request: from_json(record.request)?,
                            response: from_json(record.response)?,
                        },
                    ))
                })
                .collect::<Result<BTreeMap<_, _>>>()?,
        };
        log::info!(
            "loaded persisted catalogs and feed cursors in {} ms",
            startup_started.elapsed().as_millis()
        );
        let sessions = SessionManager::from_state(state.sessions)?;
        {
            let mut target = self.sessions()?;
            *target = sessions;
        }
        {
            let mut target = self.journal()?;
            if state
                .journal
                .events
                .windows(2)
                .any(|events| events[0].sequence >= events[1].sequence)
                || state
                    .journal
                    .events
                    .last()
                    .is_some_and(|event| event.sequence != state.journal.next_sequence)
            {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted event journal sequences are invalid",
                    false,
                ));
            }
            *target = state.journal;
        }
        self.idempotency_store.replace_records(state.idempotency)?;
        {
            let mut target = self.session_policies()?;
            *target = state.session_policies;
        }
        {
            let mut target = self.auto_approve_actions()?;
            *target = state.auto_approve_actions;
        }
        let mut provider_configs = state.provider_configs;
        if self
            .providers
            .migrate_api_key_credentials(&mut provider_configs)?
        {
            needs_persist = true;
        }
        self.providers.restore_configs(provider_configs)?;
        self.providers.restore_health(state.provider_health)?;
        self.providers.restore_usage(state.provider_usage)?;
        *self.workspace_configs()? = state.workspace_configs;

        *self.workspace_records()? =
            loom_session::WorkspaceManager::from_state(state.workspace_records)?;

        for session_id in persistence.list_filesystem_sessions()? {
            self.sessions()?.get(session_id)?;
            self.persisted_session_filesystems()?.insert(session_id);
        }

        let active_run_summaries = persistence.load_active_run_summaries()?;
        let active_run_count = active_run_summaries.len();
        let mut lazy_run_count = 0;
        let mut recovery_updates = BTreeMap::new();
        let mut run_summaries = BTreeMap::new();
        let mut restored_runs = BTreeMap::new();
        for (run_id, mut summary) in active_run_summaries {
            let mut snapshot = summary.snapshot.clone();
            let run_state = snapshot.state;
            let usage = summary.usage.clone();
            self.sessions()?.get(snapshot.session_id)?;
            run_summaries.insert(
                run_id,
                PersistedRunSummary {
                    snapshot: snapshot.clone(),
                    usage: usage.clone(),
                },
            );
            let mut execution_state = persistence.load_run_execution_state(run_id)?;
            let safely_deferred = run_can_be_deferred_during_restore(
                run_state,
                execution_state
                    .as_ref()
                    .map(|state| state.pending_tool_execution.is_some()),
                execution_state
                    .as_ref()
                    .map(|state| state.pending_project_join.is_some()),
            );
            if safely_deferred {
                if matches!(
                    run_state,
                    AgentRunState::Planning | AgentRunState::Executing | AgentRunState::Evaluating
                ) {
                    let updated_at = Timestamp::now();
                    snapshot.state = AgentRunState::Paused;
                    snapshot.updated_at = updated_at;
                    snapshot.completed_at = None;
                    let execution = execution_state.as_mut().ok_or_else(|| {
                        LoomError::new(
                            ErrorCode::RecoveryRequired,
                            format!("persisted run {run_id} has no typed execution state"),
                            true,
                        )
                    })?;
                    execution.state = AgentRunState::Paused;
                    let mut attempts = persistence.load_run_attempts(run_id)?;
                    let attempt = attempts
                        .iter_mut()
                        .find(|attempt| attempt.id == snapshot.attempt_id)
                        .ok_or_else(|| {
                            LoomError::new(
                                ErrorCode::RecoveryRequired,
                                format!("persisted run {run_id} has no current attempt"),
                                true,
                            )
                        })?;
                    attempt.state = AgentRunState::Paused;
                    attempt.completed_at = None;
                    summary.snapshot = snapshot.clone();
                    summary.attempts = Some(attempts);
                    summary.execution_state = execution_state;
                    summary.interactions = None;
                    recovery_updates.insert(run_id, summary);
                    run_summaries.insert(
                        run_id,
                        PersistedRunSummary {
                            snapshot: snapshot.clone(),
                            usage: usage.clone(),
                        },
                    );
                    self.append_recovery_events(
                        snapshot.session_id,
                        vec![AgentEvent::RunStateChanged {
                            run_id,
                            state: AgentRunState::Paused,
                        }],
                    )?;
                }
                lazy_run_count += 1;
                continue;
            }
            let runtime_config = persistence
                .load_run_runtime_config(run_id)?
                .ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::RecoveryRequired,
                        format!("persisted run {run_id} has no runtime configuration"),
                        true,
                    )
                })?;
            let mut runtime_state = runtime_state_from_durable_config(
                run_summaries.get(&run_id).ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::RecoveryRequired,
                        format!("persisted run {run_id} summary is unavailable"),
                        true,
                    )
                })?,
                runtime_config,
            )?;
            let execution_state = execution_state.take().ok_or_else(|| {
                LoomError::new(
                    ErrorCode::RecoveryRequired,
                    format!("persisted run {run_id} has no typed execution state"),
                    true,
                )
            })?;
            hydrate_runtime_execution_state(&mut runtime_state, execution_state)?;
            runtime_state.plan = persistence.load_run_plan(run_id)?;
            let durable_messages = persistence.load_run_messages(run_id)?;
            runtime_state.message_timeline_ordinals = durable_messages
                .iter()
                .map(|message| message.timeline_ordinal)
                .collect();
            runtime_state.messages = persisted_run_messages(durable_messages);
            hydrate_run_context_checkpoint(&persistence, run_id, &mut runtime_state)?;
            runtime_state.activities = persistence.load_run_activities(run_id)?;
            runtime_state.attempts = persistence.load_run_attempts(run_id)?;
            if runtime_state.attempts.is_empty() {
                return Err(LoomError::new(
                    ErrorCode::RecoveryRequired,
                    format!("persisted run {run_id} has no typed attempt history"),
                    true,
                ));
            }
            runtime_state.interactions = persistence.load_run_interactions(run_id)?;
            if runtime_state.run.id != run_id {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted run key does not match runtime state {run_id}"),
                    false,
                ));
            }
            let session = self.sessions()?.get(runtime_state.session_id)?;
            let workspace = self.restore_session_filesystem(session.id)?;
            let mut recovery_reason = None;
            let provider =
                match self.provider_at(&runtime_state.task.model, runtime_state.provider_cursor) {
                    Ok(provider) => provider,
                    Err(error) => {
                        let descriptor = self
                            .providers
                            .describe_model(&runtime_state.task.model)
                            .unwrap_or_else(|_| ModelDescriptor {
                                id: runtime_state.task.model.clone(),
                                provider: ProviderId::new("recovered"),
                                display_name: "Unavailable persisted model".to_owned(),
                                context_window: None,
                                max_input_tokens: None,
                                max_output_tokens: None,
                                capabilities: ModelCapabilities::default(),
                            });
                        recovery_reason = Some(error.message.clone());
                        Box::new(UnavailableProvider::new(descriptor, error))
                    }
                };
            let tools = ToolExecutor::new_with_workspace(workspace)
                .with_github_token(self.providers.github_account_token().ok());
            let tools = self.with_project_agent_tools(
                tools,
                runtime_state.session_id,
                runtime_state.task.model.clone(),
                ProjectAgentToolGrants {
                    delegation: runtime_state.options.project_delegation_enabled,
                    messaging: runtime_state.options.project_messaging_enabled,
                    branch_messaging: runtime_state.options.project_branch_messaging_enabled,
                    inspection: runtime_state.options.project_inspection_enabled,
                    child_control: runtime_state.options.project_child_control_enabled,
                    worktree: runtime_state.options.project_worktree_enabled,
                    review: runtime_state.options.project_review_enabled,
                    integration: runtime_state.options.project_integration_enabled,
                },
            )?;
            let mut runtime = AgentRuntime::from_state(runtime_state, provider, tools)?;
            if runtime.session_id() != session.id {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted run {run_id} references a different session"),
                    false,
                ));
            }
            let mut recovery_events = runtime.recover_after_restart()?;
            if let Some(reason) = recovery_reason {
                recovery_events.push(AgentEvent::RecoveryRequired { run_id, reason });
            }
            let handle = self.register_runtime(runtime);
            if !recovery_events.is_empty()
                && let Some(summary) = run_summaries.get_mut(&run_id)
            {
                summary.snapshot = handle.state().run;
            }
            restored_runs.insert(run_id, handle);
            if !recovery_events.is_empty() {
                self.append_recovery_events(session.id, recovery_events)?;
                needs_persist = true;
            }
        }
        let restored_run_count = restored_runs.len();
        *self.persisted_runs()? = run_summaries;
        *self.runs()? = restored_runs;
        if needs_persist {
            self.persist_state_with_recovery_updates(&recovery_updates)?;
        } else if !recovery_updates.is_empty() {
            let mut journal = self.journal()?;
            let feed = DurableFeedState {
                next_sequence: journal.next_sequence,
                retention_limit: journal.retention_limit,
                events: journal.pending_events.clone(),
                workspace_events: journal.pending_workspace_events.clone(),
            };
            persistence.save_recovery_updates(&recovery_updates, &feed)?;
            journal.pending_events.clear();
        }
        // Finish durable cascade intents before the scheduler can reconcile or
        // admit any queued project work after restart.
        self.connect()
            .recover_pending_project_cancellation_cascades()?;
        self.reconcile_project_tasks_and_resume_queued(true)?;
        let lazy_filesystem_count = self.persisted_session_filesystems()?.len();
        log::info!(
            "indexed {} resumable runs, restored {} runtimes, deferred {} runtimes, and left {} filesystem services lazy in {} ms total",
            active_run_count,
            restored_run_count,
            lazy_run_count,
            lazy_filesystem_count,
            startup_started.elapsed().as_millis()
        );
        Ok(())
    }

    pub(crate) fn reconcile_project_tasks_and_resume_queued(
        self: &Arc<Self>,
        reconcile_persisted_runs: bool,
    ) -> Result<()> {
        let Some(persistence) = self.persistence.as_ref() else {
            return Ok(());
        };
        let sessions = self.sessions()?.list_in_workspace(None, true);
        let session_ids = sessions
            .iter()
            .map(|session| session.id)
            .collect::<Vec<_>>();
        let workspace_ids = sessions
            .iter()
            .map(|session| session.workspace_id)
            .collect::<BTreeSet<_>>();
        let mut project_ids = BTreeSet::new();
        for session_id in session_ids {
            let project_id = ProjectId::from_uuid(*session_id.as_uuid());
            if persistence.load_project_snapshot(project_id)?.is_some() {
                project_ids.insert(project_id);
            }
        }
        let connection = self.connect();
        for project_id in project_ids {
            let mut project_tasks = persistence.list_project_tasks(project_id)?;
            for task in &mut project_tasks {
                if reconcile_persisted_runs {
                    let latest_run =
                        persistence.load_latest_run_summary_for_session(task.target_session_id)?;
                    let recovered_status = latest_run
                        .as_ref()
                        .map(|summary| delegated_task_status_for_run_state(summary.snapshot.state))
                        .or_else(|| {
                            (task.status == loom_core::DelegatedTaskStatus::Running)
                                .then_some(loom_core::DelegatedTaskStatus::Queued)
                        });
                    if let Some(status) = recovered_status
                        && status != task.status
                        && persistence.update_delegated_task_status(
                            task.task_id,
                            status,
                            Timestamp::now(),
                        )?
                    {
                        let Some(updated_task) = persistence.load_delegated_task(task.task_id)?
                        else {
                            return Err(LoomError::not_found("delegated task", task.task_id));
                        };
                        *task = updated_task;
                        let sequence = self.journal()?.next();
                        self.journal()?.append_event(ServerEventEnvelope {
                            protocol_version: CURRENT_PROTOCOL_VERSION,
                            sequence,
                            session_id: task.requester_session_id,
                            event: ServerEvent::ProjectTaskUpdated { task: task.clone() },
                        });
                    }
                }
            }

            let task_statuses = project_tasks
                .iter()
                .map(|task| (task.task_id, task.status))
                .collect::<Vec<_>>();
            for task in &mut project_tasks {
                let failed_dependency = task.dependencies.iter().any(|dependency| {
                    task_statuses.iter().any(|(task_id, status)| {
                        task_id == dependency
                            && matches!(
                                status,
                                loom_core::DelegatedTaskStatus::Failed
                                    | loom_core::DelegatedTaskStatus::Cancelled
                            )
                    })
                });
                if task.status == loom_core::DelegatedTaskStatus::Queued
                    && failed_dependency
                    && persistence.update_delegated_task_status_if_queued(
                        task.task_id,
                        loom_core::DelegatedTaskStatus::Blocked,
                        Timestamp::now(),
                    )?
                {
                    *task = persistence
                        .load_delegated_task(task.task_id)?
                        .ok_or_else(|| LoomError::not_found("delegated task", task.task_id))?;
                    let sequence = self.journal()?.next();
                    self.journal()?.append_event(ServerEventEnvelope {
                        protocol_version: CURRENT_PROTOCOL_VERSION,
                        sequence,
                        session_id: task.requester_session_id,
                        event: ServerEvent::ProjectTaskUpdated { task: task.clone() },
                    });
                }
            }
        }
        for workspace_id in workspace_ids {
            connection
                .drain_workspace_project_admissions(workspace_id, reconcile_persisted_runs)?;
        }
        Ok(())
    }

    pub(crate) fn persist_state(&self) -> Result<()> {
        self.persist_state_with_recovery_updates(&BTreeMap::new())
    }

    pub(crate) fn persist_state_with_idempotency_candidate(
        &self,
        candidate: (loom_core::RequestId, IdempotencyRecord),
    ) -> Result<()> {
        self.latch_on_persistence_error(self.persist_state_inner(&BTreeMap::new(), Some(candidate)))
    }

    /// Persists the current worker checkpoint without enumerating unrelated runs,
    /// catalogs, or session filesystems. The journal lock is retained through the
    /// transaction so only the captured event prefix can be acknowledged.
    pub(crate) fn persist_run_checkpoint(&self, handle: &RunHandle) -> Result<()> {
        self.ensure_persistence_healthy()?;
        let result = self.persist_run_checkpoint_inner(handle);
        self.latch_on_persistence_error(result)
    }

    pub(crate) fn persist_worker_state(&self) -> Result<()> {
        self.ensure_persistence_healthy()?;
        self.persist_state()
    }

    pub(crate) fn ensure_persistence_healthy(&self) -> Result<()> {
        if self.persistence_failed.load(Ordering::SeqCst) {
            Err(LoomError::new(
                ErrorCode::Persistence,
                "backend is unavailable after a durable state save failure; reopen it to recover",
                true,
            ))
        } else {
            Ok(())
        }
    }

    pub(crate) fn persist_run_checkpoint_inner(&self, handle: &RunHandle) -> Result<()> {
        let Some(persistence) = &self.persistence else {
            return Ok(());
        };
        let _event_guard = handle
            .event_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        handle.flush_message_fragments(persistence)?;
        // Project only the checkpoint fields while holding the state lock.
        // This intentionally excludes the potentially large activities vector;
        // changed rows come from the ID-keyed queue below.
        let (
            session_id,
            summary,
            runtime_config,
            context_checkpoint,
            plan,
            message_delta,
            activities,
            project_manager_wait,
        ) = {
            let state = handle.locked_state();
            let summary = DurableRunSummary {
                snapshot: state.run.clone(),
                usage: state.usage.clone(),
                attempts: Some(state.attempts.clone()),
                execution_state: Some(execution_state_from_runtime(&state)?),
                interactions: Some(state.interactions.clone()),
            };
            let mut context_inspection = state.context_inspection.clone();
            if let Some(inspection) = &mut context_inspection {
                inspection.summary = None;
            }
            let runtime_config = DurableRunRuntimeConfig {
                system_instructions: state.task.system_instructions.clone(),
                repository_instructions: state.task.repository_instructions.clone(),
                approval_policy: state.approval_policy.clone(),
                limits: state.options.limits.clone(),
                context_options: state.options.context.clone(),
                checkpoint_id: state.options.checkpoint_id,
                input_cost_micros_per_1k: state.options.input_cost_micros_per_1k,
                output_cost_micros_per_1k: state.options.output_cost_micros_per_1k,
                context_inspection,
                project_delegation_enabled: state.options.project_delegation_enabled,
                project_messaging_enabled: state.options.project_messaging_enabled,
                project_inspection_enabled: state.options.project_inspection_enabled,
                project_child_control_enabled: state.options.project_child_control_enabled,
                project_worktree_enabled: state.options.project_worktree_enabled,
                project_review_enabled: state.options.project_review_enabled,
                project_integration_enabled: state.options.project_integration_enabled,
                project_branch_messaging_enabled: state.options.project_branch_messaging_enabled,
            };
            let context_checkpoint =
                state
                    .context_checkpoint
                    .clone()
                    .map(|summary| DurableRunContextCheckpoint {
                        session_id: state.session_id,
                        summary,
                    });
            let activities = handle
                .activity_deltas
                .lock()
                .map_err(|_| {
                    LoomError::new(
                        ErrorCode::Internal,
                        "activity delta lock was poisoned",
                        true,
                    )
                })?
                .ordered_values();
            let cursor = handle.message_checkpoint.lock().map_err(|_| {
                LoomError::new(
                    ErrorCode::Internal,
                    "message checkpoint cursor lock was poisoned",
                    true,
                )
            })?;
            let reset_messages = cursor.attempt_id != state.run.attempt_id
                || state.messages.len() < cursor.message_count;
            let start_ordinal = if reset_messages {
                0
            } else {
                cursor.message_count.saturating_sub(1)
            };
            let start_index = start_ordinal.min(state.messages.len());
            let message_delta = DurableRunMessageDelta {
                start_ordinal: start_ordinal as u64,
                reset: reset_messages,
                messages: durable_run_messages_from_runtime(
                    &state.messages[start_index..],
                    &state.message_timeline_ordinals[start_index..],
                )?,
            };
            let project_manager_wait = state
                .pending_project_join
                .as_ref()
                .map(|continuation| {
                    let arguments = serde_json::from_value::<WaitForProjectChildrenArguments>(
                        continuation.call.arguments.clone(),
                    )
                    .map_err(|error| {
                        LoomError::new(
                            ErrorCode::MalformedPayload,
                            format!("parked project join has invalid child task IDs: {error}"),
                            false,
                        )
                    })?;
                    let timestamp = Timestamp::now();
                    Ok(loom_core::ProjectManagerWaitRecord {
                        wait_id: loom_core::ProjectManagerWaitId::from_uuid(
                            *continuation.call.id.as_uuid(),
                        ),
                        run_id: state.run.id,
                        attempt_id: state.run.attempt_id,
                        tool_call_id: continuation.call.id,
                        manager_session_id: state.session_id,
                        child_task_ids: arguments.task_ids,
                        status: loom_core::ProjectManagerWaitStatus::Waiting,
                        result_summary: None,
                        created_at: timestamp,
                        updated_at: timestamp,
                    })
                })
                .transpose()?;
            (
                state.session_id,
                summary,
                runtime_config,
                context_checkpoint,
                state.plan.clone(),
                message_delta,
                activities,
                project_manager_wait,
            )
        };

        let (filesystem_record, filesystem_ack) = {
            let filesystem = self.session_filesystems()?.get(&session_id).cloned();
            if let Some(filesystem) = filesystem {
                if let Some(versioned) = filesystem.export_delta_if_dirty()? {
                    let workspace_delta = versioned.delta;
                    let filesystem_state = versioned.state;
                    let checkpoints = workspace_delta.checkpoints;
                    let edits = workspace_delta
                        .edits
                        .into_iter()
                        .map(|edit| DurableFilesystemEdit {
                            id: edit.id,
                            path: edit.path,
                            before: edit.before,
                            before_bytes: edit.before_bytes,
                            after_revision: edit.after_revision,
                            source: edit.source,
                        })
                        .collect();
                    let changes = workspace_delta.changes;
                    let deleted_checkpoints = workspace_delta.deleted_checkpoints;
                    let deleted_edits = workspace_delta.deleted_edits;
                    let deleted_changes = workspace_delta.deleted_changes;
                    let directories = filesystem
                        .mounted_directories()?
                        .into_iter()
                        .map(|(path, source)| SessionDirectory {
                            path,
                            source: source.display().to_string(),
                        })
                        .collect::<Vec<_>>();
                    let repositories = self
                        .session_repositories()?
                        .get(&session_id)
                        .cloned()
                        .unwrap_or_default();
                    let persisted = PersistedSessionFilesystem {
                        filesystem: filesystem_state,
                        repositories: repositories.clone(),
                        directories: directories.clone(),
                    };
                    (
                        Some(DurableFilesystemRecord {
                            session_id,
                            root: persisted.filesystem.root.clone(),
                            control: persisted.filesystem.control,
                            checkpoints,
                            edits,
                            changes,
                            repositories,
                            directories,
                            payload: json_value(persisted)?,
                            delta: Some(DurableFilesystemDelta {
                                deleted_checkpoints,
                                deleted_edits,
                                deleted_changes,
                            }),
                        }),
                        Some((filesystem, versioned.generation)),
                    )
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            }
        };

        let mut journal = self.journal()?;
        let (feed, captured_event_sequences) = journal.capture_session_feed(session_id);
        // Full retention ranking scans the retained feed, so amortize it until
        // at least 64 new global event sequences or 4 MiB of pending payloads
        // have arrived. The byte threshold prevents large event bodies from
        // overshooting the total feed retention budget between pruning passes.
        let sequence = feed.next_sequence.value();
        let last_pruned = self.last_feed_pruned_sequence.load(Ordering::Relaxed);
        let pending_feed_bytes = serde_json::to_vec(&(&feed.events, &feed.workspace_events))
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::Persistence,
                    format!("could not size pending event feed: {error}"),
                    false,
                )
            })?
            .len();
        let prior_feed_bytes = self.feed_bytes_since_prune.load(Ordering::Relaxed);
        let accumulated_feed_bytes = prior_feed_bytes.saturating_add(pending_feed_bytes);
        let prune_feed =
            should_prune_worker_feed(sequence, last_pruned, prior_feed_bytes, pending_feed_bytes);
        let session = self.sessions()?.get(session_id)?;
        let session_next_sequence = self.sessions()?.next_sequence();
        let checkpoint = DurableRunCheckpointWrite {
            session: &session,
            session_next_sequence,
            prune_feed,
            summary: &summary,
            runtime_config: &runtime_config,
            context_checkpoint: context_checkpoint.as_ref(),
            plan: &plan,
            messages: &[],
            message_delta: Some(&message_delta),
            activities: &[],
            activity_deltas: Some(&activities),
            filesystem: filesystem_record.as_ref(),
            feed: &feed,
        };
        let checkpoint_result = match project_manager_wait.as_ref() {
            Some(wait) => {
                persistence.save_run_checkpoint_with_project_manager_wait(checkpoint, wait)
            }
            None => persistence.save_run_checkpoint(checkpoint),
        };
        if let Err(error) = checkpoint_result {
            self.persistence_failed.store(true, Ordering::SeqCst);
            return Err(error);
        }
        if let Some((filesystem, generation)) = filesystem_ack {
            filesystem.acknowledge_persisted_generation(generation)?;
        }
        if prune_feed {
            self.last_feed_pruned_sequence
                .store(sequence, Ordering::Relaxed);
            self.feed_bytes_since_prune.store(0, Ordering::Relaxed);
        } else {
            self.feed_bytes_since_prune
                .store(accumulated_feed_bytes, Ordering::Relaxed);
        }
        {
            let mut cursor = handle
                .message_checkpoint
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            cursor.attempt_id = summary.snapshot.attempt_id;
            cursor.message_count = usize::try_from(message_delta.start_ordinal)
                .unwrap_or(usize::MAX)
                .saturating_add(message_delta.messages.len());
        }
        if !activities.is_empty() {
            let mut pending = handle
                .activity_deltas
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for activity in &activities {
                if pending.by_id.get(&activity.id) == Some(activity) {
                    pending.by_id.remove(&activity.id);
                }
            }
            let still_pending = pending.by_id.keys().copied().collect::<BTreeSet<_>>();
            pending
                .appended_order
                .retain(|activity_id| still_pending.contains(activity_id));
        }
        journal.acknowledge_session_feed(&captured_event_sequences);
        Ok(())
    }

    pub(crate) fn persist_state_with_recovery_updates(
        &self,
        recovery_updates: &BTreeMap<loom_core::RunId, DurableRunSummary>,
    ) -> Result<()> {
        self.latch_on_persistence_error(self.persist_state_inner(recovery_updates, None))
    }

    pub(crate) fn latch_on_persistence_error<T>(&self, result: Result<T>) -> Result<T> {
        if result.is_err() {
            self.persistence_failed.store(true, Ordering::SeqCst);
        }
        result
    }

    pub(crate) fn persist_state_inner(
        &self,
        recovery_updates: &BTreeMap<loom_core::RunId, DurableRunSummary>,
        idempotency_candidate: Option<(loom_core::RequestId, IdempotencyRecord)>,
    ) -> Result<()> {
        let Some(persistence) = &self.persistence else {
            return Ok(());
        };
        let _state_persist_guard = self.state_persist_gate.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "state persistence lock was poisoned",
                true,
            )
        })?;
        let handles = self.runs()?.values().cloned().collect::<Vec<_>>();
        for handle in handles {
            handle.flush_message_fragments(persistence)?;
        }
        let mut runs: BTreeMap<loom_core::RunId, AgentRuntimeState> = self
            .runs()?
            .iter()
            .map(|(run_id, handle)| (*run_id, handle.state()))
            .collect();
        let durable_run_plans = runs
            .iter()
            .map(|(run_id, state)| (*run_id, state.plan.clone()))
            .collect::<BTreeMap<_, _>>();
        let mut durable_run_summaries: BTreeMap<loom_core::RunId, DurableRunSummary> = self
            .persisted_runs()?
            .iter()
            .map(|(run_id, summary)| {
                (
                    *run_id,
                    DurableRunSummary {
                        snapshot: summary.snapshot.clone(),
                        usage: summary.usage.clone(),
                        attempts: None,
                        execution_state: None,
                        interactions: None,
                    },
                )
            })
            .collect();
        for (run_id, state) in &runs {
            durable_run_summaries.insert(
                *run_id,
                DurableRunSummary {
                    snapshot: state.run.clone(),
                    usage: state.usage.clone(),
                    attempts: Some(state.attempts.clone()),
                    execution_state: Some(execution_state_from_runtime(state)?),
                    interactions: Some(state.interactions.clone()),
                },
            );
        }
        durable_run_summaries.extend(
            recovery_updates
                .iter()
                .map(|(run_id, summary)| (*run_id, summary.clone())),
        );
        let durable_run_context_checkpoints =
            runs.iter()
                .map(|(run_id, state)| {
                    (
                        *run_id,
                        state.context_checkpoint.clone().map(|summary| {
                            DurableRunContextCheckpoint {
                                session_id: state.session_id,
                                summary,
                            }
                        }),
                    )
                })
                .collect::<BTreeMap<_, _>>();
        let durable_run_runtime_configs = runs
            .iter()
            .map(|(run_id, state)| {
                let mut context_inspection = state.context_inspection.clone();
                if let Some(inspection) = &mut context_inspection {
                    inspection.summary = None;
                }
                Ok((
                    *run_id,
                    DurableRunRuntimeConfig {
                        system_instructions: state.task.system_instructions.clone(),
                        repository_instructions: state.task.repository_instructions.clone(),
                        approval_policy: state.approval_policy.clone(),
                        limits: state.options.limits.clone(),
                        context_options: state.options.context.clone(),
                        checkpoint_id: state.options.checkpoint_id,
                        input_cost_micros_per_1k: state.options.input_cost_micros_per_1k,
                        output_cost_micros_per_1k: state.options.output_cost_micros_per_1k,
                        context_inspection,
                        project_delegation_enabled: state.options.project_delegation_enabled,
                        project_messaging_enabled: state.options.project_messaging_enabled,
                        project_inspection_enabled: state.options.project_inspection_enabled,
                        project_child_control_enabled: state.options.project_child_control_enabled,
                        project_worktree_enabled: state.options.project_worktree_enabled,
                        project_review_enabled: state.options.project_review_enabled,
                        project_integration_enabled: state.options.project_integration_enabled,
                        project_branch_messaging_enabled: state
                            .options
                            .project_branch_messaging_enabled,
                    },
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let mut durable_run_messages = BTreeMap::new();
        let mut durable_run_activities = BTreeMap::new();
        for (run_id, state) in &mut runs {
            durable_run_messages.insert(
                *run_id,
                durable_run_messages_from_runtime(
                    &state.messages,
                    &state.message_timeline_ordinals,
                )?,
            );
            durable_run_activities.insert(*run_id, std::mem::take(&mut state.activities));
        }
        let loaded_repositories = self.session_repositories()?.clone();
        let mut filesystem_records = Vec::new();
        let mut filesystem_generations = Vec::new();
        for (session_id, filesystem) in self.session_filesystems()?.iter() {
            let Some(versioned) = filesystem.export_delta_if_dirty()? else {
                continue;
            };
            let workspace_delta = versioned.delta;
            let filesystem_state = versioned.state;
            let checkpoints = workspace_delta.checkpoints;
            let edits = workspace_delta
                .edits
                .into_iter()
                .map(|edit| DurableFilesystemEdit {
                    id: edit.id,
                    path: edit.path,
                    before: edit.before,
                    before_bytes: edit.before_bytes,
                    after_revision: edit.after_revision,
                    source: edit.source,
                })
                .collect();
            let changes = workspace_delta.changes;
            let deleted_checkpoints = workspace_delta.deleted_checkpoints;
            let deleted_edits = workspace_delta.deleted_edits;
            let deleted_changes = workspace_delta.deleted_changes;
            let directories = filesystem
                .mounted_directories()?
                .into_iter()
                .map(|(path, source)| SessionDirectory {
                    path,
                    source: source.display().to_string(),
                })
                .collect::<Vec<_>>();
            let repositories = loaded_repositories
                .get(session_id)
                .cloned()
                .unwrap_or_default();
            let persisted = PersistedSessionFilesystem {
                filesystem: filesystem_state,
                repositories: repositories.clone(),
                directories: directories.clone(),
            };
            filesystem_records.push(DurableFilesystemRecord {
                session_id: *session_id,
                root: persisted.filesystem.root.clone(),
                control: persisted.filesystem.control,
                checkpoints,
                edits,
                changes,
                repositories,
                directories,
                payload: json_value(persisted)?,
                delta: Some(DurableFilesystemDelta {
                    deleted_checkpoints,
                    deleted_edits,
                    deleted_changes,
                }),
            });
            filesystem_generations.push((filesystem.clone(), versioned.generation));
        }
        let sessions = self.sessions()?.export_state();
        let mut journal = self.journal()?;
        let feed = DurableFeedState {
            next_sequence: journal.next_sequence,
            retention_limit: journal.retention_limit,
            events: journal.pending_events.clone(),
            workspace_events: journal.pending_workspace_events.clone(),
        };
        let session_settings = DurableSessionSettings {
            approval_policies: self.session_policies()?.clone(),
            auto_approve_actions: self.auto_approve_actions()?.clone(),
        };
        let workspace_configs = self.workspace_configs()?.clone();
        let provider_state = DurableProviderState {
            configs: self
                .providers
                .export_configs()?
                .into_iter()
                .map(|config| (config.id.clone(), config))
                .collect(),
            health: self.providers.export_health()?,
        };
        let workspace_records = self.workspace_records()?.export_state();
        let provider_usage = self.providers.usage()?;
        let idempotency = self
            .idempotency_store
            .durable_records(idempotency_candidate.as_ref())?;
        #[cfg(test)]
        if self.fail_next_state_save.swap(false, Ordering::SeqCst) {
            self.persistence_failed.store(true, Ordering::SeqCst);
            return Err(LoomError::new(
                ErrorCode::Internal,
                "injected durable state save failure",
                true,
            ));
        }
        let result = persistence.save_state(DurableStateWrite {
            sessions: &sessions,
            workspaces: Some(&workspace_records),
            settings: Some(&session_settings),
            workspace_configs: Some(&workspace_configs),
            providers: Some(&provider_state),
            usage: Some(&provider_usage),
            idempotency: Some(&idempotency),
            run_summaries: Some(&durable_run_summaries),
            run_runtime_configs: Some(&durable_run_runtime_configs),
            run_context_checkpoints: Some(&durable_run_context_checkpoints),
            run_plans: Some(&durable_run_plans),
            run_messages: Some(&durable_run_messages),
            run_activities: Some(&durable_run_activities),
            filesystem_records: Some(&filesystem_records),
            feed: Some(&feed),
        });
        if result.is_err() {
            self.persistence_failed.store(true, Ordering::SeqCst);
        }
        if result.is_ok() {
            for (filesystem, generation) in filesystem_generations {
                filesystem.acknowledge_persisted_generation(generation)?;
            }
            journal.pending_events.clear();
            journal.pending_workspace_events.clear();
            self.last_feed_pruned_sequence
                .store(feed.next_sequence.value(), Ordering::Relaxed);
            self.feed_bytes_since_prune.store(0, Ordering::Relaxed);
        }
        result
    }

    pub fn flush(&self) -> Result<()> {
        if *self.request_lifecycle.read().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "backend request lifecycle lock was poisoned",
                true,
            )
        })? != 0
        {
            return Err(LoomError::conflict("backend is shutting down"));
        }
        self.persist_state()
    }

    /// Stops active run workers, persists their paused continuation state,
    /// joins all worker threads, and releases exclusive database ownership.
    /// Requests through existing connections are rejected after shutdown.
    pub fn shutdown(&self) -> Result<()> {
        let mut shutting_down = self.request_lifecycle.write().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "backend request lifecycle lock was poisoned",
                true,
            )
        })?;
        if *shutting_down == 2 {
            return Ok(());
        }
        *shutting_down = 1;
        let handles = self
            .runs()?
            .values()
            .cloned()
            .collect::<Vec<Arc<RunHandle>>>();
        for handle in handles {
            if handle.is_running() {
                handle.control.request_pause();
                handle.wait_until_idle()?;
                if let Some(error) = handle.take_failure() {
                    return Err(error);
                }
                if handle.control.is_stopping() {
                    let state = handle.state().run.state;
                    // Only active runs need to be parked. Awaiting approval and
                    // waiting for input are already durable, resumable stops and
                    // must not be downgraded to Paused by shutdown.
                    if matches!(
                        state,
                        AgentRunState::Planning
                            | AgentRunState::Executing
                            | AgentRunState::Evaluating
                    ) {
                        let mut runtime = handle.try_runtime()?;
                        runtime.pause()?;
                        handle.refresh(&runtime);
                    }
                    handle.control.clear_request();
                }
            }
            handle.join_worker()?;
        }
        // A fail-stopped backend must not write more state, but shutdown still
        // has to release database ownership so a restart can reopen it.
        let persist_result = if self.persistence_failed.load(Ordering::SeqCst) {
            Ok(())
        } else {
            self.persist_state()
        };
        if let Some(persistence) = self.persistence.as_ref() {
            persistence.release_exclusive_writer()?;
        }
        *shutting_down = 2;
        persist_result
    }
    pub(crate) fn append_recovery_events(
        self: &Arc<Self>,
        session_id: AgentSessionId,
        events: Vec<AgentEvent>,
    ) -> Result<()> {
        for event in events {
            let state = session_state_for_event(&event);
            self.journal()?.append_agent(session_id, event);
            if let Some(state) = state {
                let current = self.sessions()?.get(session_id)?.state;
                if current != state {
                    let (_, record) = self.sessions()?.transition(session_id, state)?;
                    self.journal()?.append_session(record);
                }
            }
        }
        Ok(())
    }

    /// Journals one agent event and keeps the session state in step with it.
    pub(crate) fn record_agent_event(
        self: &Arc<Self>,
        session_id: AgentSessionId,
        event: AgentEvent,
    ) -> Result<()> {
        let state = session_state_for_event(&event);
        self.journal()?.append_agent(session_id, event);
        if let Some(state) = state {
            let current = self.sessions()?.get(session_id)?.state;
            if current != state {
                let (_, record) = self.sessions()?.transition(session_id, state)?;
                self.journal()?.append_session(record);
            }
        }
        Ok(())
    }

    pub(crate) fn update_project_task_for_session_state(
        &self,
        session_id: AgentSessionId,
        state: AgentSessionState,
    ) -> Result<()> {
        let Some(next_status) = delegated_task_status_for_session_state(state) else {
            return Ok(());
        };
        let Some(persistence) = self.persistence.as_ref() else {
            return Ok(());
        };
        let Some(task) = persistence.load_delegated_task_for_target(session_id)? else {
            return Ok(());
        };
        if !persistence.update_delegated_task_status(task.task_id, next_status, Timestamp::now())? {
            return Ok(());
        }
        let task = persistence
            .load_delegated_task(task.task_id)?
            .ok_or_else(|| LoomError::not_found("delegated task", task.task_id))?;
        let sequence = self.journal()?.next();
        self.journal()?.append_event(ServerEventEnvelope {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            sequence,
            session_id: task.requester_session_id,
            event: ServerEvent::ProjectTaskUpdated { task },
        });
        Ok(())
    }

    pub(crate) fn after_run_checkpoint(self: &Arc<Self>, handle: &RunHandle) -> Result<()> {
        let state = handle.state().run.state;
        let session_id = handle.session_id;
        let session_state = session_state_for_run_state(state);
        if !project_agent_slot_released(session_state) {
            return Ok(());
        }
        self.update_project_task_for_session_state(session_id, session_state)?;
        self.reconcile_project_tasks_and_resume_queued(false)
    }

    /// Observer installed on every runtime so events are journaled as they are
    /// produced rather than after the run finishes.
    pub(crate) fn run_observer(
        self: &Arc<Self>,
        handle: Weak<RunHandle>,
        session_id: AgentSessionId,
    ) -> AgentEventObserver {
        let backend = Arc::downgrade(self);
        Arc::new(move |event: &AgentEvent| {
            let Some(backend) = backend.upgrade() else {
                return;
            };
            let handle = handle.upgrade();
            // Keep journal append and cached-state/dirty-activity application
            // indivisible relative to a durable worker checkpoint.
            let _event_guard = handle.as_ref().map(|handle| {
                handle
                    .event_gate
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
            });
            let fragment_result = match (event, backend.persistence.as_ref(), handle.as_ref()) {
                (
                    AgentEvent::AssistantMessageDelta { text, .. },
                    Some(persistence),
                    Some(handle),
                ) => handle.append_message_delta(persistence, text),
                (AgentEvent::RunCompleted { .. }, Some(persistence), Some(handle)) => {
                    handle.flush_message_fragments(persistence)
                }
                _ => Ok(()),
            };
            let recorded = fragment_result
                .and_then(|()| backend.record_agent_event(session_id, event.clone()));
            if let Some(handle) = handle.as_ref() {
                handle.apply_event(event);
                if let Err(error) = recorded {
                    handle.record_failure(error);
                }
            }
        })
    }

    /// Wraps a runtime in a handle and attaches the journaling observer.
    pub(crate) fn register_runtime(self: &Arc<Self>, mut runtime: AgentRuntime) -> Arc<RunHandle> {
        let session_id = runtime.session_id();
        Arc::new_cyclic(|weak: &Weak<RunHandle>| {
            runtime.set_event_observer(self.run_observer(weak.clone(), session_id));
            RunHandle::new(runtime)
        })
    }

    /// Drives a registered run on its own worker so the request handler returns
    /// as soon as the run is registered.
    pub(crate) fn spawn_run_worker(self: &Arc<Self>, handle: Arc<RunHandle>) -> Result<()> {
        handle.join_worker()?;
        handle.set_running(true);
        let backend = Arc::clone(self);
        let worker_handle = Arc::clone(&handle);
        let run_id = handle.run_id;
        let worker = thread::Builder::new()
            .name(format!("loom-run-{run_id}"))
            .spawn(move || {
                let fragment_flusher = backend.persistence.clone().map(|persistence| {
                    let handle = Arc::downgrade(&handle);
                    thread::spawn(move || {
                        RunHandle::flush_message_fragments_until_stopped(handle, persistence)
                    })
                });
                loop {
                    let delivered = if let Some(persistence) = backend.persistence.as_ref() {
                        let mut runtime = handle
                            .runtime
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner);
                        match deliver_project_agent_messages(persistence, &mut runtime) {
                            Ok(delivered) => {
                                if delivered {
                                    handle.refresh(&runtime);
                                }
                                delivered
                            }
                            Err(error) => {
                                handle.record_failure(error);
                                false
                            }
                        }
                    } else {
                        false
                    };
                    if handle.failure().is_some() {
                        break;
                    }
                    if delivered && let Err(error) = backend.persist_run_checkpoint(&handle) {
                        handle.record_failure(error);
                        break;
                    }
                    let progress = {
                        let mut runtime = handle
                            .runtime
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner);
                        let progress = runtime.run_step();
                        handle.refresh(&runtime);
                        progress
                    };
                    if handle.failure().is_some() {
                        break;
                    }
                    match progress {
                        Ok(progress) => {
                            if let Some(persistence) = backend.persistence.as_ref()
                                && let Err(error) = handle.flush_message_fragments(persistence)
                            {
                                handle.record_failure(error);
                                break;
                            }
                            if let Err(error) = backend.persist_run_checkpoint(&handle) {
                                handle.record_failure(error);
                                break;
                            }
                            if let Err(error) = backend.after_run_checkpoint(&handle) {
                                handle.record_failure(error);
                                break;
                            }
                            if !progress.continues {
                                break;
                            }
                        }
                        Err(error) => {
                            let flush_error =
                                backend.persistence.as_ref().and_then(|persistence| {
                                    handle.flush_message_fragments(persistence).err()
                                });
                            handle.record_failure(flush_error.unwrap_or(error));
                            break;
                        }
                    }
                }
                if let Err(error) = backend.persist_worker_state() {
                    handle.record_failure(error);
                }
                handle.set_running(false);
                if fragment_flusher.is_some_and(|flusher| flusher.join().is_err()) {
                    log::error!("run message fragment flusher thread panicked");
                }
            })
            .map_err(|error| {
                worker_handle.set_running(false);
                LoomError::new(
                    ErrorCode::Internal,
                    format!("could not start worker for run {run_id}: {error}"),
                    true,
                )
            })?;
        *worker_handle
            .worker
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(worker);
        Ok(())
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

    pub(crate) fn project_agent_tools(
        &self,
        session_id: AgentSessionId,
        model_id: ModelId,
        grants: ProjectAgentToolGrants,
    ) -> Result<Option<Arc<dyn ToolExtension>>> {
        let Some(persistence) = &self.persistence else {
            return Ok(None);
        };
        let Some(project) = persistence.load_project_snapshot_for_session(session_id)? else {
            return Ok(None);
        };
        if !project
            .agents
            .iter()
            .any(|agent| agent.session_id == session_id)
        {
            return Ok(None);
        }
        let can_delegate = grants.delegation
            && self
                .supported_capabilities
                .contains(Capability::CreateProjectChild);
        let can_delegate_code = can_delegate
            && grants.worktree
            && self
                .supported_capabilities
                .contains(Capability::CreateProjectWorktree);
        let can_message = grants.messaging
            && self
                .supported_capabilities
                .contains(Capability::SendProjectAgentMessage);
        let can_branch_message = grants.branch_messaging
            && self
                .supported_capabilities
                .contains(Capability::SendProjectBranchMessage);
        let can_inspect_children = grants.inspection
            && self
                .supported_capabilities
                .contains(Capability::ReadProject);
        let can_wait_children = grants.delegation
            && can_inspect_children
            && self
                .supported_capabilities
                .contains(Capability::CreateProjectChild);
        let can_control_children = grants.child_control
            && self
                .supported_capabilities
                .contains(Capability::ControlProjectChild);
        let can_review_children = grants.review
            && self
                .supported_capabilities
                .contains(Capability::ReadProjectChildReview);
        let can_integrate_children = grants.integration
            && self
                .supported_capabilities
                .contains(Capability::IntegrateProjectChild);
        let backend = self
            .self_reference
            .lock()
            .map_err(|_| {
                LoomError::new(
                    ErrorCode::Internal,
                    "backend self reference lock was poisoned",
                    true,
                )
            })?
            .clone();
        if backend.strong_count() == 0 {
            return Err(LoomError::new(
                ErrorCode::Internal,
                "project agent tools require a registered backend",
                true,
            ));
        }
        Ok(Some(Arc::new(ProjectAgentTools {
            backend,
            session_id,
            project_id: project.project_id,
            model_id,
            can_delegate,
            can_delegate_code,
            can_message,
            can_branch_message,
            can_inspect_children,
            can_wait_children,
            can_control_children,
            can_review_children,
            can_integrate_children,
        })))
    }

    pub(crate) fn with_project_agent_tools(
        &self,
        tools: ToolExecutor,
        session_id: AgentSessionId,
        model_id: ModelId,
        grants: ProjectAgentToolGrants,
    ) -> Result<ToolExecutor> {
        Ok(
            match self.project_agent_tools(session_id, model_id, grants)? {
                Some(extension) => tools.with_extension(extension),
                None => tools,
            },
        )
    }

    pub(crate) fn accept_project_agent_message(
        &self,
        request_id: RequestId,
        trusted_sender_session_id: AgentSessionId,
        sender_can_message: bool,
        sender_can_branch_message: bool,
        mut draft: loom_core::AgentMessageDraft,
    ) -> Result<ServerResponse> {
        if draft.body.trim().is_empty() || draft.body.len() > 16 * 1024 {
            return Err(LoomError::invalid_request(
                "agent message body must contain 1 to 16384 bytes",
            ));
        }
        let persistence = self.persistence.as_ref().ok_or_else(|| {
            LoomError::new(
                ErrorCode::UnsupportedCapability,
                "agent messaging requires durable storage",
                false,
            )
        })?;
        let project = persistence
            .load_project_snapshot(draft.project_id)?
            .ok_or_else(|| LoomError::not_found("project", draft.project_id))?;
        draft.sender_session_id = trusted_sender_session_id;
        let sender = project
            .agents
            .iter()
            .find(|agent| agent.session_id == trusted_sender_session_id)
            .ok_or_else(|| LoomError::invalid_request("message sender is not a project member"))?;
        let target = project
            .agents
            .iter()
            .find(|agent| agent.session_id == draft.target_session_id)
            .ok_or_else(|| LoomError::invalid_request("message target is not a project member"))?;
        let is_direct_route = sender.parent_session_id == Some(target.session_id)
            || target.parent_session_id == Some(sender.session_id);
        if let Some(task_id) = draft.task_id {
            let context_task = persistence
                .load_delegated_task(task_id)?
                .ok_or_else(|| LoomError::not_found("delegated task", task_id))?;
            if context_task.project_id != draft.project_id {
                return Err(LoomError::invalid_request(
                    "message task context must belong to the sender's project",
                ));
            }
        }
        let target_task = persistence.load_delegated_task_for_target(draft.target_session_id)?;
        let branch_route = !is_direct_route;
        if is_direct_route && !sender_can_message {
            return Err(LoomError::new(
                ErrorCode::AuthorizationDenied,
                "direct project messaging is not granted to this run",
                false,
            ));
        }
        if branch_route
            && (!sender_can_branch_message
                || !project_member_branch_messaging_enabled(
                    persistence,
                    project.root_session_id,
                    target.session_id,
                )?)
        {
            return Err(LoomError::new(
                ErrorCode::AuthorizationDenied,
                "branch messages require explicit sender and recipient grants",
                false,
            ));
        }
        if persistence
            .load_agent_message_by_request(request_id)?
            .is_some()
        {
            let message = persistence.accept_agent_message(request_id, &draft)?;
            return Ok(ServerResponse::ProjectAgentMessageAccepted(message));
        }
        if matches!(
            target.state,
            AgentSessionState::Completed
                | AgentSessionState::Failed
                | AgentSessionState::Cancelled
                | AgentSessionState::Archived
        ) {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "cannot message a terminal project agent that has no resume path",
                false,
            ));
        }
        if target_task.as_ref().is_some_and(|task| {
            matches!(
                task.status,
                loom_core::DelegatedTaskStatus::Completed
                    | loom_core::DelegatedTaskStatus::Failed
                    | loom_core::DelegatedTaskStatus::Cancelled
            )
        }) {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "cannot message a terminal delegated task that has no resume path",
                false,
            ));
        }
        let message = persistence.accept_agent_message(request_id, &draft)?;
        let sequence = self.journal()?.next();
        self.journal()?.append_event(ServerEventEnvelope {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            sequence,
            session_id: draft.target_session_id,
            event: ServerEvent::ProjectAgentMessageAccepted {
                message: message.clone(),
            },
        });
        if !branch_route && draft.target_session_id != project.root_session_id {
            let sequence = self.journal()?.next();
            self.journal()?.append_event(ServerEventEnvelope {
                protocol_version: CURRENT_PROTOCOL_VERSION,
                sequence,
                session_id: project.root_session_id,
                event: ServerEvent::ProjectAgentMessageAccepted {
                    message: message.clone(),
                },
            });
        }
        Ok(ServerResponse::ProjectAgentMessageAccepted(message))
    }

    pub fn event_retention(&self) -> Result<usize> {
        Ok(self.journal()?.retention_limit.max(1))
    }
}
