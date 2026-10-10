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
            Some(
                Arc::new(FilePersistence::open_exclusive_writer(path.into())?)
                    as Arc<dyn Persistence>,
            ),
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
            Some(
                Arc::new(FilePersistence::open_exclusive_writer(path.into())?)
                    as Arc<dyn Persistence>,
            ),
        )
    }

    pub(crate) fn with_provider_registry_persistent_credentials(
        providers: ProviderRegistry,
        path: Option<PathBuf>,
    ) -> Result<Arc<Self>> {
        Self::with_provider_registry_and_persistence(
            providers,
            path.map(|path| {
                FilePersistence::open_exclusive_writer(path)
                    .map(|store| Arc::new(store) as Arc<dyn Persistence>)
            })
            .transpose()?,
        )
    }

    pub(crate) fn with_provider_registry_and_persistence(
        providers: ProviderRegistry,
        persistence: Option<Arc<dyn Persistence>>,
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
        // Repository mirrors cached on this node live next to the session roots
        // so they survive process restarts and can seed later session clones.
        let clone_cache_base = session_root_base.with_extension("clone-cache");
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
                Capability::DeleteAgentSession,
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
            session_service: SessionService::new(),
            workspace_service: WorkspaceService::new(),
            run_service: RunService::new(),
            journal: Mutex::new(EventJournal::default()),
            last_feed_pruned_sequence: AtomicU64::new(0),
            feed_bytes_since_prune: AtomicUsize::new(0),
            session_filesystem_service: SessionFilesystemService::new(),
            repository_service: RepositoryService::new(clone_cache_base),
            process_service: ProcessService::new(),
            terminals: TerminalManager::new(),
            resource_monitor: Mutex::new(ResourceMonitor::default()),
            supported_capabilities: CapabilitySet::new(supported_capabilities),
            providers,
            credentials: CredentialService::new(),
            persistence,
            run_executor: RunExecutor::with_defaults(),
            session_root_base,
            archive_retention: Mutex::new(loom_protocol::ArchiveRetentionPolicy::disabled()),
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
}
