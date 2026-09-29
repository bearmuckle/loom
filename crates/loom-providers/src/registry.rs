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
        let reference = CredentialRef::new(format!(
            "api-key:{}:{}",
            provider_id.as_str(),
            loom_core::RequestId::new()
        ));
        let credentials = self
            .api_key_credentials
            .lock()
            .map_err(|_| internal_lock_error("API-key credential store"))?
            .clone()
            .unwrap_or_else(|| Arc::clone(&self.credentials));
        credentials.store(&reference, api_key)?;
        config.credential = Some(reference);
        Ok(())
    }

    /// Resolves the GitHub account token used by Copilot and repository
    /// browsing. Callers must keep this token backend-only.
    pub fn github_account_token(&self) -> Result<String> {
        self.credentials
            .resolve(&CredentialRef::new(GITHUB_COPILOT_CREDENTIAL_REF))
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
            .filter(|config| self.is_configured(config))
            .flat_map(|config| config.models.iter().cloned())
            .collect())
    }

    pub fn is_configured(&self, config: &ProviderConfig) -> bool {
        config
            .credential
            .as_ref()
            .is_none_or(|reference| self.resolve_credential(reference).is_ok())
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
                    context_window: discovered_context_window(model).or_else(|| {
                        config
                            .models
                            .iter()
                            .find(|configured| configured.id.as_str() == id)
                            .and_then(|configured| configured.context_window)
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

    pub fn describe_model(&self, model: &ModelId) -> Result<ModelDescriptor> {
        let configurations = self
            .configurations
            .lock()
            .map_err(|_| internal_lock_error("provider configuration"))?;
        configurations
            .values()
            .flat_map(|config| config.models.iter())
            .find(|descriptor| &descriptor.id == model)
            .cloned()
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::ProviderUnavailable,
                    format!("model '{}' is not configured", model.as_str()),
                    false,
                )
            })
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
        let config = {
            let configurations = self
                .configurations
                .lock()
                .map_err(|_| internal_lock_error("provider configuration"))?;
            configurations
                .values()
                .find(|config| {
                    config
                        .models
                        .iter()
                        .any(|descriptor| &descriptor.id == model)
                })
                .cloned()
                .ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::ProviderUnavailable,
                        format!("model '{}' is not configured", model.as_str()),
                        false,
                    )
                })?
        };
        let descriptor = config
            .models
            .iter()
            .find(|descriptor| &descriptor.id == model)
            .cloned()
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::ProviderUnavailable,
                    format!("model '{}' is not configured", model.as_str()),
                    false,
                )
            })?;
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
                let secret = config
                    .credential
                    .as_ref()
                    .map(|reference| self.resolve_credential(reference))
                    .transpose()?
                    .unwrap_or_default();
                Box::new(OpenAiCompatibleProvider::with_descriptor(
                    endpoint, secret, descriptor,
                ))
            }

            ProviderKind::OpenAi => {
                let endpoint = config.endpoint.as_deref().unwrap_or(OPENAI_API_ENDPOINT);
                let secret = config
                    .credential
                    .as_ref()
                    .map(|reference| self.resolve_credential(reference))
                    .transpose()?
                    .unwrap_or_default();
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
                let secret = config
                    .credential
                    .as_ref()
                    .map(|reference| self.resolve_credential(reference))
                    .transpose()?
                    .unwrap_or_default();
                Box::new(OpenAiCompatibleProvider::with_descriptor(
                    endpoint, secret, descriptor,
                ))
            }
            ProviderKind::GitHubCopilot => Box::new(
                self.create_github_copilot_provider(&config)?
                    .with_model_descriptor(descriptor),
            ),
        };
        Ok(Box::new(AccountingProvider {
            inner: provider,
            ledger: Arc::clone(&self.usage),
            input_cost_micros_per_1k: config.input_cost_micros_per_1k,
            output_cost_micros_per_1k: config.output_cost_micros_per_1k,
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
        let github_token = self.credentials.resolve(credential)?;
        let endpoint = config
            .endpoint
            .as_deref()
            .unwrap_or(GITHUB_COPILOT_API_ENDPOINT);
        Ok(GitHubCopilotProvider::with_descriptor(
            endpoint,
            github_token,
            config
                .models
                .first()
                .cloned()
                .unwrap_or_else(github_copilot_descriptor),
        ))
    }

    pub fn pricing(&self, model: &ModelId) -> Result<(u64, u64)> {
        let configurations = self
            .configurations
            .lock()
            .map_err(|_| internal_lock_error("provider configuration"))?;
        configurations
            .values()
            .find(|config| {
                config
                    .models
                    .iter()
                    .any(|descriptor| &descriptor.id == model)
            })
            .map(|config| {
                (
                    config.input_cost_micros_per_1k,
                    config.output_cost_micros_per_1k,
                )
            })
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::ProviderUnavailable,
                    format!("model '{}' is not configured", model.as_str()),
                    false,
                )
            })
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
            .create_provider(&model.id)
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
            self.inner.stream(request, cancel, &mut |event| {
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
            })
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
