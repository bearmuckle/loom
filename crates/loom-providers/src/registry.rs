use super::*;

/// Non-secret credential-store entry recording whether the connected GitHub
/// account may be used for writes and pull requests.
const GITHUB_WRITE_ACCESS_CREDENTIAL_REF: &str = "github-write-access";

#[derive(Clone)]
pub struct ProviderRegistry {
    pub configurations: Arc<Mutex<BTreeMap<ProviderId, ProviderConfig>>>,
    pub credentials: Arc<dyn CredentialStore>,
    pub api_key_credentials: Arc<Mutex<Option<Arc<dyn CredentialStore>>>>,
    pub health: Arc<Mutex<BTreeMap<ProviderId, ProviderHealth>>>,
    pub usage: Arc<Mutex<UsageLedger>>,
}

impl fmt::Debug for ProviderRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderRegistry")
            .field(
                "provider_count",
                &self.list_providers().map(|items| items.len()).unwrap_or(0),
            )
            .finish()
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::demo()
    }
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::with_credentials(Arc::new(InMemoryCredentialStore::default()))
    }

    pub fn with_credentials(credentials: Arc<dyn CredentialStore>) -> Self {
        Self {
            configurations: Arc::new(Mutex::new(BTreeMap::new())),
            credentials,
            api_key_credentials: Arc::new(Mutex::new(None)),
            health: Arc::new(Mutex::new(BTreeMap::new())),
            usage: Arc::new(Mutex::new(UsageLedger::default())),
        }
    }

    /// Scope newly entered API keys to one backend installation while keeping
    /// legacy and provider-specific credentials in their existing store.
    pub fn scope_api_key_credentials(&self, credentials: Arc<dyn CredentialStore>) -> Result<()> {
        *self
            .api_key_credentials
            .lock()
            .map_err(|_| internal_lock_error("API-key credential store"))? = Some(credentials);
        Ok(())
    }

    pub fn resolve_credential(&self, reference: &CredentialRef) -> Result<String> {
        if reference.as_str().starts_with("api-key:")
            && let Some(credentials) = self
                .api_key_credentials
                .lock()
                .map_err(|_| internal_lock_error("API-key credential store"))?
                .as_ref()
        {
            return credentials.resolve(reference);
        }
        self.credentials.resolve(reference)
    }

    /// The credential store private to this backend installation: the scoped
    /// API-key store when one is configured, otherwise the main store.
    pub fn instance_credentials(&self) -> Result<Arc<dyn CredentialStore>> {
        Ok(self
            .api_key_credentials
            .lock()
            .map_err(|_| internal_lock_error("API-key credential store"))?
            .clone()
            .unwrap_or_else(|| Arc::clone(&self.credentials)))
    }

    /// Migrate resolvable legacy OpenAI-compatible credentials into this
    /// backend's scoped store. The legacy entry remains in place so another
    /// backend that has not been opened yet can migrate its own saved config.
    pub fn migrate_api_key_credentials(&self, configs: &mut [ProviderConfig]) -> Result<bool> {
        let Some(scoped_store) = self
            .api_key_credentials
            .lock()
            .map_err(|_| internal_lock_error("API-key credential store"))?
            .clone()
        else {
            return Ok(false);
        };
        let mut migrated = false;
        for config in configs {
            if !matches!(
                config.kind,
                ProviderKind::OpenAi | ProviderKind::OpenAiCompatible | ProviderKind::DeepSeek
            ) {
                continue;
            }
            let Some(reference) = config.credential.as_ref() else {
                continue;
            };
            if reference.as_str().starts_with("api-key:") {
                continue;
            }
            let Ok(secret) = self.credentials.resolve(reference) else {
                continue;
            };
            let scoped_reference = CredentialRef::new(format!(
                "api-key:{}:{}",
                config.id.as_str(),
                loom_core::RequestId::new()
            ));
            scoped_store.store(&scoped_reference, secret)?;
            config.credential = Some(scoped_reference);
            migrated = true;
        }
        Ok(migrated)
    }

    pub fn demo() -> Self {
        Self::build_demo(Arc::new(InMemoryCredentialStore::default()), false)
    }

    pub fn demo_with_credentials(credentials: Arc<dyn CredentialStore>) -> Self {
        Self::build_demo(credentials, true)
    }

    pub fn configured(credentials: Arc<dyn CredentialStore>) -> Result<Self> {
        let registry = Self::with_credentials(Arc::clone(&credentials));
        let model =
            std::env::var("LOOM_OPENAI_MODEL").unwrap_or_else(|_| OPENAI_DEFAULT_MODEL.to_owned());
        registry.register(ProviderConfig::openai(model))?;
        let deepseek_model = std::env::var("LOOM_DEEPSEEK_MODEL")
            .unwrap_or_else(|_| DEEPSEEK_DEFAULT_MODEL.to_owned());
        registry.register(ProviderConfig::deepseek(deepseek_model))?;
        registry.register(ProviderConfig::github_copilot(CredentialRef::new(
            GITHUB_COPILOT_CREDENTIAL_REF,
        )))?;
        if let Some(endpoint) = std::env::var_os("LOOM_OLLAMA_ENDPOINT") {
            let endpoint = endpoint.to_string_lossy();
            if !endpoint.trim().is_empty() {
                let model =
                    std::env::var("LOOM_OLLAMA_MODEL").unwrap_or_else(|_| "llama3.2".to_owned());
                registry.register(ProviderConfig::ollama(endpoint.into_owned(), model))?;
            }
        }
        Ok(registry)
    }

    pub fn build_demo(credentials: Arc<dyn CredentialStore>, include_github_copilot: bool) -> Self {
        let registry = Self::with_credentials(credentials);
        registry
            .register(ProviderConfig::deterministic())
            .expect("valid deterministic provider");
        registry
            .register(ProviderConfig::ollama(
                "http://127.0.0.1:11434",
                ModelId::new("llama3.2"),
            ))
            .expect("valid Ollama provider");
        registry
            .register(ProviderConfig::openai_compatible(
                "openai-compatible",
                "OpenAI-compatible gateway",
                "http://127.0.0.1:8000/v1/chat/completions",
                ModelDescriptor {
                    id: ModelId::new("openai-compatible/demo"),
                    provider: ProviderId::new("openai-compatible"),
                    display_name: "OpenAI-compatible demo model".to_owned(),
                    context_window: Some(16_384),
                    max_input_tokens: None,
                    max_output_tokens: None,
                    capabilities: ModelCapabilities {
                        streaming: false,
                        tool_calling: true,
                        vision: false,
                        json_mode: true,
                    },
                },
                None,
            ))
            .expect("valid OpenAI-compatible provider");
        if include_github_copilot {
            registry
                .register(ProviderConfig::github_copilot(CredentialRef::new(
                    GITHUB_COPILOT_CREDENTIAL_REF,
                )))
                .expect("valid GitHub Copilot provider");
        }
        registry
    }

    pub fn register(&self, config: ProviderConfig) -> Result<()> {
        if config.id.as_str().trim().is_empty() {
            return Err(LoomError::invalid_request("provider id must not be empty"));
        }
        if config.models.is_empty() {
            return Err(LoomError::invalid_request(format!(
                "provider '{}' must expose at least one model",
                config.id.as_str()
            )));
        }
        if config
            .credential
            .as_ref()
            .is_some_and(|reference| reference.as_str().trim().is_empty())
        {
            return Err(LoomError::invalid_request(
                "provider credential reference must not be empty",
            ));
        }
        for model in &config.models {
            if model.id.as_str().trim().is_empty() || model.provider.as_str().trim().is_empty() {
                return Err(LoomError::invalid_request(
                    "provider models must have non-empty ids",
                ));
            }
            if config.kind == ProviderKind::Deterministic
                && model.id.as_str() != "deterministic/demo"
            {
                return Err(LoomError::invalid_request(
                    "the deterministic provider only supports model 'deterministic/demo'",
                ));
            }
        }
        self.configurations
            .lock()
            .map_err(|_| internal_lock_error("provider configuration"))?
            .insert(config.id.clone(), config);
        Ok(())
    }

    pub fn register_openai_compatible(
        &self,
        id: impl Into<ProviderId>,
        display_name: impl Into<String>,
        endpoint: impl Into<String>,
        model: ModelDescriptor,
        credential: Option<CredentialRef>,
    ) -> Result<()> {
        self.register(ProviderConfig::openai_compatible(
            id,
            display_name,
            endpoint,
            model,
            credential,
        ))
    }

    pub fn register_ollama(
        &self,
        endpoint: impl Into<String>,
        model: impl Into<ModelId>,
    ) -> Result<()> {
        self.register(ProviderConfig::ollama(endpoint, model))
    }

    pub fn register_github_copilot(&self, credential: impl Into<CredentialRef>) -> Result<()> {
        self.register(ProviderConfig::github_copilot(credential))
    }

    pub fn configure_github_copilot(&self, access_token: String) -> Result<()> {
        if access_token.trim().is_empty() {
            return Err(LoomError::invalid_request(
                "GitHub Copilot access token must not be empty",
            ));
        }
        let credential = CredentialRef::new(GITHUB_COPILOT_CREDENTIAL_REF);
        self.credentials.store(&credential, access_token)?;
        self.register(ProviderConfig::github_copilot(credential))
    }

    /// Attach a user supplied API key to an already registered API-key provider.
    /// Each setup gets a new opaque credential reference, so separate backend
    /// registries never share ownership through a predictable global key name.
    pub fn configure_api_key_provider(
        &self,
        provider_id: &ProviderId,
        api_key: String,
    ) -> Result<()> {
        if api_key.trim().is_empty() {
            return Err(LoomError::invalid_request(
                "provider API key must not be empty",
            ));
        }
        let credentials = self.instance_credentials()?;
        let reference = CredentialRef::new(format!(
            "api-key:{}:{}",
            provider_id.as_str(),
            loom_core::RequestId::new()
        ));
        let mut configurations = self
            .configurations
            .lock()
            .map_err(|_| internal_lock_error("provider configuration"))?;
        let config = configurations
            .get_mut(provider_id)
            .ok_or_else(|| LoomError::not_found("provider", provider_id.as_str()))?;
        if !matches!(
            config.kind,
            ProviderKind::OpenAi | ProviderKind::OpenAiCompatible | ProviderKind::DeepSeek
        ) {
            return Err(LoomError::invalid_request(
                "API keys can only be configured for API-key providers",
            ));
        }
        credentials.store(&reference, api_key)?;
        let previous = config.credential.replace(reference);
        drop(configurations);
        // A replaced installation-scoped key is dead weight; drop it so
        // rotating a key does not leave the old secret behind.
        if let Some(previous) = previous
            && previous.as_str().starts_with("api-key:")
            && let Err(error) = credentials.remove(&previous)
        {
            // The new key is stored and selected; a leftover old secret is not
            // worth failing the rotation the user asked for.
            log::warn!(
                "[loom-providers] rotated API key for '{}' but could not remove the previous \
                 credential: {}",
                provider_id.as_str(),
                error.message
            );
        }
        Ok(())
    }

    /// Drops the API key configured for `provider_id`, leaving the provider in
    /// the catalog so it can be configured again. Returns whether a key was
    /// present. Only API-key providers may be cleared.
    pub fn remove_api_key_provider(&self, provider_id: &ProviderId) -> Result<bool> {
        let reference = {
            let configurations = self
                .configurations
                .lock()
                .map_err(|_| internal_lock_error("provider configuration"))?;
            let config = configurations
                .get(provider_id)
                .ok_or_else(|| LoomError::not_found("provider", provider_id.as_str()))?;
            if !matches!(
                config.kind,
                ProviderKind::OpenAi | ProviderKind::OpenAiCompatible | ProviderKind::DeepSeek
            ) {
                return Err(LoomError::invalid_request(
                    "API keys can only be removed from API-key providers",
                ));
            }
            config.credential.clone()
        };
        let Some(reference) = reference else {
            return Ok(false);
        };
        // Only installation-scoped keys are safe to erase: a legacy reference
        // may be shared by another backend that has not migrated it yet.
        if reference.as_str().starts_with("api-key:") {
            self.instance_credentials()?.remove(&reference)?;
        }
        let mut configurations = self
            .configurations
            .lock()
            .map_err(|_| internal_lock_error("provider configuration"))?;
        if let Some(config) = configurations.get_mut(provider_id) {
            config.credential = None;
        }
        Ok(true)
    }

    /// Resolves the GitHub token used for repository operations (clone, browse,
    /// push, and pull requests). Prefers the repository-scoped credential
    /// obtained from the GitHub CLI OAuth app and falls back to the Copilot
    /// token, which can only browse public repositories. Callers must keep this
    /// token backend-only.
    pub fn github_account_token(&self) -> Result<String> {
        self.github_repository_token().or_else(|_| {
            self.credentials
                .resolve(&CredentialRef::new(GITHUB_COPILOT_CREDENTIAL_REF))
        })
    }

    /// Resolves the repository-scoped GitHub credential, if one is configured.
    pub fn github_repository_token(&self) -> Result<String> {
        self.credentials
            .resolve(&CredentialRef::new(GITHUB_REPOSITORY_CREDENTIAL_REF))
    }

    /// Stores a repository-scoped GitHub token obtained from the GitHub CLI
    /// OAuth app device flow. This token authorizes clone, push, and pull
    /// requests on the worker, which has no ambient Git credentials. The device
    /// login requests the `workflow` scope alongside `repo`, because GitHub
    /// rejects a push that creates or updates `.github/workflows/**` from a
    /// credential that lacks it.
    pub fn configure_github_repository(&self, access_token: String) -> Result<()> {
        if access_token.trim().is_empty() {
            return Err(LoomError::invalid_request(
                "GitHub repository access token must not be empty",
            ));
        }
        self.credentials.store(
            &CredentialRef::new(GITHUB_REPOSITORY_CREDENTIAL_REF),
            access_token,
        )
    }

    /// Whether the connected GitHub account is authorized for writes and pull
    /// requests. Disabled until the user opts in. Stored next to the GitHub
    /// credential so the grant survives restarts.
    pub fn github_write_access(&self) -> bool {
        self.credentials
            .resolve(&CredentialRef::new(GITHUB_WRITE_ACCESS_CREDENTIAL_REF))
            .map(|value| value == "true")
            .unwrap_or(false)
    }

    pub fn set_github_write_access(&self, enabled: bool) -> Result<()> {
        self.credentials.store(
            &CredentialRef::new(GITHUB_WRITE_ACCESS_CREDENTIAL_REF),
            if enabled { "true" } else { "false" }.to_owned(),
        )
    }

    pub fn add_model(&self, provider_id: &ProviderId, model: ModelDescriptor) -> Result<()> {
        if model.id.as_str().trim().is_empty() || model.provider.as_str().trim().is_empty() {
            return Err(LoomError::invalid_request(
                "provider model must have a non-empty id",
            ));
        }
        let mut configurations = self
            .configurations
            .lock()
            .map_err(|_| internal_lock_error("provider configuration"))?;
        let config = configurations
            .get_mut(provider_id)
            .ok_or_else(|| LoomError::not_found("provider", provider_id.as_str()))?;
        if config.models.iter().any(|existing| existing.id == model.id) {
            return Err(LoomError::conflict(format!(
                "provider '{}' already exposes model '{}'",
                provider_id.as_str(),
                model.id.as_str()
            )));
        }
        config.models.push(model);
        Ok(())
    }

    pub fn list_providers(&self) -> Result<Vec<ProviderSummary>> {
        let configurations = self
            .configurations
            .lock()
            .map_err(|_| internal_lock_error("provider configuration"))?;
        let health = self
            .health
            .lock()
            .map_err(|_| internal_lock_error("provider health"))?;
        Ok(configurations
            .values()
            .filter(|config| self.is_configured(config))
            .map(|config| ProviderSummary {
                id: config.id.clone(),
                kind: config.kind,
                display_name: config.display_name.clone(),
                models: config.models.clone(),
                credential_id: config
                    .credential
                    .as_ref()
                    .map(|reference| reference.as_str().to_owned()),
                api_key_configurable: matches!(
                    config.kind,
                    ProviderKind::OpenAi | ProviderKind::OpenAiCompatible | ProviderKind::DeepSeek
                ),
                health: health.get(&config.id).cloned().unwrap_or_default(),
            })
            .collect())
    }

    pub fn list_models(&self) -> Result<Vec<ModelDescriptor>> {
        let configurations = self
            .configurations
            .lock()
            .map_err(|_| internal_lock_error("provider configuration"))?;
        Ok(configurations
            .values()
            .filter(|config| self.is_usable(config))
            .flat_map(|config| config.models.iter().cloned())
            .collect())
    }

    pub fn is_configured(&self, config: &ProviderConfig) -> bool {
        config
            .credential
            .as_ref()
            .is_none_or(|reference| self.resolve_credential(reference).is_ok())
    }

    /// Whether a provider can serve model requests. Official API providers
    /// still appear in `list_providers` for setup, but their seeded models are
    /// only catalogued once a credential is stored.
    fn is_usable(&self, config: &ProviderConfig) -> bool {
        self.is_configured(config)
            && (!config.kind.requires_credential() || config.credential.is_some())
    }

    pub fn models(&self) -> Result<Vec<ModelDescriptor>> {
        self.list_models()
    }

    pub fn discover_models(&self, provider_id: &ProviderId) -> Result<Vec<ModelDescriptor>> {
        let config = self
            .configurations
            .lock()
            .map_err(|_| internal_lock_error("provider configuration"))?
            .get(provider_id)
            .cloned()
            .ok_or_else(|| LoomError::not_found("provider", provider_id.as_str()))?;
        if config.kind == ProviderKind::Deterministic {
            return Ok(config.models);
        }
        if config.kind == ProviderKind::GitHubCopilot {
            let provider = self.create_github_copilot_provider(&config)?;
            let models = provider.discover_models()?;
            self.configurations
                .lock()
                .map_err(|_| internal_lock_error("provider configuration"))?
                .get_mut(provider_id)
                .ok_or_else(|| LoomError::not_found("provider", provider_id.as_str()))?
                .models = models.clone();
            return Ok(models);
        }
        let endpoint = config.endpoint.as_deref().ok_or_else(|| {
            LoomError::invalid_request(format!(
                "provider '{}' has no endpoint",
                provider_id.as_str()
            ))
        })?;
        let credential = config
            .credential
            .as_ref()
            .map(|reference| self.resolve_credential(reference))
            .transpose()?
            .unwrap_or_default();
        // Credentials are attached only to the backend request.
        let mut headers = Vec::new();
        if !credential.is_empty() {
            headers.push(("Authorization", format!("Bearer {credential}")));
        }
        let (status, body) = run_async(request_json(
            reqwest::Method::GET,
            &health_endpoint(endpoint),
            &headers,
            None,
        ))?;
        if status >= 400 {
            return Err(normalize_provider_error(provider_id.as_str(), status));
        }
        let models = body
            .get("data")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::ProviderInvalidResponse,
                    format!(
                        "{} model discovery did not contain a data array",
                        provider_id.as_str()
                    ),
                    false,
                )
            })?
            .iter()
            .filter(|model| {
                config.kind != ProviderKind::OpenAi
                    || model
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(openai_model_supported)
            })
            .map(|model| {
                let id = model
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        LoomError::new(
                            ErrorCode::ProviderInvalidResponse,
                            "provider model discovery returned a model without an id",
                            false,
                        )
                    })?;
                Ok(ModelDescriptor {
                    id: ModelId::new(id),
                    provider: provider_id.clone(),
                    display_name: id.to_owned(),
                    context_window: discovered_context_window(model)
                        .or_else(|| {
                            config
                                .models
                                .iter()
                                .find(|configured| configured.id.as_str() == id)
                                .and_then(|configured| configured.context_window)
                        })
                        // The official OpenAI API publishes no window, so fall back
                        // to the declared family table there. A gateway or local
                        // runtime may serve the same name with a different real
                        // window, so the table does not apply to those kinds.
                        .or_else(|| {
                            (config.kind == ProviderKind::OpenAi)
                                .then_some(id)
                                .and_then(declared_context_window)
                        }),
                    max_input_tokens: discovered_input_tokens(model).or_else(|| {
                        config
                            .models
                            .iter()
                            .find(|configured| configured.id.as_str() == id)
                            .and_then(|configured| configured.max_input_tokens)
                    }),
                    max_output_tokens: discovered_output_tokens(model).or_else(|| {
                        config
                            .models
                            .iter()
                            .find(|configured| configured.id.as_str() == id)
                            .and_then(|configured| configured.max_output_tokens)
                    }),
                    capabilities: config
                        .models
                        .first()
                        .map_or_else(ModelCapabilities::default, |model| {
                            model.capabilities.clone()
                        }),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if models.is_empty() {
            return Err(LoomError::new(
                ErrorCode::ProviderInvalidResponse,
                format!(
                    "{} model discovery returned no models",
                    provider_id.as_str()
                ),
                false,
            ));
        }
        self.configurations
            .lock()
            .map_err(|_| internal_lock_error("provider configuration"))?
            .get_mut(provider_id)
            .ok_or_else(|| LoomError::not_found("provider", provider_id.as_str()))?
            .models = models.clone();
        Ok(models)
    }

    /// Resolve once against configured providers, including unavailable ones:
    /// a missing credential must never silently reroute an explicit selection.
    fn resolve_model(&self, model: &ModelId) -> Result<(ProviderConfig, ModelDescriptor)> {
        let configurations = self
            .configurations
            .lock()
            .map_err(|_| internal_lock_error("provider configuration"))?;
        let selection = model.provider_model();
        let raw = selection.as_ref().map_or(model, |(_, raw)| raw);
        let mut matches = configurations
            .values()
            .filter(|config| selection.as_ref().is_none_or(|(id, _)| &config.id == id))
            .filter_map(|config| {
                config
                    .models
                    .iter()
                    .find(|item| &item.id == raw)
                    .map(|descriptor| (config, descriptor))
            });
        let Some((config, descriptor)) = matches.next() else {
            return Err(LoomError::new(
                ErrorCode::ProviderUnavailable,
                format!("model '{}' is not configured", model.as_str()),
                false,
            ));
        };
        if matches.next().is_some() {
            let providers = configurations
                .values()
                .filter(|config| config.models.iter().any(|item| &item.id == raw))
                .map(|config| ModelId::for_provider(&config.id, raw).as_str().to_owned())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(LoomError::invalid_request(format!(
                "model '{}' is ambiguous; select an explicit provider-qualified model: {providers}",
                model.as_str()
            )));
        }
        let mut descriptor = descriptor.clone();
        descriptor.provider = config.id.clone();
        Ok((config.clone(), descriptor))
    }

    pub fn describe_model(&self, model: &ModelId) -> Result<ModelDescriptor> {
        let (_, mut descriptor) = self.resolve_model(model)?;
        descriptor.id = model.clone();
        Ok(descriptor)
    }

    pub fn negotiate_capabilities(
        &self,
        model: &ModelId,
        requested: ModelCapabilities,
    ) -> Result<ModelCapabilities> {
        Ok(self
            .describe_model(model)?
            .capabilities
            .intersect(requested))
    }

    pub fn create_provider(&self, model: &ModelId) -> Result<Box<dyn ModelProvider>> {
        self.create_provider_at(model, 0)
    }

    pub fn provider(&self, model: &ModelId) -> Result<Box<dyn ModelProvider>> {
        self.create_provider(model)
    }

    pub fn create_provider_at(
        &self,
        model: &ModelId,
        deterministic_cursor: usize,
    ) -> Result<Box<dyn ModelProvider>> {
        self.create_provider_with_credential(model, deterministic_cursor, None)
    }

    /// The provider id that serves `model`.
    pub fn provider_id_for_model(&self, model: &ModelId) -> Result<ProviderId> {
        Ok(self.resolve_model(model)?.0.id)
    }

    /// Creates a provider for `model`, authenticating with `secret` instead of
    /// the provider's configured credential when one is given.
    ///
    /// The caller resolves a pinned secret itself, so a pinned credential that
    /// is missing fails before this call and never falls back to the
    /// configured credential.
    pub fn create_provider_with_secret(
        &self,
        model: &ModelId,
        deterministic_cursor: usize,
        secret: Option<String>,
    ) -> Result<Box<dyn ModelProvider>> {
        self.create_provider_with_credential(model, deterministic_cursor, secret)
    }

    fn create_provider_with_credential(
        &self,
        model: &ModelId,
        deterministic_cursor: usize,
        pinned_secret: Option<String>,
    ) -> Result<Box<dyn ModelProvider>> {
        let (config, descriptor) = self.resolve_model(model)?;
        let resolve_secret = |config: &ProviderConfig| -> Result<String> {
            if let Some(secret) = &pinned_secret {
                return Ok(secret.clone());
            }
            Ok(config
                .credential
                .as_ref()
                .map(|reference| self.resolve_credential(reference))
                .transpose()?
                .unwrap_or_default())
        };
        let mut selector_descriptor = descriptor.clone();
        selector_descriptor.id = model.clone();
        let provider: Box<dyn ModelProvider> = match config.kind {
            ProviderKind::Deterministic => {
                Box::new(DeterministicProvider::demo().with_cursor(deterministic_cursor))
            }
            ProviderKind::OpenAiCompatible => {
                let endpoint = config.endpoint.as_deref().ok_or_else(|| {
                    LoomError::invalid_request(format!(
                        "provider '{}' has no endpoint",
                        config.id.as_str()
                    ))
                })?;
                let secret = resolve_secret(&config)?;
                Box::new(OpenAiCompatibleProvider::with_descriptor(
                    endpoint, secret, descriptor,
                ))
            }

            ProviderKind::OpenAi => {
                let endpoint = config.endpoint.as_deref().unwrap_or(OPENAI_API_ENDPOINT);
                let secret = resolve_secret(&config)?;
                Box::new(OpenAiCompatibleProvider::with_descriptor(
                    endpoint, secret, descriptor,
                ))
            }

            ProviderKind::Ollama => {
                let endpoint = config.endpoint.as_deref().ok_or_else(|| {
                    LoomError::invalid_request(format!(
                        "provider '{}' has no endpoint",
                        config.id.as_str()
                    ))
                })?;
                Box::new(OllamaProvider::with_descriptor(endpoint, descriptor))
            }
            ProviderKind::DeepSeek => {
                let endpoint = config.endpoint.as_deref().unwrap_or(DEEPSEEK_API_ENDPOINT);
                let secret = resolve_secret(&config)?;
                Box::new(OpenAiCompatibleProvider::with_descriptor(
                    endpoint, secret, descriptor,
                ))
            }
            ProviderKind::GitHubCopilot => Box::new(
                match pinned_secret.clone() {
                    Some(secret) => self.github_copilot_provider_with_token(&config, secret),
                    None => self.create_github_copilot_provider(&config)?,
                }
                .with_model_descriptor(descriptor),
            ),
        };
        Ok(Box::new(SelectedModelProvider {
            descriptor: selector_descriptor,
            inner: Box::new(AccountingProvider {
                inner: provider,
                ledger: Arc::clone(&self.usage),
                input_cost_micros_per_1k: config.input_cost_micros_per_1k,
                output_cost_micros_per_1k: config.output_cost_micros_per_1k,
            }),
        }))
    }

    pub fn create_github_copilot_provider(
        &self,
        config: &ProviderConfig,
    ) -> Result<GitHubCopilotProvider> {
        let credential = config.credential.as_ref().ok_or_else(|| {
            LoomError::new(
                ErrorCode::ProviderAuthentication,
                format!(
                    "provider '{}' has no configured credential",
                    config.id.as_str()
                ),
                false,
            )
        })?;
        let github_token = self.resolve_credential(credential)?;
        Ok(self.github_copilot_provider_with_token(config, github_token))
    }

    fn github_copilot_provider_with_token(
        &self,
        config: &ProviderConfig,
        github_token: String,
    ) -> GitHubCopilotProvider {
        let endpoint = config
            .endpoint
            .as_deref()
            .unwrap_or(GITHUB_COPILOT_API_ENDPOINT);
        GitHubCopilotProvider::with_descriptor(
            endpoint,
            github_token,
            config
                .models
                .first()
                .cloned()
                .unwrap_or_else(github_copilot_descriptor),
        )
    }

    pub fn pricing(&self, model: &ModelId) -> Result<(u64, u64)> {
        let (config, _) = self.resolve_model(model)?;
        Ok((
            config.input_cost_micros_per_1k,
            config.output_cost_micros_per_1k,
        ))
    }

    pub fn export_configs(&self) -> Result<Vec<ProviderConfig>> {
        Ok(self
            .configurations
            .lock()
            .map_err(|_| internal_lock_error("provider configuration"))?
            .values()
            .cloned()
            .collect())
    }

    pub fn restore_configs(&self, configs: Vec<ProviderConfig>) -> Result<()> {
        let existing = self
            .configurations
            .lock()
            .map_err(|_| internal_lock_error("provider configuration"))?
            .clone();
        for mut config in configs {
            if let Some(current) = existing.get(&config.id) {
                if config.kind == ProviderKind::GitHubCopilot && config.models.len() <= 1 {
                    continue;
                }
                if config.kind != current.kind {
                    continue;
                }
                if config.kind != ProviderKind::GitHubCopilot
                    && (!matches!(
                        config.kind,
                        ProviderKind::OpenAi
                            | ProviderKind::OpenAiCompatible
                            | ProviderKind::DeepSeek
                    ) || config.credential.is_none())
                {
                    continue;
                }
                if config.kind == ProviderKind::GitHubCopilot
                    && current.credential != config.credential
                {
                    continue;
                }
                if config
                    .credential
                    .as_ref()
                    .is_some_and(|reference| self.resolve_credential(reference).is_err())
                {
                    continue;
                }
            } else if config
                .credential
                .as_ref()
                .is_none_or(|reference| self.resolve_credential(reference).is_err())
            {
                continue;
            }
            if config.kind == ProviderKind::GitHubCopilot {
                config.models = vec![github_copilot_descriptor()];
            } else if let Some(current) = existing.get(&config.id)
                && current.kind == config.kind
            {
                // Persisted configurations can predate a built-in descriptor
                // gaining a limit (for example a provider's maximum output
                // tokens). Fill only the missing values so an existing install
                // picks up the corrected cap without discarding user choices.
                fill_missing_model_limits(&mut config, current);
            }
            self.register(config)?;
        }
        Ok(())
    }

    pub fn export_health(&self) -> Result<BTreeMap<ProviderId, ProviderHealth>> {
        Ok(self
            .health
            .lock()
            .map_err(|_| internal_lock_error("provider health"))?
            .clone())
    }

    pub fn restore_health(&self, health: BTreeMap<ProviderId, ProviderHealth>) -> Result<()> {
        *self
            .health
            .lock()
            .map_err(|_| internal_lock_error("provider health"))? = health;
        Ok(())
    }

    pub fn usage(&self) -> Result<UsageLedger> {
        Ok(self
            .usage
            .lock()
            .map_err(|_| internal_lock_error("provider usage"))?
            .clone())
    }

    pub fn restore_usage(&self, ledger: UsageLedger) -> Result<()> {
        *self
            .usage
            .lock()
            .map_err(|_| internal_lock_error("provider usage"))? = ledger;
        Ok(())
    }

    pub fn health(&self, provider_id: &ProviderId) -> Result<ProviderHealth> {
        Ok(self
            .health
            .lock()
            .map_err(|_| internal_lock_error("provider health"))?
            .get(provider_id)
            .cloned()
            .unwrap_or_default())
    }

    pub fn check_health(&self, provider_id: &ProviderId) -> Result<ProviderHealth> {
        let config = self
            .configurations
            .lock()
            .map_err(|_| internal_lock_error("provider configuration"))?
            .get(provider_id)
            .cloned()
            .ok_or_else(|| LoomError::not_found("provider", provider_id.as_str()))?;
        let model = config.models.first().ok_or_else(|| {
            LoomError::new(
                ErrorCode::ProviderUnavailable,
                format!("provider '{}' has no models", provider_id.as_str()),
                false,
            )
        })?;
        let result = self
            .create_provider(&ModelId::for_provider(provider_id, &model.id))
            .and_then(|mut provider| provider.health_check());
        let mut health = self
            .health
            .lock()
            .map_err(|_| internal_lock_error("provider health"))?;
        let current = health.entry(provider_id.clone()).or_default();
        current.checked_at = Some(Timestamp::now());
        match result {
            Ok(()) => {
                current.state = ProviderHealthState::Healthy;
                current.consecutive_failures = 0;
                current.last_error = None;
            }
            Err(error) => {
                current.consecutive_failures = current.consecutive_failures.saturating_add(1);
                current.state = if error.retryable {
                    ProviderHealthState::Degraded
                } else {
                    ProviderHealthState::Unavailable
                };
                current.last_error = Some(error);
            }
        }
        Ok(current.clone())
    }

    pub fn health_check(&self, provider_id: &ProviderId) -> Result<ProviderHealth> {
        self.check_health(provider_id)
    }
}

/// Keep selector identity outside the adapter and raw identity at its boundary.
/// Accounting remains inside this wrapper so persisted usage stays provider/raw.
struct SelectedModelProvider {
    inner: Box<dyn ModelProvider>,
    descriptor: ModelDescriptor,
}

impl SelectedModelProvider {
    fn raw_request(&self, request: &ModelRequest) -> ModelRequest {
        let mut request = request.clone();
        request.model = self.inner.descriptor().id.clone();
        request
    }
}

impl ModelProvider for SelectedModelProvider {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn stream(
        &mut self,
        request: &ModelRequest,
        cancel: &CancellationToken,
        sink: &mut dyn ModelStreamSink,
    ) -> Result<()> {
        if request.model != self.descriptor.id {
            return Err(LoomError::invalid_request(format!(
                "request model '{}' does not match provider model '{}'",
                request.model.as_str(),
                self.descriptor.id.as_str()
            )));
        }
        self.inner.stream(&self.raw_request(request), cancel, sink)
    }

    fn count_tokens(&self, request: &ModelRequest) -> u64 {
        self.inner.count_tokens(&self.raw_request(request))
    }

    fn health_check(&mut self) -> Result<()> {
        self.inner.health_check()
    }

    fn reset(&mut self) {
        self.inner.reset();
    }
}

pub struct AccountingProvider {
    pub inner: Box<dyn ModelProvider>,
    pub ledger: Arc<Mutex<UsageLedger>>,
    pub input_cost_micros_per_1k: u64,
    pub output_cost_micros_per_1k: u64,
}

impl ModelProvider for AccountingProvider {
    fn descriptor(&self) -> &ModelDescriptor {
        self.inner.descriptor()
    }

    fn stream(
        &mut self,
        request: &ModelRequest,
        cancel: &CancellationToken,
        sink: &mut dyn ModelStreamSink,
    ) -> Result<()> {
        let mut usage = TokenUsage::default();
        let result = {
            let usage = &mut usage;
            // Retry at the pre-body boundary: a transient failure before the
            // provider emits its first event is retried without the caller
            // observing (or the ledger counting) the discarded attempt.
            stream_with_retry(
                self.inner.as_mut(),
                request,
                cancel,
                &mut |event| {
                    if let ModelStreamEvent::Usage { usage: event_usage } = &event {
                        usage.input_tokens =
                            usage.input_tokens.saturating_add(event_usage.input_tokens);
                        usage.output_tokens = usage
                            .output_tokens
                            .saturating_add(event_usage.output_tokens);
                        usage.cached_input_tokens = usage
                            .cached_input_tokens
                            .saturating_add(event_usage.cached_input_tokens);
                    }
                    sink.emit(event)
                },
                RetryPolicy::default(),
            )
        };
        let cost_micros = cost_for_usage(
            &usage,
            self.input_cost_micros_per_1k,
            self.output_cost_micros_per_1k,
        );
        self.ledger
            .lock()
            .map_err(|_| internal_lock_error("provider usage"))?
            .record(
                self.inner.descriptor().provider.clone(),
                self.inner.descriptor().id.clone(),
                usage,
                cost_micros,
            );
        result
    }

    fn count_tokens(&self, request: &ModelRequest) -> u64 {
        self.inner.count_tokens(request)
    }

    fn health_check(&mut self) -> Result<()> {
        self.inner.health_check()
    }

    fn reset(&mut self) {
        self.inner.reset();
    }
}

/// Fill limits the persisted configuration never recorded from the built-in
/// provider descriptor. Only `None` values are replaced, so an existing install
/// picks up newly advertised caps (such as a provider's maximum output tokens)
/// without discarding explicit user choices.
fn fill_missing_model_limits(config: &mut ProviderConfig, known: &ProviderConfig) {
    for model in &mut config.models {
        let Some(descriptor) = known
            .models
            .iter()
            .find(|candidate| candidate.id == model.id)
        else {
            continue;
        };
        model.context_window = model.context_window.or(descriptor.context_window);
        model.max_input_tokens = model.max_input_tokens.or(descriptor.max_input_tokens);
        model.max_output_tokens = model.max_output_tokens.or(descriptor.max_output_tokens);
    }
}

#[cfg(test)]
mod selection_tests {
    use super::*;
    use loom_model::CollectingSink;

    fn shared_registry() -> ProviderRegistry {
        let credentials = Arc::new(InMemoryCredentialStore::default());
        credentials.insert(CredentialRef::new("openai-key"), "test-openai");
        credentials.insert(CredentialRef::new("copilot-key"), "test-copilot");
        let registry = ProviderRegistry::with_credentials(credentials);
        let mut openai = ProviderConfig::openai("gpt-4o").with_pricing(100, 200);
        openai.credential = Some(CredentialRef::new("openai-key"));
        openai.models[0].context_window = Some(128_000);
        let mut copilot = ProviderConfig::github_copilot(CredentialRef::new("copilot-key"))
            .with_pricing(300, 400);
        copilot.models[0].id = ModelId::new("gpt-4o");
        copilot.models[0].context_window = Some(64_000);
        copilot.models[0].capabilities.vision = !openai.models[0].capabilities.vision;
        registry.register(openai).unwrap();
        registry.register(copilot).unwrap();
        registry
    }

    #[test]
    fn shared_real_model_ids_select_exact_provider_metadata_and_adapter() {
        let registry = shared_registry();
        let raw = ModelId::new("gpt-4o");
        for (id, window, pricing) in [
            (OPENAI_PROVIDER_ID, 128_000, (100, 200)),
            (GITHUB_COPILOT_PROVIDER_ID, 64_000, (300, 400)),
        ] {
            let provider_id = ProviderId::new(id);
            let selector = ModelId::for_provider(&provider_id, &raw);
            let descriptor = registry.describe_model(&selector).unwrap();
            assert_eq!(descriptor.id, selector);
            assert_eq!(descriptor.provider, provider_id);
            assert_eq!(descriptor.context_window, Some(window));
            assert_eq!(registry.pricing(&selector).unwrap(), pricing);
            let mut adapter = registry.create_provider(&selector).unwrap();
            assert_eq!(adapter.descriptor(), &descriptor);
            assert_eq!(adapter.list_models(), vec![descriptor.clone()]);
            assert_eq!(adapter.capabilities(), descriptor.capabilities);
            assert_eq!(
                registry
                    .negotiate_capabilities(
                        &selector,
                        ModelCapabilities {
                            streaming: true,
                            tool_calling: true,
                            vision: true,
                            json_mode: true,
                        }
                    )
                    .unwrap(),
                descriptor.capabilities
            );
            adapter.reset();
        }
        // The API/storage catalog stays raw for old clients and saved configs.
        assert!(
            registry
                .list_models()
                .unwrap()
                .iter()
                .all(|model| model.id == raw)
        );
        assert!(
            registry
                .list_providers()
                .unwrap()
                .iter()
                .all(|provider| provider.models[0].id == raw)
        );
        assert!(
            registry
                .export_configs()
                .unwrap()
                .iter()
                .all(|config| config.models[0].id == raw)
        );
    }

    #[test]
    fn raw_shared_ids_are_ambiguous_even_when_one_provider_is_unavailable() {
        let registry = shared_registry();
        let raw = ModelId::new("gpt-4o");
        for error in [
            registry.describe_model(&raw).unwrap_err(),
            registry.pricing(&raw).unwrap_err(),
            registry.create_provider(&raw).err().unwrap(),
        ] {
            assert_eq!(error.code, ErrorCode::InvalidRequest);
            assert!(error.message.contains("ambiguous"));
            assert!(error.message.contains(OPENAI_PROVIDER_ID));
            assert!(error.message.contains(GITHUB_COPILOT_PROVIDER_ID));
        }
        let selector = ModelId::for_provider(&ProviderId::new(GITHUB_COPILOT_PROVIDER_ID), &raw);
        registry
            .configurations
            .lock()
            .unwrap()
            .get_mut(&ProviderId::new(GITHUB_COPILOT_PROVIDER_ID))
            .unwrap()
            .credential = Some(CredentialRef::new("missing-credential"));
        assert!(registry.create_provider(&selector).is_err());
        assert!(registry.create_provider(&raw).is_err());
        // Removing the provider (or the selected model) never selects OpenAI.
        registry
            .configurations
            .lock()
            .unwrap()
            .remove(&ProviderId::new(GITHUB_COPILOT_PROVIDER_ID));
        assert!(registry.describe_model(&selector).is_err());
        assert!(registry.pricing(&selector).is_err());
        assert!(registry.create_provider(&selector).is_err());
        assert_eq!(
            registry.describe_model(&raw).unwrap().provider.as_str(),
            OPENAI_PROVIDER_ID
        );
        assert!(registry.create_provider(&raw).is_ok());
        assert_eq!(registry.pricing(&raw).unwrap(), (100, 200));
        let missing = ModelId::for_provider(
            &ProviderId::new(OPENAI_PROVIDER_ID),
            &ModelId::new("missing"),
        );
        assert!(registry.create_provider(&missing).is_err());
    }

    #[test]
    fn selected_provider_rejects_a_request_for_another_provider_before_dispatch() {
        let registry = shared_registry();
        let raw = ModelId::new("gpt-4o");
        let selected = ModelId::for_provider(&ProviderId::new(OPENAI_PROVIDER_ID), &raw);
        let other = ModelId::for_provider(&ProviderId::new(GITHUB_COPILOT_PROVIDER_ID), &raw);
        let mut adapter = registry.create_provider(&selected).unwrap();
        for model in [other, raw] {
            let request = ModelRequest {
                model,
                messages: vec![],
                tools: vec![],
                options: Default::default(),
            };
            let mut sink = CollectingSink::default();
            let error = adapter
                .stream(&request, &CancellationToken::new(), &mut sink)
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidRequest);
            assert!(error.message.contains("does not match"));
            assert!(sink.events.is_empty());
        }
        assert_eq!(registry.usage().unwrap().summary(None, None).requests, 0);
    }

    struct RawBoundaryProvider {
        descriptor: ModelDescriptor,
    }

    impl ModelProvider for RawBoundaryProvider {
        fn descriptor(&self) -> &ModelDescriptor {
            &self.descriptor
        }
        fn stream(
            &mut self,
            request: &ModelRequest,
            _: &CancellationToken,
            sink: &mut dyn ModelStreamSink,
        ) -> Result<()> {
            assert_eq!(request.model, self.descriptor.id);
            assert_eq!(openai_request_payload(request)["model"], "gpt-4o");
            assert!(!uses_responses_endpoint(request.model.as_str()));
            sink.emit(ModelStreamEvent::Usage {
                usage: TokenUsage {
                    input_tokens: 1000,
                    output_tokens: 1000,
                    cached_input_tokens: 0,
                },
            })?;
            Ok(())
        }
        fn count_tokens(&self, request: &ModelRequest) -> u64 {
            assert_eq!(request.model, self.descriptor.id);
            42
        }
    }

    #[test]
    fn selector_boundary_preserves_raw_requests_and_usage_keys() {
        let registry = shared_registry();
        let raw = ModelId::new("gpt-4o");
        let request = ModelRequest {
            model: raw.clone(),
            messages: vec![],
            tools: vec![],
            options: Default::default(),
        };
        for id in [OPENAI_PROVIDER_ID, GITHUB_COPILOT_PROVIDER_ID] {
            let provider_id = ProviderId::new(id);
            let selector = ModelId::for_provider(&provider_id, &raw);
            let descriptor = registry.describe_model(&selector).unwrap();
            let (_, raw_descriptor) = registry.resolve_model(&selector).unwrap();
            let mut adapter = SelectedModelProvider {
                descriptor,
                inner: Box::new(AccountingProvider {
                    inner: Box::new(RawBoundaryProvider {
                        descriptor: raw_descriptor,
                    }),
                    ledger: Arc::clone(&registry.usage),
                    input_cost_micros_per_1k: 100,
                    output_cost_micros_per_1k: 200,
                }),
            };
            let mut request = request.clone();
            request.model = selector.clone();
            assert_eq!(adapter.count_tokens(&request), 42);
            assert!(adapter.health_check().is_ok());
            adapter
                .stream(
                    &request,
                    &CancellationToken::new(),
                    &mut CollectingSink::default(),
                )
                .unwrap();
            assert_eq!(request.model, selector);
            let ledger = registry.usage().unwrap();
            assert_eq!(ledger.summary(None, Some(&selector)).requests, 1);
            assert_eq!(
                ledger.summary(Some(&provider_id), Some(&raw)).cost_micros,
                300
            );
            let other = ProviderId::new(if id == OPENAI_PROVIDER_ID {
                GITHUB_COPILOT_PROVIDER_ID
            } else {
                OPENAI_PROVIDER_ID
            });
            assert_eq!(ledger.summary(Some(&other), Some(&selector)).requests, 0);
            assert!(ledger.aggregates.keys().all(|key| key.model == raw));
        }
        assert_eq!(
            registry.usage().unwrap().summary(None, Some(&raw)).requests,
            2
        );
    }
}
