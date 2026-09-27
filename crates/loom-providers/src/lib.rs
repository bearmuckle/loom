use std::{
    collections::BTreeMap,
    fmt, fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use loom_core::{ErrorCode, LoomError, Result, Timestamp, ToolCallId, UsageSnapshot};
use loom_model::{
    CancellationToken, FinishReason, MessageRole, ModelCapabilities, ModelDescriptor, ModelId,
    ModelMessage, ModelRequest, ModelStreamEvent, ModelStreamSink, ProviderId, StreamFlow,
    TokenUsage, ToolCall,
};
pub use loom_model::{
    ModelProvider, ProviderDescriptor, ProviderHealth, ProviderHealthState, ProviderKind,
    ProviderSummary, ProviderUsageRecord, ProviderUsageSummary, UnavailableProvider,
    estimate_tokens,
};
use serde::{Deserialize, Serialize};

pub const GITHUB_COPILOT_PROVIDER_ID: &str = "github-copilot";
pub const GITHUB_COPILOT_CREDENTIAL_REF: &str = "github-copilot";
pub const GITHUB_COPILOT_DEFAULT_MODEL: &str = "gpt-5.6-luna";
const GITHUB_OAUTH_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const GITHUB_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const GITHUB_ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const GITHUB_COPILOT_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";
const GITHUB_COPILOT_API_ENDPOINT: &str = "https://api.githubcopilot.com";
const GITHUB_COPILOT_EDITOR_VERSION: &str = "vscode/1.96.2";
const GITHUB_COPILOT_PLUGIN_VERSION: &str = "copilot-chat/0.26.7";
const GITHUB_COPILOT_USER_AGENT: &str = "GitHubCopilotChat/0.26.7";
const GITHUB_API_VERSION: &str = "2025-04-01";
const PROVIDER_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct CredentialRef(String);

pub type CredentialReference = CredentialRef;

impl CredentialRef {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for CredentialRef {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for CredentialRef {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

pub trait CredentialStore: Send + Sync {
    fn resolve(&self, reference: &CredentialRef) -> Result<String>;

    fn store(&self, _reference: &CredentialRef, _secret: String) -> Result<()> {
        Err(LoomError::new(
            ErrorCode::InvalidState,
            "credential store does not support updates",
            false,
        ))
    }
}

#[derive(Clone)]
pub struct FileCredentialStore {
    path: PathBuf,
    values: Arc<Mutex<BTreeMap<String, String>>>,
}

impl fmt::Debug for FileCredentialStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileCredentialStore")
            .field("path", &self.path)
            .field(
                "credential_count",
                &self.values.lock().map(|values| values.len()).unwrap_or(0),
            )
            .finish()
    }
}

impl FileCredentialStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let values = Self::read_values(&path)?;
        Ok(Self {
            path,
            values: Arc::new(Mutex::new(values)),
        })
    }

    pub fn default_path() -> PathBuf {
        std::env::var_os("LOOM_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .map(|path| path.join("loom"))
            })
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|path| path.join(".config").join("loom"))
            })
            .unwrap_or_else(|| PathBuf::from(".loom"))
            .join("credentials.json")
    }

    pub fn insert(
        &self,
        reference: impl Into<CredentialRef>,
        secret: impl Into<String>,
    ) -> Result<()> {
        let mut values = self.values.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "credential store lock was poisoned",
                true,
            )
        })?;
        values.insert(reference.into().0, secret.into());
        self.persist(&values)
    }

    pub fn remove(&self, reference: &CredentialRef) -> Result<bool> {
        let mut values = self.values.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "credential store lock was poisoned",
                true,
            )
        })?;
        let removed = values.remove(reference.as_str()).is_some();
        if removed {
            self.persist(&values)?;
        }
        Ok(removed)
    }

    fn persist(&self, values: &BTreeMap<String, String>) -> Result<()> {
        let parent = self.path.parent().ok_or_else(|| {
            LoomError::invalid_request("credential store path must have a parent directory")
        })?;
        fs::create_dir_all(parent).map_err(|error| credential_store_error(&self.path, error))?;
        let temporary = self.path.with_extension("json.tmp");
        let bytes = serde_json::to_vec(values).map_err(|error| {
            LoomError::new(
                ErrorCode::Internal,
                format!("could not encode credential store: {error}"),
                false,
            )
        })?;
        let mut file = fs::OpenOptions::new();
        file.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;

            file.mode(0o600);
        }
        file.open(&temporary)
            .and_then(|mut file| file.write_all(&bytes))
            .map_err(|error| credential_store_error(&temporary, error))?;
        restrict_file_permissions(&temporary)?;
        fs::rename(&temporary, &self.path)
            .map_err(|error| credential_store_error(&self.path, error))?;
        restrict_file_permissions(&self.path)?;
        Ok(())
    }

    fn read_values(path: &Path) -> Result<BTreeMap<String, String>> {
        if !path.is_file() {
            return Ok(BTreeMap::new());
        }
        let bytes = fs::read(path).map_err(|error| credential_store_error(path, error))?;
        serde_json::from_slice(&bytes).map_err(|error| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                format!(
                    "credential store '{}' contains invalid JSON: {error}",
                    path.display()
                ),
                false,
            )
        })
    }
}

impl CredentialStore for FileCredentialStore {
    fn resolve(&self, reference: &CredentialRef) -> Result<String> {
        let mut values = self.values.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "credential store lock was poisoned",
                true,
            )
        })?;
        *values = Self::read_values(&self.path)?;
        values.get(reference.as_str()).cloned().ok_or_else(|| {
            LoomError::new(
                ErrorCode::ProviderAuthentication,
                format!(
                    "credential reference '{}' was not found",
                    reference.as_str()
                ),
                false,
            )
        })
    }

    fn store(&self, reference: &CredentialRef, secret: String) -> Result<()> {
        self.insert(reference.clone(), secret)
    }
}

fn credential_store_error(path: &Path, error: impl fmt::Display) -> LoomError {
    LoomError::new(
        ErrorCode::Internal,
        format!(
            "could not access credential store '{}': {error}",
            path.display()
        ),
        false,
    )
}

fn restrict_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| credential_store_error(path, error))?;
    }
    Ok(())
}

#[derive(Clone, Default)]
pub struct InMemoryCredentialStore {
    values: Arc<Mutex<BTreeMap<String, String>>>,
}

impl fmt::Debug for InMemoryCredentialStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InMemoryCredentialStore")
            .field(
                "credential_count",
                &self.values.lock().map(|values| values.len()).unwrap_or(0),
            )
            .finish()
    }
}

impl InMemoryCredentialStore {
    pub fn insert(&self, reference: impl Into<CredentialRef>, secret: impl Into<String>) {
        if let Ok(mut values) = self.values.lock() {
            values.insert(reference.into().0, secret.into());
        }
    }

    pub fn remove(&self, reference: &CredentialRef) {
        if let Ok(mut values) = self.values.lock() {
            values.remove(reference.as_str());
        }
    }
}

impl CredentialStore for InMemoryCredentialStore {
    fn resolve(&self, reference: &CredentialRef) -> Result<String> {
        self.values
            .lock()
            .map_err(|_| {
                LoomError::new(
                    ErrorCode::Internal,
                    "credential store lock was poisoned",
                    true,
                )
            })?
            .get(reference.as_str())
            .cloned()
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::ProviderAuthentication,
                    format!(
                        "credential reference '{}' was not found",
                        reference.as_str()
                    ),
                    false,
                )
            })
    }

    fn store(&self, reference: &CredentialRef, secret: String) -> Result<()> {
        self.values
            .lock()
            .map_err(|_| {
                LoomError::new(
                    ErrorCode::Internal,
                    "credential store lock was poisoned",
                    true,
                )
            })?
            .insert(reference.as_str().to_owned(), secret);
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderConfig {
    pub id: ProviderId,
    pub kind: ProviderKind,
    pub display_name: String,
    pub endpoint: Option<String>,
    pub models: Vec<ModelDescriptor>,
    pub credential: Option<CredentialRef>,
    pub input_cost_micros_per_1k: u64,
    pub output_cost_micros_per_1k: u64,
}

impl ProviderConfig {
    pub fn deterministic() -> Self {
        Self {
            id: ProviderId::new("deterministic"),
            kind: ProviderKind::Deterministic,
            display_name: "Deterministic test provider".to_owned(),
            endpoint: None,
            models: vec![deterministic_descriptor()],
            credential: None,
            input_cost_micros_per_1k: 0,
            output_cost_micros_per_1k: 0,
        }
    }

    pub fn openai_compatible(
        id: impl Into<ProviderId>,
        display_name: impl Into<String>,
        endpoint: impl Into<String>,
        model: ModelDescriptor,
        credential: Option<CredentialRef>,
    ) -> Self {
        Self {
            id: id.into(),
            kind: ProviderKind::OpenAiCompatible,
            display_name: display_name.into(),
            endpoint: Some(endpoint.into()),
            models: vec![model],
            credential,
            input_cost_micros_per_1k: 0,
            output_cost_micros_per_1k: 0,
        }
    }

    pub fn ollama(endpoint: impl Into<String>, model: impl Into<ModelId>) -> Self {
        let model = model.into();
        let model_descriptor = ModelDescriptor {
            id: model,
            provider: ProviderId::new("ollama"),
            display_name: "Ollama local model".to_owned(),
            context_window: Some(32_768),
            capabilities: ModelCapabilities {
                streaming: false,
                tool_calling: true,
                vision: false,
                json_mode: true,
            },
        };
        Self {
            id: ProviderId::new("ollama"),
            kind: ProviderKind::Ollama,
            display_name: "Ollama local runtime".to_owned(),
            endpoint: Some(endpoint.into()),
            models: vec![model_descriptor],
            credential: None,
            input_cost_micros_per_1k: 0,
            output_cost_micros_per_1k: 0,
        }
    }

    pub fn github_copilot(credential: impl Into<CredentialRef>) -> Self {
        Self {
            id: ProviderId::new(GITHUB_COPILOT_PROVIDER_ID),
            kind: ProviderKind::GitHubCopilot,
            display_name: "GitHub Copilot".to_owned(),
            endpoint: Some(GITHUB_COPILOT_API_ENDPOINT.to_owned()),
            models: vec![github_copilot_descriptor()],
            credential: Some(credential.into()),
            input_cost_micros_per_1k: 0,
            output_cost_micros_per_1k: 0,
        }
    }

    pub fn with_pricing(
        mut self,
        input_cost_micros_per_1k: u64,
        output_cost_micros_per_1k: u64,
    ) -> Self {
        self.input_cost_micros_per_1k = input_cost_micros_per_1k;
        self.output_cost_micros_per_1k = output_cost_micros_per_1k;
        self
    }
}

pub fn github_copilot_descriptor() -> ModelDescriptor {
    ModelDescriptor {
        id: ModelId::new(GITHUB_COPILOT_DEFAULT_MODEL),
        provider: ProviderId::new(GITHUB_COPILOT_PROVIDER_ID),
        display_name: "GitHub Copilot GPT-5.6 Luna".to_owned(),
        context_window: Some(128_000),
        capabilities: ModelCapabilities {
            streaming: false,
            tool_calling: true,
            vision: true,
            json_mode: true,
        },
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ProviderUsageKey {
    pub provider: ProviderId,
    pub model: ModelId,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct UsageLedger {
    #[serde(with = "usage_aggregate_entries")]
    pub aggregates: BTreeMap<ProviderUsageKey, ProviderUsageSummary>,
}

mod usage_aggregate_entries {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::{ProviderUsageKey, ProviderUsageSummary};

    pub fn serialize<S>(
        aggregates: &BTreeMap<ProviderUsageKey, ProviderUsageSummary>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        aggregates.iter().collect::<Vec<_>>().serialize(serializer)
    }

    pub fn deserialize<'de, D>(
        deserializer: D,
    ) -> Result<BTreeMap<ProviderUsageKey, ProviderUsageSummary>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let entries = Vec::<(ProviderUsageKey, ProviderUsageSummary)>::deserialize(deserializer)?;
        Ok(entries.into_iter().collect())
    }
}

impl UsageLedger {
    pub fn record(
        &mut self,
        provider: ProviderId,
        model: ModelId,
        usage: TokenUsage,
        cost_micros: u64,
    ) {
        let summary = self
            .aggregates
            .entry(ProviderUsageKey { provider, model })
            .or_default();
        summary.requests = summary.requests.saturating_add(1);
        summary.input_tokens = summary.input_tokens.saturating_add(usage.input_tokens);
        summary.output_tokens = summary.output_tokens.saturating_add(usage.output_tokens);
        summary.cached_input_tokens = summary
            .cached_input_tokens
            .saturating_add(usage.cached_input_tokens);
        summary.cost_micros = summary.cost_micros.saturating_add(cost_micros);
    }

    pub fn summary(
        &self,
        provider: Option<&ProviderId>,
        model: Option<&ModelId>,
    ) -> ProviderUsageSummary {
        self.aggregates
            .iter()
            .filter(|(key, _)| {
                provider.is_none_or(|provider| &key.provider == provider)
                    && model.is_none_or(|model| &key.model == model)
            })
            .fold(
                ProviderUsageSummary::default(),
                |mut total, (_, summary)| {
                    total.requests = total.requests.saturating_add(summary.requests);
                    total.input_tokens = total.input_tokens.saturating_add(summary.input_tokens);
                    total.output_tokens = total.output_tokens.saturating_add(summary.output_tokens);
                    total.cached_input_tokens = total
                        .cached_input_tokens
                        .saturating_add(summary.cached_input_tokens);
                    total.cost_micros = total.cost_micros.saturating_add(summary.cost_micros);
                    total
                },
            )
    }

    pub fn session_usage(&self) -> UsageSnapshot {
        let summary = self.summary(None, None);
        let mut usage = UsageSnapshot::default();
        usage.add_tokens(
            summary.input_tokens,
            summary.output_tokens,
            summary.cached_input_tokens,
        );
        usage.add_cost_micros(summary.cost_micros);
        usage
    }
}

#[derive(Clone)]
pub struct ProviderRegistry {
    configurations: Arc<Mutex<BTreeMap<ProviderId, ProviderConfig>>>,
    credentials: Arc<dyn CredentialStore>,
    api_key_credentials: Arc<Mutex<Option<Arc<dyn CredentialStore>>>>,
    health: Arc<Mutex<BTreeMap<ProviderId, ProviderHealth>>>,
    usage: Arc<Mutex<UsageLedger>>,
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

    fn resolve_credential(&self, reference: &CredentialRef) -> Result<String> {
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
            if config.kind != ProviderKind::OpenAiCompatible {
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

    fn build_demo(credentials: Arc<dyn CredentialStore>, include_github_copilot: bool) -> Self {
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
        if config.kind != ProviderKind::OpenAiCompatible {
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
                api_key_configurable: config.kind == ProviderKind::OpenAiCompatible,
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

    fn is_configured(&self, config: &ProviderConfig) -> bool {
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
        let request = configure_request(ureq::get(&health_endpoint(endpoint)));
        let request = if credential.is_empty() {
            request
        } else {
            request.header("Authorization", format!("Bearer {credential}"))
        };
        let response = request
            .call()
            .map_err(|error| normalize_provider_request_error(provider_id.as_str(), error))?;
        let mut response = ensure_success(provider_id.as_str(), response)?;
        let body: serde_json::Value = response.body_mut().read_json().map_err(|error| {
            LoomError::new(
                ErrorCode::ProviderInvalidResponse,
                format!(
                    "{} model discovery returned invalid JSON: {error}",
                    provider_id.as_str()
                ),
                false,
            )
        })?;
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

            ProviderKind::Ollama => {
                let endpoint = config.endpoint.as_deref().ok_or_else(|| {
                    LoomError::invalid_request(format!(
                        "provider '{}' has no endpoint",
                        config.id.as_str()
                    ))
                })?;
                Box::new(OllamaProvider::with_descriptor(endpoint, descriptor))
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

    fn create_github_copilot_provider(
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
                    && (config.kind != ProviderKind::OpenAiCompatible
                        || config.credential.is_none())
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

struct AccountingProvider {
    inner: Box<dyn ModelProvider>,
    ledger: Arc<Mutex<UsageLedger>>,
    input_cost_micros_per_1k: u64,
    output_cost_micros_per_1k: u64,
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

pub struct DeterministicProvider {
    descriptor: ModelDescriptor,
    steps: Vec<Vec<ModelStreamEvent>>,
    cursor: usize,
}

impl DeterministicProvider {
    pub fn demo() -> Self {
        Self {
            descriptor: deterministic_descriptor(),
            steps: vec![
                vec![
                    ModelStreamEvent::TextDelta {
                        text: "I'll inspect the workspace before making changes.\n".to_owned(),
                    },
                    ModelStreamEvent::ToolCallDelta {
                        call: ToolCall {
                            id: ToolCallId::new(),
                            name: "list_files".to_owned(),
                            arguments: serde_json::json!({"path": "."}),
                        },
                    },
                    ModelStreamEvent::Completed {
                        reason: FinishReason::ToolCall,
                    },
                ],
                vec![
                    ModelStreamEvent::TextDelta {
                        text: "I found the workspace. I'll apply a focused change next.\n"
                            .to_owned(),
                    },
                    ModelStreamEvent::ToolCallDelta {
                        call: ToolCall {
                            id: ToolCallId::new(),
                            name: "apply_patch".to_owned(),
                            arguments: serde_json::json!({
                                "path": "loom-m1-demo.txt",
                                "old_text": "",
                                "new_text": "Loom M1 deterministic demo\n"
                            }),
                        },
                    },
                    ModelStreamEvent::Completed {
                        reason: FinishReason::ToolCall,
                    },
                ],
                vec![
                    ModelStreamEvent::TextDelta {
                        text: "The change is applied. I'll run validation now.\n".to_owned(),
                    },
                    ModelStreamEvent::ToolCallDelta {
                        call: ToolCall {
                            id: ToolCallId::new(),
                            name: "run_command".to_owned(),
                            arguments: serde_json::json!({
                                "command": "rustc",
                                "args": ["--version"]
                            }),
                        },
                    },
                    ModelStreamEvent::Completed {
                        reason: FinishReason::ToolCall,
                    },
                ],
                vec![
                    ModelStreamEvent::TextDelta {
                        text: "The task is complete: the workspace change was applied and validation ran successfully."
                            .to_owned(),
                    },
                    ModelStreamEvent::Usage {
                        usage: TokenUsage {
                            input_tokens: 240,
                            output_tokens: 52,
                            cached_input_tokens: 0,
                        },
                    },
                    ModelStreamEvent::Completed {
                        reason: FinishReason::Stop,
                    },
                ],
            ],
            cursor: 0,
        }
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn with_cursor(mut self, cursor: usize) -> Self {
        self.cursor = cursor.min(self.steps.len());
        self
    }
}

impl ModelProvider for DeterministicProvider {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn stream(
        &mut self,
        _request: &ModelRequest,
        cancel: &CancellationToken,
        sink: &mut dyn ModelStreamSink,
    ) -> Result<()> {
        let events = self.steps.get(self.cursor).cloned().unwrap_or_else(|| {
            vec![ModelStreamEvent::Completed {
                reason: FinishReason::Stop,
            }]
        });
        self.cursor = self.cursor.saturating_add(1);
        for event in events {
            cancel.check()?;
            if sink.emit(event)? == StreamFlow::Stop {
                break;
            }
        }
        Ok(())
    }

    fn reset(&mut self) {
        self.cursor = 0;
    }
}

pub fn deterministic_descriptor() -> ModelDescriptor {
    ModelDescriptor {
        id: ModelId::new("deterministic/demo"),
        provider: ProviderId::new("deterministic"),
        display_name: "Deterministic M1 demo".to_owned(),
        context_window: Some(16_384),
        capabilities: ModelCapabilities {
            streaming: true,
            tool_calling: true,
            vision: false,
            json_mode: true,
        },
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct GitHubDeviceCode {
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
    device_code: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct OAuthTokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

pub struct GitHubCopilotAuthenticator {
    client_id: String,
}

impl Default for GitHubCopilotAuthenticator {
    fn default() -> Self {
        Self {
            client_id: GITHUB_OAUTH_CLIENT_ID.to_owned(),
        }
    }
}

impl GitHubCopilotAuthenticator {
    pub fn begin(&self) -> Result<GitHubDeviceCode> {
        let response = configure_request(ureq::post(GITHUB_DEVICE_CODE_URL))
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .send_json(serde_json::json!({
                "client_id": self.client_id.as_str(),
                "scope": "read:user repo"
            }))
            .map_err(|error| normalize_oauth_error("GitHub device authorization", error))?;
        let mut response = ensure_success("GitHub device authorization", response)?;
        response.body_mut().read_json().map_err(|error| {
            LoomError::new(
                ErrorCode::ProviderInvalidResponse,
                format!("GitHub device authorization returned invalid JSON: {error}"),
                false,
            )
        })
    }

    pub fn poll(&self, device: &GitHubDeviceCode) -> Result<String> {
        let deadline = Instant::now() + Duration::from_secs(device.expires_in);
        let mut interval = Duration::from_secs(device.interval.max(1));
        loop {
            if Instant::now() >= deadline {
                return Err(LoomError::new(
                    ErrorCode::ProviderAuthentication,
                    "GitHub device authorization expired",
                    false,
                ));
            }
            let response = configure_request(ureq::post(GITHUB_ACCESS_TOKEN_URL))
                .header("Accept", "application/json")
                .header("Content-Type", "application/json")
                .send_json(serde_json::json!({
                    "client_id": self.client_id.as_str(),
                    "device_code": device.device_code.as_str(),
                    "grant_type": "urn:ietf:params:oauth:grant-type:device_code"
                }));
            match response {
                Ok(mut response) if response.status().as_u16() == 400 => {
                    let body: OAuthTokenResponse =
                        response.body_mut().read_json().unwrap_or_default();
                    match body.error.as_deref() {
                        Some("authorization_pending") => thread::sleep(interval),
                        Some("slow_down") => {
                            interval = interval.saturating_add(Duration::from_secs(5));
                            thread::sleep(interval);
                        }
                        _ => return Err(oauth_response_error(body)),
                    }
                }
                Ok(mut response) => {
                    let body: OAuthTokenResponse =
                        response.body_mut().read_json().map_err(|error| {
                            LoomError::new(
                                ErrorCode::ProviderInvalidResponse,
                                format!("GitHub token response was invalid JSON: {error}"),
                                false,
                            )
                        })?;
                    if let Some(token) =
                        body.access_token.as_ref().filter(|token| !token.is_empty())
                    {
                        return Ok(token.clone());
                    }
                    match body.error.as_deref() {
                        Some("authorization_pending") => thread::sleep(interval),
                        Some("slow_down") => {
                            interval = interval.saturating_add(Duration::from_secs(5));
                            thread::sleep(interval);
                        }
                        _ => return Err(oauth_response_error(body)),
                    }
                }
                Err(error) => {
                    return Err(normalize_oauth_error("GitHub token exchange", error));
                }
            }
        }
    }
}

pub struct GitHubCopilotProvider {
    api_endpoint: String,
    token_endpoint: String,
    github_token: String,
    descriptor: ModelDescriptor,
    responses_call_ids: BTreeMap<String, ToolCallId>,
}

impl GitHubCopilotProvider {
    pub fn new(github_token: impl Into<String>, model: impl Into<ModelId>) -> Self {
        let model = model.into();
        Self::with_descriptor(
            GITHUB_COPILOT_API_ENDPOINT,
            github_token,
            ModelDescriptor {
                id: model,
                provider: ProviderId::new(GITHUB_COPILOT_PROVIDER_ID),
                display_name: "GitHub Copilot model".to_owned(),
                context_window: Some(128_000),
                capabilities: ModelCapabilities {
                    streaming: false,
                    tool_calling: true,
                    vision: true,
                    json_mode: true,
                },
            },
        )
    }

    pub fn with_descriptor(
        api_endpoint: impl Into<String>,
        github_token: impl Into<String>,
        descriptor: ModelDescriptor,
    ) -> Self {
        Self::with_endpoints(
            api_endpoint,
            GITHUB_COPILOT_TOKEN_URL,
            github_token,
            descriptor,
        )
    }

    pub fn with_endpoints(
        api_endpoint: impl Into<String>,
        token_endpoint: impl Into<String>,
        github_token: impl Into<String>,
        descriptor: ModelDescriptor,
    ) -> Self {
        Self {
            api_endpoint: api_endpoint.into(),
            token_endpoint: token_endpoint.into(),
            github_token: github_token.into(),
            descriptor,
            responses_call_ids: BTreeMap::new(),
        }
    }

    pub fn with_model_descriptor(mut self, descriptor: ModelDescriptor) -> Self {
        self.descriptor = descriptor;
        self
    }

    pub fn discover_models(&self) -> Result<Vec<ModelDescriptor>> {
        let token = self.fetch_copilot_token()?;
        let response = configure_request(ureq::get(&format!(
            "{}/models",
            trim_endpoint(&token.api_endpoint)
        )))
        .header("Accept", "application/json")
        .header("Authorization", &format!("Bearer {}", token.value))
        .header("Editor-Version", GITHUB_COPILOT_EDITOR_VERSION)
        .header("Editor-Plugin-Version", GITHUB_COPILOT_PLUGIN_VERSION)
        .header("Copilot-Integration-Id", "vscode-chat")
        .header("Openai-Intent", "conversation-panel")
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .header("X-Vscode-User-Agent-Library-Version", "electron-fetch")
        .header("User-Agent", GITHUB_COPILOT_USER_AGENT)
        .call()
        .map_err(|error| {
            normalize_provider_request_error("github-copilot model discovery", error)
        })?;
        let mut response = ensure_success("github-copilot model discovery", response)?;
        let body: serde_json::Value = response.body_mut().read_json().map_err(|error| {
            LoomError::new(
                ErrorCode::ProviderInvalidResponse,
                format!("GitHub Copilot model discovery returned invalid JSON: {error}"),
                false,
            )
        })?;
        let models = body
            .get("data")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::ProviderInvalidResponse,
                    "GitHub Copilot model discovery did not contain a data array",
                    false,
                )
            })?
            .iter()
            .map(|model| {
                let id = model
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        LoomError::new(
                            ErrorCode::ProviderInvalidResponse,
                            "GitHub Copilot returned a model without an id",
                            false,
                        )
                    })?;
                Ok((
                    ModelDescriptor {
                        id: ModelId::new(id),
                        provider: self.descriptor.provider.clone(),
                        display_name: id.to_owned(),
                        context_window: discovered_context_window(model).or_else(|| {
                            (self.descriptor.id.as_str() == id)
                                .then_some(self.descriptor.context_window)
                                .flatten()
                        }),
                        capabilities: self.descriptor.capabilities.clone(),
                    },
                    github_copilot_supports_tool_calls(model),
                ))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter_map(|(model, supports_tool_calls)| supports_tool_calls.then_some(model))
            .collect::<Vec<_>>();
        if models.is_empty() {
            return Err(LoomError::new(
                ErrorCode::ProviderInvalidResponse,
                "GitHub Copilot model discovery returned no agentic models",
                false,
            ));
        }
        Ok(models)
    }

    fn fetch_copilot_token(&self) -> Result<CopilotAccessToken> {
        let response = configure_request(ureq::get(&self.token_endpoint))
            .header("Accept", "application/json")
            .header("Authorization", format!("token {}", self.github_token))
            .header("Editor-Version", GITHUB_COPILOT_EDITOR_VERSION)
            .header("Editor-Plugin-Version", GITHUB_COPILOT_PLUGIN_VERSION)
            .header("Copilot-Integration-Id", "vscode-chat")
            .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
            .header("X-Vscode-User-Agent-Library-Version", "electron-fetch")
            .header("User-Agent", GITHUB_COPILOT_USER_AGENT)
            .call()
            .map_err(|error| {
                normalize_provider_request_error("github-copilot token exchange", error)
            })?;
        let mut response = ensure_success("github-copilot token exchange", response)?;
        let body: serde_json::Value = response.body_mut().read_json().map_err(|error| {
            LoomError::new(
                ErrorCode::ProviderInvalidResponse,
                format!("GitHub Copilot token response was invalid JSON: {error}"),
                false,
            )
        })?;
        let value = body
            .get("token")
            .and_then(serde_json::Value::as_str)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::ProviderAuthentication,
                    "GitHub Copilot token response did not contain a token",
                    false,
                )
            })?;
        let api_endpoint = body
            .get("endpoints")
            .and_then(|endpoints| endpoints.get("api"))
            .and_then(serde_json::Value::as_str)
            .filter(|endpoint| !endpoint.is_empty())
            .map_or_else(|| self.api_endpoint.clone(), ToOwned::to_owned);
        Ok(CopilotAccessToken {
            value: value.to_owned(),
            api_endpoint,
        })
    }
}

struct CopilotAccessToken {
    value: String,
    api_endpoint: String,
}

// Copilot exposes per-model limits under capabilities.limits. Some compatible
// catalogs expose context_length directly. A prompt cap is also a safe upper
// bound on our usable window, since the runtime separately reserves output.
fn discovered_context_window(model: &serde_json::Value) -> Option<u32> {
    [
        "/capabilities/limits/max_context_window_tokens",
        "/capabilities/limits/max_prompt_tokens",
        "/context_length",
    ]
    .iter()
    .filter_map(|path| model.pointer(path).and_then(serde_json::Value::as_u64))
    .filter_map(|value| u32::try_from(value).ok())
    .filter(|value| *value > 0)
    .min()
}

fn github_copilot_supports_tool_calls(model: &serde_json::Value) -> bool {
    model
        .get("capabilities")
        .and_then(|capabilities| capabilities.get("supports"))
        .and_then(|supports| supports.get("tool_calls"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

impl ModelProvider for GitHubCopilotProvider {
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
        cancel.check()?;
        let token = self.fetch_copilot_token()?;
        let responses_api = uses_responses_endpoint(request.model.as_str());
        let (endpoint, payload, provider) = if responses_api {
            (
                format!("{}/responses", trim_endpoint(&token.api_endpoint)),
                responses_request_payload(request),
                "github-copilot responses",
            )
        } else {
            (
                format!("{}/chat/completions", trim_endpoint(&token.api_endpoint)),
                openai_request_payload(request),
                "github-copilot chat completion",
            )
        };
        send_openai_request(
            &endpoint,
            &bearer_header(&token.value),
            &[
                ("Editor-Version", GITHUB_COPILOT_EDITOR_VERSION),
                ("Editor-Plugin-Version", GITHUB_COPILOT_PLUGIN_VERSION),
                ("Openai-Intent", "conversation-panel"),
                ("Copilot-Integration-Id", "vscode-chat"),
                ("X-GitHub-Api-Version", GITHUB_API_VERSION),
                ("X-Vscode-User-Agent-Library-Version", "electron-fetch"),
                ("User-Agent", GITHUB_COPILOT_USER_AGENT),
            ],
            payload,
            provider,
            if responses_api {
                Some(&mut self.responses_call_ids)
            } else {
                None
            },
            cancel,
            sink,
        )
    }

    fn health_check(&mut self) -> Result<()> {
        self.fetch_copilot_token().map(|_| ())
    }
}

pub struct OpenAiCompatibleProvider {
    endpoint: String,
    api_key: String,
    descriptor: ModelDescriptor,
}

impl OpenAiCompatibleProvider {
    pub fn new(
        endpoint: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<ModelId>,
    ) -> Self {
        let model = model.into();
        Self::with_descriptor(
            endpoint,
            api_key,
            ModelDescriptor {
                id: model,
                provider: ProviderId::new("openai-compatible"),
                display_name: "OpenAI-compatible model".to_owned(),
                context_window: None,
                capabilities: ModelCapabilities {
                    streaming: false,
                    tool_calling: true,
                    vision: false,
                    json_mode: true,
                },
            },
        )
    }

    pub fn with_descriptor(
        endpoint: impl Into<String>,
        api_key: impl Into<String>,
        descriptor: ModelDescriptor,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            api_key: api_key.into(),
            descriptor,
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

impl ModelProvider for OpenAiCompatibleProvider {
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
        cancel.check()?;
        let mut payload = serde_json::json!({
            "model": request.model.as_str(),
            "messages": request.messages.iter().map(message_json).collect::<Vec<_>>(),
            "stream": false
        });
        if !request.tools.is_empty() {
            payload["tools"] = serde_json::Value::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        serde_json::json!({
                            "type": "function",
                            "function": {
                                "name": tool.name,
                                "description": tool.description,
                                "parameters": tool.input_schema
                            }
                        })
                    })
                    .collect(),
            );
        }
        if let Some(temperature) = request.options.temperature {
            payload["temperature"] = serde_json::json!(temperature);
        }
        if let Some(max_output_tokens) = request.options.max_output_tokens {
            payload["max_tokens"] = serde_json::json!(max_output_tokens);
        }
        if !request.options.stop_sequences.is_empty() {
            payload["stop"] = serde_json::json!(request.options.stop_sequences);
        }

        let authorization = if self.api_key.is_empty() {
            String::new()
        } else {
            bearer_header(&self.api_key)
        };
        send_openai_request(
            &self.endpoint,
            &authorization,
            &[],
            payload,
            self.descriptor.provider.as_str(),
            None,
            cancel,
            sink,
        )
    }

    fn health_check(&mut self) -> Result<()> {
        let response = configure_request(ureq::get(&health_endpoint(&self.endpoint))).call();
        match response {
            Ok(response) => ensure_success(self.descriptor.provider.as_str(), response).map(|_| ()),
            Err(error) => Err(normalize_provider_request_error(
                self.descriptor.provider.as_str(),
                error,
            )),
        }
    }
}

pub struct OllamaProvider {
    inner: OpenAiCompatibleProvider,
}

impl OllamaProvider {
    pub fn new(endpoint: impl Into<String>, model: impl Into<ModelId>) -> Self {
        let model = model.into();
        let descriptor = ModelDescriptor {
            id: model,
            provider: ProviderId::new("ollama"),
            display_name: "Ollama local model".to_owned(),
            context_window: Some(32_768),
            capabilities: ModelCapabilities {
                streaming: false,
                tool_calling: true,
                vision: false,
                json_mode: true,
            },
        };
        Self::with_descriptor(endpoint, descriptor)
    }

    pub fn with_descriptor(endpoint: impl Into<String>, descriptor: ModelDescriptor) -> Self {
        let endpoint = ollama_chat_endpoint(endpoint.into());
        Self {
            inner: OpenAiCompatibleProvider::with_descriptor(endpoint, "", descriptor),
        }
    }

    pub fn endpoint(&self) -> &str {
        self.inner.endpoint()
    }
}

impl ModelProvider for OllamaProvider {
    fn descriptor(&self) -> &ModelDescriptor {
        self.inner.descriptor()
    }

    fn stream(
        &mut self,
        request: &ModelRequest,
        cancel: &CancellationToken,
        sink: &mut dyn ModelStreamSink,
    ) -> Result<()> {
        self.inner.stream(request, cancel, sink)
    }

    fn count_tokens(&self, request: &ModelRequest) -> u64 {
        self.inner.count_tokens(request)
    }

    fn health_check(&mut self) -> Result<()> {
        self.inner.health_check()
    }
}

fn openai_request_payload(request: &ModelRequest) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "model": request.model.as_str(),
        "messages": request.messages.iter().map(message_json).collect::<Vec<_>>(),
        "stream": false
    });
    if !request.tools.is_empty() {
        payload["tools"] = serde_json::Value::Array(
            request
                .tools
                .iter()
                .map(|tool| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.input_schema
                        }
                    })
                })
                .collect(),
        );
    }
    if let Some(temperature) = request.options.temperature {
        payload["temperature"] = serde_json::json!(temperature);
    }
    if let Some(max_output_tokens) = request.options.max_output_tokens {
        payload["max_tokens"] = serde_json::json!(max_output_tokens);
    }
    if !request.options.stop_sequences.is_empty() {
        payload["stop"] = serde_json::json!(request.options.stop_sequences);
    }
    payload
}

fn uses_responses_endpoint(model: &str) -> bool {
    model.starts_with("gpt-5") || model.starts_with("gpt-6")
}

fn responses_request_payload(request: &ModelRequest) -> serde_json::Value {
    let input = request
        .messages
        .iter()
        .flat_map(|message| {
            if message.role == MessageRole::Tool {
                vec![serde_json::json!({
                    "type": "function_call_output",
                    "call_id": message
                        .tool_call_id
                        .map(|id| id.to_string())
                        .unwrap_or_default(),
                    "output": message.content
                })]
            } else if !message.tool_calls.is_empty() {
                message
                    .tool_calls
                    .iter()
                    .map(|call| {
                        serde_json::json!({
                                "type": "function_call",
                                "call_id": call.id.to_string(),
                                "name": call.name,
                                "arguments": call.arguments.to_string()
                        })
                    })
                    .collect::<Vec<_>>()
            } else {
                let content_type = if message.role == MessageRole::Assistant {
                    "output_text"
                } else {
                    "input_text"
                };
                vec![serde_json::json!({
                    "role": match message.role {
                        MessageRole::System => "system",
                        MessageRole::User => "user",
                        MessageRole::Assistant => "assistant",
                        MessageRole::Tool => unreachable!(),
                    },
                    "content": [{
                        "type": content_type,
                        "text": message.content
                    }]
                })]
            }
        })
        .collect::<Vec<_>>();
    let mut payload = serde_json::json!({
        "model": request.model.as_str(),
        "input": input,
        "stream": false
    });
    if !request.tools.is_empty() {
        payload["tools"] = serde_json::Value::Array(
            request
                .tools
                .iter()
                .map(|tool| {
                    serde_json::json!({
                        "type": "function",
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.input_schema
                    })
                })
                .collect(),
        );
    }
    if let Some(max_output_tokens) = request.options.max_output_tokens {
        payload["max_output_tokens"] = serde_json::json!(max_output_tokens);
    }
    payload
}

/// Sends an OpenAI-shaped request and forwards every decoded event to `sink`.
///
/// The request asks for server-sent events. When the endpoint honours that, the
/// body is decoded chunk by chunk and text reaches the sink while the completion
/// is still being produced; the cancellation token is checked between chunks so
/// an interrupt does not have to wait for the completion. When the endpoint
/// answers with a complete JSON document instead (declared by its content type),
/// the document is normalized and emitted in one pass.
#[allow(clippy::too_many_arguments)]
fn send_openai_request(
    endpoint: &str,
    authorization: &str,
    headers: &[(&str, &str)],
    mut payload: serde_json::Value,
    provider: &str,
    call_ids: Option<&mut BTreeMap<String, ToolCallId>>,
    cancel: &CancellationToken,
    sink: &mut dyn ModelStreamSink,
) -> Result<()> {
    let responses_api = provider.contains("responses");
    payload["stream"] = serde_json::Value::Bool(true);
    if !responses_api {
        payload["stream_options"] = serde_json::json!({"include_usage": true});
    }
    let request = configure_request(ureq::post(endpoint))
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream");
    let request = if authorization.is_empty() {
        request
    } else {
        request.header("Authorization", authorization)
    };
    let request = headers.iter().fold(request, |request, (name, value)| {
        request.header(*name, *value)
    });
    cancel.check()?;
    let response = request
        .send_json(payload)
        .map_err(|error| normalize_provider_request_error(provider, error))?;
    let mut response = ensure_success(provider, response)?;
    let event_stream = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("text/event-stream"));
    let mut own_call_ids = BTreeMap::new();
    let call_ids = call_ids.unwrap_or(&mut own_call_ids);
    if !event_stream {
        let mut response = response;
        let body: serde_json::Value = response.body_mut().read_json().map_err(|error| {
            LoomError::new(
                ErrorCode::ProviderInvalidResponse,
                format!("{provider} returned a response that was not valid JSON: {error}"),
                false,
            )
        })?;
        let events = if responses_api {
            normalize_responses_response(&body, call_ids)?
        } else {
            normalize_openai_response(&body)?
        };
        for event in events {
            cancel.check()?;
            if sink.emit(event)? == StreamFlow::Stop {
                break;
            }
        }
        return Ok(());
    }
    let reader = BufReader::new(response.body_mut().as_reader());
    let mut decoder = if responses_api {
        StreamDecoder::responses(provider.to_owned())
    } else {
        StreamDecoder::chat_completions(provider.to_owned())
    };
    for line in reader.lines() {
        cancel.check()?;
        let line = line.map_err(|error| {
            normalize_transport_error(provider, &format!("event stream read failed: {error}"))
        })?;
        let Some(payload) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload.is_empty() {
            continue;
        }
        if payload == "[DONE]" {
            break;
        }
        let chunk: serde_json::Value = serde_json::from_str(payload).map_err(|error| {
            LoomError::new(
                ErrorCode::ProviderInvalidResponse,
                format!("{provider} sent an event that was not valid JSON: {error}"),
                false,
            )
        })?;
        if decoder.accept(&chunk, call_ids, sink)? == StreamFlow::Stop {
            return Ok(());
        }
    }
    decoder.finish(sink)
}

/// Incremental decoder for the two OpenAI-shaped event streams Loom speaks.
struct StreamDecoder {
    provider: String,
    responses_api: bool,
    tool_calls: BTreeMap<u64, PartialToolCall>,
    usage: Option<TokenUsage>,
    finish_reason: Option<FinishReason>,
    completed: bool,
}

#[derive(Clone, Debug, Default)]
struct PartialToolCall {
    name: String,
    arguments: String,
}

impl StreamDecoder {
    fn chat_completions(provider: String) -> Self {
        Self {
            provider,
            responses_api: false,
            tool_calls: BTreeMap::new(),
            usage: None,
            finish_reason: None,
            completed: false,
        }
    }

    fn responses(provider: String) -> Self {
        Self {
            provider,
            responses_api: true,
            tool_calls: BTreeMap::new(),
            usage: None,
            finish_reason: None,
            completed: false,
        }
    }

    fn accept(
        &mut self,
        chunk: &serde_json::Value,
        call_ids: &mut BTreeMap<String, ToolCallId>,
        sink: &mut dyn ModelStreamSink,
    ) -> Result<StreamFlow> {
        if self.responses_api {
            self.accept_responses_event(chunk, call_ids, sink)
        } else {
            self.accept_chat_chunk(chunk, sink)
        }
    }

    fn accept_chat_chunk(
        &mut self,
        chunk: &serde_json::Value,
        sink: &mut dyn ModelStreamSink,
    ) -> Result<StreamFlow> {
        if let Some(usage) = chunk.get("usage").filter(|usage| !usage.is_null()) {
            self.usage = Some(openai_usage(usage));
        }
        let Some(choice) = chunk
            .get("choices")
            .and_then(serde_json::Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return Ok(StreamFlow::Continue);
        };
        if let Some(reason) = choice
            .get("finish_reason")
            .and_then(serde_json::Value::as_str)
        {
            self.finish_reason = Some(finish_reason_from_str(reason));
        }
        let Some(delta) = choice.get("delta") else {
            return Ok(StreamFlow::Continue);
        };
        if let Some(fragments) = delta
            .get("tool_calls")
            .and_then(serde_json::Value::as_array)
        {
            for fragment in fragments {
                let index = fragment
                    .get("index")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default();
                let entry = self.tool_calls.entry(index).or_default();
                if let Some(function) = fragment.get("function") {
                    if let Some(name) = function.get("name").and_then(serde_json::Value::as_str) {
                        entry.name.push_str(name);
                    }
                    if let Some(arguments) = function
                        .get("arguments")
                        .and_then(serde_json::Value::as_str)
                    {
                        entry.arguments.push_str(arguments);
                    }
                }
            }
        }
        if let Some(text) = delta.get("content").and_then(serde_json::Value::as_str)
            && !text.is_empty()
        {
            return sink.emit(ModelStreamEvent::TextDelta {
                text: text.to_owned(),
            });
        }
        Ok(StreamFlow::Continue)
    }

    fn accept_responses_event(
        &mut self,
        chunk: &serde_json::Value,
        call_ids: &mut BTreeMap<String, ToolCallId>,
        sink: &mut dyn ModelStreamSink,
    ) -> Result<StreamFlow> {
        match chunk.get("type").and_then(serde_json::Value::as_str) {
            Some("response.output_text.delta") => {
                let text = chunk
                    .get("delta")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                if !text.is_empty() {
                    return sink.emit(ModelStreamEvent::TextDelta {
                        text: text.to_owned(),
                    });
                }
                Ok(StreamFlow::Continue)
            }
            Some("response.output_item.done") => {
                let Some(item) = chunk.get("item") else {
                    return Ok(StreamFlow::Continue);
                };
                if item.get("type").and_then(serde_json::Value::as_str) != Some("function_call") {
                    return Ok(StreamFlow::Continue);
                }
                let call = responses_function_call(item, call_ids)?;
                sink.emit(ModelStreamEvent::ToolCallDelta { call })
            }
            Some("response.completed" | "response.incomplete" | "response.failed") => {
                let response = chunk.get("response");
                if let Some(usage) = response
                    .and_then(|response| response.get("usage"))
                    .filter(|usage| !usage.is_null())
                {
                    self.usage = Some(responses_usage(usage));
                }
                let status = response
                    .and_then(|response| response.get("status"))
                    .and_then(serde_json::Value::as_str);
                self.finish_reason = Some(match status {
                    Some("completed") => FinishReason::Stop,
                    Some("incomplete") => {
                        let reason = response
                            .and_then(|response| response.get("incomplete_details"))
                            .and_then(|details| details.get("reason"))
                            .and_then(serde_json::Value::as_str);
                        log::error!(
                            "{} response incomplete: status={status:?}, reason={reason:?}",
                            self.provider
                        );
                        FinishReason::ErrorWithMessage {
                            message: reason.map_or_else(
                                || format!("{} response was incomplete", self.provider),
                                |reason| {
                                    format!("{} response was incomplete: {reason}", self.provider)
                                },
                            ),
                        }
                    }
                    _ => {
                        let error = response.and_then(|response| response.get("error"));
                        log::error!(
                            "{} response failed: status={status:?}, error={}",
                            self.provider,
                            error.map_or_else(
                                || "<missing>".to_owned(),
                                serde_json::Value::to_string
                            )
                        );
                        let message = error
                            .and_then(|error| error.get("message"))
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                            .unwrap_or_else(|| {
                                format!("{} request failed without further details", self.provider)
                            });
                        FinishReason::ErrorWithMessage {
                            message: format!("{} request failed: {message}", self.provider),
                        }
                    }
                });
                self.completed = true;
                Ok(StreamFlow::Continue)
            }
            Some("error") => Err(LoomError::new(
                ErrorCode::ProviderInvalidResponse,
                format!(
                    "{} reported a stream error: {}",
                    self.provider,
                    chunk
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("no detail")
                ),
                false,
            )),
            _ => Ok(StreamFlow::Continue),
        }
    }

    /// Emits the events that are only known once the stream ends.
    fn finish(self, sink: &mut dyn ModelStreamSink) -> Result<()> {
        let mut saw_tool_call = false;
        for (_, partial) in self.tool_calls {
            if partial.name.trim().is_empty() {
                return Err(LoomError::new(
                    ErrorCode::ProviderInvalidResponse,
                    format!("{} streamed a tool call without a name", self.provider),
                    false,
                ));
            }
            let arguments = parse_tool_arguments(&partial.arguments)?;
            saw_tool_call = true;
            if sink.emit(ModelStreamEvent::ToolCallDelta {
                call: ToolCall {
                    id: ToolCallId::new(),
                    name: partial.name,
                    arguments,
                },
            })? == StreamFlow::Stop
            {
                return Ok(());
            }
        }
        if let Some(usage) = self.usage
            && sink.emit(ModelStreamEvent::Usage { usage })? == StreamFlow::Stop
        {
            return Ok(());
        }
        let reason = self.finish_reason.unwrap_or(if saw_tool_call {
            FinishReason::ToolCall
        } else {
            FinishReason::Stop
        });
        sink.emit(ModelStreamEvent::Completed { reason })?;
        Ok(())
    }
}

fn parse_tool_arguments(arguments: &str) -> Result<serde_json::Value> {
    if arguments.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(arguments).map_err(|error| {
        LoomError::new(
            ErrorCode::ProviderInvalidResponse,
            format!("tool arguments were not valid JSON: {error}"),
            false,
        )
    })
}

fn finish_reason_from_str(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "tool_calls" | "function_call" => FinishReason::ToolCall,
        "length" => FinishReason::Length,
        "cancelled" => FinishReason::Cancelled,
        _ => FinishReason::Error,
    }
}

fn openai_usage(usage: &serde_json::Value) -> TokenUsage {
    TokenUsage {
        input_tokens: usage
            .get("prompt_tokens")
            .or_else(|| usage.get("input_tokens"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        output_tokens: usage
            .get("completion_tokens")
            .or_else(|| usage.get("output_tokens"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        cached_input_tokens: usage
            .get("prompt_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
    }
}

fn responses_usage(usage: &serde_json::Value) -> TokenUsage {
    TokenUsage {
        input_tokens: usage
            .get("input_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        output_tokens: usage
            .get("output_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        cached_input_tokens: usage
            .get("input_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
    }
}

fn responses_function_call(
    item: &serde_json::Value,
    call_ids: &mut BTreeMap<String, ToolCallId>,
) -> Result<ToolCall> {
    let name = item
        .get("name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            LoomError::new(
                ErrorCode::ProviderInvalidResponse,
                "Copilot Responses function call did not contain a name",
                false,
            )
        })?;
    let arguments = item
        .get("arguments")
        .and_then(serde_json::Value::as_str)
        .map(|arguments| {
            serde_json::from_str(arguments).map_err(|error| {
                LoomError::new(
                    ErrorCode::ProviderInvalidResponse,
                    format!("Copilot Responses tool arguments were invalid: {error}"),
                    false,
                )
            })
        })
        .transpose()?
        .unwrap_or_else(|| serde_json::json!({}));
    let remote_call_id = item
        .get("call_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let id = *call_ids.entry(remote_call_id).or_default();
    Ok(ToolCall {
        id,
        name: name.to_owned(),
        arguments,
    })
}

fn normalize_responses_response(
    body: &serde_json::Value,
    call_ids: &mut BTreeMap<String, ToolCallId>,
) -> Result<Vec<ModelStreamEvent>> {
    let mut events = Vec::new();
    if let Some(output) = body.get("output").and_then(serde_json::Value::as_array) {
        for item in output {
            match item.get("type").and_then(serde_json::Value::as_str) {
                Some("message") => {
                    if let Some(content) = item.get("content").and_then(serde_json::Value::as_array)
                    {
                        for part in content {
                            if let Some(text) = part.get("text").and_then(serde_json::Value::as_str)
                            {
                                events.push(ModelStreamEvent::TextDelta {
                                    text: text.to_owned(),
                                });
                            }
                        }
                    }
                }
                Some("function_call") => {
                    let name = item
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| {
                            LoomError::new(
                                ErrorCode::ProviderInvalidResponse,
                                "Copilot Responses function call did not contain a name",
                                false,
                            )
                        })?;
                    let arguments = item
                        .get("arguments")
                        .and_then(serde_json::Value::as_str)
                        .map(|arguments| {
                            serde_json::from_str(arguments).map_err(|error| {
                                LoomError::new(
                                    ErrorCode::ProviderInvalidResponse,
                                    format!(
                                        "Copilot Responses tool arguments were invalid: {error}"
                                    ),
                                    false,
                                )
                            })
                        })
                        .transpose()?
                        .unwrap_or_else(|| serde_json::json!({}));
                    let remote_call_id = item
                        .get("call_id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let call_id = call_ids.entry(remote_call_id).or_default().to_owned();
                    events.push(ModelStreamEvent::ToolCallDelta {
                        call: ToolCall {
                            id: call_id,
                            name: name.to_owned(),
                            arguments,
                        },
                    });
                }
                _ => {}
            }
        }
    }
    if let Some(usage) = body.get("usage") {
        events.push(ModelStreamEvent::Usage {
            usage: TokenUsage {
                input_tokens: usage
                    .get("input_tokens")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default(),
                output_tokens: usage
                    .get("output_tokens")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default(),
                cached_input_tokens: usage
                    .get("input_tokens_details")
                    .and_then(|details| details.get("cached_tokens"))
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default(),
            },
        });
    }
    events.push(ModelStreamEvent::Completed {
        reason: if body.get("status").and_then(serde_json::Value::as_str) == Some("completed") {
            FinishReason::Stop
        } else {
            FinishReason::Error
        },
    });
    Ok(events)
}

fn bearer_header(token: &str) -> String {
    let mut value = String::from("Bearer ");
    value.push_str(token);
    value
}

fn trim_endpoint(endpoint: &str) -> String {
    endpoint.trim_end_matches('/').to_owned()
}

fn configure_request<B>(request: ureq::RequestBuilder<B>) -> ureq::RequestBuilder<B> {
    request
        .config()
        .timeout_global(Some(PROVIDER_REQUEST_TIMEOUT))
        .http_status_as_error(false)
        .build()
}

fn ensure_success(
    provider: &str,
    mut response: ureq::http::Response<ureq::Body>,
) -> Result<ureq::http::Response<ureq::Body>> {
    let status = response.status().as_u16();
    if status < 400 {
        return Ok(response);
    }
    let detail = response.body_mut().read_to_string().unwrap_or_default();
    let detail = detail
        .replace("Bearer ", "****** ")
        .replace("token ", "token [redacted] ");
    let detail = detail.chars().take(512).collect::<String>();
    if detail.trim().is_empty() {
        Err(normalize_provider_error(provider, status))
    } else {
        Err(LoomError::new(
            ErrorCode::ProviderInvalidResponse,
            format!("{provider} rejected the request (HTTP {status}): {detail}"),
            false,
        ))
    }
}

fn normalize_provider_request_error(provider: &str, error: ureq::Error) -> LoomError {
    match error {
        ureq::Error::StatusCode(status) => normalize_provider_error(provider, status),
        error => normalize_transport_error(provider, &error.to_string()),
    }
}

fn normalize_oauth_error(operation: &str, error: ureq::Error) -> LoomError {
    match error {
        ureq::Error::StatusCode(status) => LoomError::new(
            ErrorCode::ProviderAuthentication,
            format!("{operation} failed (HTTP {status})"),
            false,
        ),
        error => normalize_transport_error(operation, &error.to_string()),
    }
}

fn oauth_response_error(response: OAuthTokenResponse) -> LoomError {
    let detail = response
        .error_description
        .or(response.error)
        .unwrap_or_else(|| "GitHub did not issue an access token".to_owned());
    LoomError::new(
        ErrorCode::ProviderAuthentication,
        format!("GitHub authentication failed: {detail}"),
        false,
    )
}

fn ollama_chat_endpoint(mut endpoint: String) -> String {
    while endpoint.ends_with('/') {
        endpoint.pop();
    }
    if endpoint.ends_with("/v1/chat/completions") {
        endpoint
    } else {
        format!("{endpoint}/v1/chat/completions")
    }
}

fn health_endpoint(endpoint: &str) -> String {
    endpoint
        .strip_suffix("/chat/completions")
        .map_or_else(|| endpoint.to_owned(), |base| format!("{base}/models"))
}

pub fn cost_for_usage(
    usage: &TokenUsage,
    input_cost_micros_per_1k: u64,
    output_cost_micros_per_1k: u64,
) -> u64 {
    usage
        .input_tokens
        .saturating_mul(input_cost_micros_per_1k)
        .saturating_add(
            usage
                .output_tokens
                .saturating_mul(output_cost_micros_per_1k),
        )
        / 1_000
}

pub fn normalize_provider_error(provider: &str, status: u16) -> LoomError {
    match status {
        401 | 403 => LoomError::new(
            ErrorCode::ProviderAuthentication,
            format!("{provider} rejected the configured credential (HTTP {status})"),
            false,
        ),
        425 | 429 => LoomError::new(
            ErrorCode::ProviderRateLimited,
            format!("{provider} is rate limited or timed out (HTTP {status})"),
            true,
        ),
        408 | 500..=599 => LoomError::new(
            ErrorCode::ProviderUnavailable,
            format!("{provider} is unavailable (HTTP {status})"),
            true,
        ),
        _ => LoomError::new(
            ErrorCode::ProviderInvalidResponse,
            format!("{provider} rejected the request (HTTP {status})"),
            false,
        ),
    }
}

pub fn normalize_http_error(provider: &str, status: u16, _body: &str) -> LoomError {
    normalize_provider_error(provider, status)
}

pub fn normalize_transport_error(provider: &str, _detail: &str) -> LoomError {
    LoomError::new(
        ErrorCode::ProviderUnavailable,
        format!("{provider} transport failed"),
        true,
    )
}

fn message_json(message: &ModelMessage) -> serde_json::Value {
    let role = match message.role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    };
    let mut value = serde_json::json!({
        "role": role,
        "content": message.content
    });
    if let Some(name) = &message.name {
        value["name"] = serde_json::json!(name);
    }
    if let Some(tool_call_id) = message.tool_call_id {
        value["tool_call_id"] = serde_json::json!(tool_call_id.to_string());
    }
    if !message.tool_calls.is_empty() {
        value["tool_calls"] = serde_json::Value::Array(
            message
                .tool_calls
                .iter()
                .map(|call| {
                    serde_json::json!({
                        "id": call.id.to_string(),
                        "type": "function",
                        "function": {
                            "name": call.name,
                            "arguments": call.arguments.to_string()
                        }
                    })
                })
                .collect(),
        );
    }
    value
}

pub fn normalize_openai_response(body: &serde_json::Value) -> Result<Vec<ModelStreamEvent>> {
    let choice = body
        .get("choices")
        .and_then(serde_json::Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| {
            LoomError::new(
                ErrorCode::ProviderInvalidResponse,
                "OpenAI-compatible response did not contain a choice",
                false,
            )
        })?;
    let message = choice.get("message").ok_or_else(|| {
        LoomError::new(
            ErrorCode::ProviderInvalidResponse,
            "OpenAI-compatible choice did not contain a message",
            false,
        )
    })?;
    let mut events = Vec::new();
    if let Some(content) = message.get("content").and_then(serde_json::Value::as_str)
        && !content.is_empty()
    {
        events.push(ModelStreamEvent::TextDelta {
            text: content.to_owned(),
        });
    }
    if let Some(tool_calls) = message
        .get("tool_calls")
        .and_then(serde_json::Value::as_array)
    {
        for tool_call in tool_calls {
            let function = tool_call.get("function").ok_or_else(|| {
                LoomError::new(
                    ErrorCode::ProviderInvalidResponse,
                    "OpenAI-compatible tool call did not contain a function",
                    false,
                )
            })?;
            let name = function
                .get("name")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::ProviderInvalidResponse,
                        "OpenAI-compatible tool call did not contain a name",
                        false,
                    )
                })?;
            let arguments = function
                .get("arguments")
                .map(|arguments| {
                    if let Some(arguments) = arguments.as_str() {
                        serde_json::from_str(arguments).map_err(|error| {
                            LoomError::new(
                                ErrorCode::ProviderInvalidResponse,
                                format!("tool arguments were not valid JSON: {error}"),
                                false,
                            )
                        })
                    } else if arguments.is_object() {
                        Ok(arguments.clone())
                    } else {
                        Err(LoomError::new(
                            ErrorCode::ProviderInvalidResponse,
                            "tool arguments must be a JSON object",
                            false,
                        ))
                    }
                })
                .transpose()?
                .unwrap_or_else(|| serde_json::json!({}));
            events.push(ModelStreamEvent::ToolCallDelta {
                call: ToolCall {
                    id: ToolCallId::new(),
                    name: name.to_owned(),
                    arguments,
                },
            });
        }
    }
    if let Some(usage) = body.get("usage") {
        events.push(ModelStreamEvent::Usage {
            usage: TokenUsage {
                input_tokens: usage
                    .get("prompt_tokens")
                    .or_else(|| usage.get("input_tokens"))
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default(),
                output_tokens: usage
                    .get("completion_tokens")
                    .or_else(|| usage.get("output_tokens"))
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default(),
                cached_input_tokens: usage
                    .get("prompt_tokens_details")
                    .and_then(|details| details.get("cached_tokens"))
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default(),
            },
        });
    }
    let reason = match choice
        .get("finish_reason")
        .and_then(serde_json::Value::as_str)
    {
        Some("stop") | None => FinishReason::Stop,
        Some("tool_calls") | Some("function_call") => FinishReason::ToolCall,
        Some("length") => FinishReason::Length,
        Some("cancelled") => FinishReason::Cancelled,
        Some(_) => FinishReason::Error,
    };
    events.push(ModelStreamEvent::Completed { reason });
    Ok(events)
}

fn internal_lock_error(resource: &str) -> LoomError {
    LoomError::new(
        ErrorCode::Internal,
        format!("{resource} lock was poisoned"),
        true,
    )
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        thread,
        time::Duration,
    };

    use loom_model::CollectingSink;

    use super::*;

    #[test]
    fn github_copilot_gpt_6_models_use_the_responses_endpoint() {
        assert!(uses_responses_endpoint("gpt-5.6-luna"));
        assert!(uses_responses_endpoint("gpt-6-luna"));
        assert!(uses_responses_endpoint("gpt-6-astra"));
        assert!(!uses_responses_endpoint("gpt-4o"));
    }

    #[test]
    fn request_payloads_preserve_options_and_convert_tool_messages() {
        let call = ToolCall {
            id: ToolCallId::new(),
            name: "read_file".to_owned(),
            arguments: serde_json::json!({"path":"README.md"}),
        };
        let mut assistant = ModelMessage::new(MessageRole::Assistant, "");
        assistant.tool_calls.push(call.clone());
        let mut tool = ModelMessage::new(MessageRole::Tool, "file contents");
        tool.tool_call_id = Some(call.id);
        let mut request = ModelRequest {
            model: ModelId::new("gpt-6-luna"),
            messages: vec![
                ModelMessage::new(MessageRole::System, "system"),
                ModelMessage::new(MessageRole::User, "hello"),
                assistant,
                tool,
            ],
            tools: Vec::new(),
            options: Default::default(),
        };
        request.options.max_output_tokens = Some(128);

        let payload = responses_request_payload(&request);
        assert_eq!(payload["max_output_tokens"], 128);
        assert_eq!(payload["input"][0]["role"], "system");
        assert_eq!(payload["input"][1]["content"][0]["type"], "input_text");
        assert_eq!(payload["input"][2]["type"], "function_call");
        assert_eq!(payload["input"][3]["type"], "function_call_output");
        assert_eq!(payload["input"][3]["output"], "file contents");

        request.model = ModelId::new("fixture/model");
        request.options.temperature = Some(0.25);
        request.options.max_output_tokens = Some(64);
        request.options.stop_sequences = vec!["STOP".to_owned()];
        let payload = openai_request_payload(&request);
        assert_eq!(payload["temperature"], 0.25);
        assert_eq!(payload["max_tokens"], 64);
        assert_eq!(payload["stop"][0], "STOP");
        assert_eq!(
            payload["messages"][2]["tool_calls"][0]["function"]["name"],
            "read_file"
        );

        request.tools.push(loom_model::ToolDefinition {
            name: "search".to_owned(),
            description: "Search files".to_owned(),
            input_schema: serde_json::json!({"type":"object"}),
        });
        assert_eq!(
            responses_request_payload(&request)["tools"][0]["name"],
            "search"
        );
        assert_eq!(
            openai_request_payload(&request)["tools"][0]["function"]["name"],
            "search"
        );
    }

    #[test]
    fn provider_usage_and_status_helpers_handle_empty_and_extreme_values() {
        assert_eq!(parse_tool_arguments("  ").unwrap(), serde_json::json!({}));
        assert_eq!(
            finish_reason_from_str("function_call"),
            FinishReason::ToolCall
        );
        assert_eq!(finish_reason_from_str("length"), FinishReason::Length);
        assert_eq!(finish_reason_from_str("cancelled"), FinishReason::Cancelled);
        assert_eq!(finish_reason_from_str("unexpected"), FinishReason::Error);

        let usage = openai_usage(&serde_json::json!({
            "input_tokens": 12,
            "output_tokens": 8,
            "prompt_tokens_details": {"cached_tokens": 3}
        }));
        assert_eq!(usage.input_tokens, 12);
        assert_eq!(usage.output_tokens, 8);
        assert_eq!(usage.cached_input_tokens, 3);
        assert_eq!(
            responses_usage(&serde_json::json!({"input_tokens": 4, "output_tokens": 2}))
                .output_tokens,
            2
        );
        assert_eq!(cost_for_usage(&usage, 1_000, 2_000), 28);
        assert_eq!(cost_for_usage(&usage, u64::MAX, u64::MAX), u64::MAX / 1_000);
        assert_eq!(
            health_endpoint("https://example.test/v1/chat/completions"),
            "https://example.test/v1/models"
        );
        let provider = ProviderId::new("fixture");
        let model = ModelId::new("fixture/model");
        let mut ledger = UsageLedger::default();
        ledger.record(provider.clone(), model.clone(), usage.clone(), 28);
        ledger.record(provider.clone(), model.clone(), usage, 28);
        assert_eq!(ledger.aggregates.len(), 1);
        assert_eq!(ledger.summary(Some(&provider), Some(&model)).requests, 2);
        assert_eq!(ledger.summary(None, None).input_tokens, 24);
        assert_eq!(ledger.session_usage().cost_micros, 56);
        assert_eq!(
            serde_json::from_value::<UsageLedger>(serde_json::to_value(&ledger).unwrap()).unwrap(),
            ledger
        );
        assert_eq!(
            health_endpoint("https://example.test/status"),
            "https://example.test/status"
        );
        assert_eq!(
            trim_endpoint("https://example.test/api///"),
            "https://example.test/api"
        );
        assert_eq!(
            ollama_chat_endpoint("http://localhost:11434///".to_owned()),
            "http://localhost:11434/v1/chat/completions"
        );
        assert_eq!(
            ollama_chat_endpoint("http://localhost/v1/chat/completions/".to_owned()),
            "http://localhost/v1/chat/completions"
        );
        assert_eq!(bearer_header("secret"), "Bearer secret");
        let oauth_error = oauth_response_error(OAuthTokenResponse {
            access_token: None,
            error: Some("access_denied".to_owned()),
            error_description: Some("user cancelled".to_owned()),
        });
        assert_eq!(oauth_error.code, ErrorCode::ProviderAuthentication);
        assert!(oauth_error.message.contains("user cancelled"));

        for (status, code, retryable) in [
            (401, ErrorCode::ProviderAuthentication, false),
            (425, ErrorCode::ProviderRateLimited, true),
            (503, ErrorCode::ProviderUnavailable, true),
            (400, ErrorCode::ProviderInvalidResponse, false),
        ] {
            let error = normalize_provider_error("fixture", status);
            assert_eq!(error.code, code);
            assert_eq!(error.retryable, retryable);
        }
    }

    #[test]
    fn responses_stream_decoder_handles_events_and_rejects_malformed_streams() {
        let mut decoder = StreamDecoder::responses("copilot".to_owned());
        let mut call_ids = BTreeMap::new();
        let mut sink = CollectingSink::default();

        assert_eq!(
            decoder
                .accept(
                    &serde_json::json!({"type":"response.output_text.delta", "delta":""}),
                    &mut call_ids,
                    &mut sink,
                )
                .unwrap(),
            StreamFlow::Continue
        );
        decoder
            .accept(
                &serde_json::json!({"type":"response.output_text.delta", "delta":"hello"}),
                &mut call_ids,
                &mut sink,
            )
            .unwrap();
        decoder
            .accept(
                &serde_json::json!({"type":"response.output_item.done"}),
                &mut call_ids,
                &mut sink,
            )
            .unwrap();
        decoder
            .accept(
                &serde_json::json!({"type":"response.output_item.done", "item":{"type":"message"}}),
                &mut call_ids,
                &mut sink,
            )
            .unwrap();
        decoder
            .accept(
                &serde_json::json!({
                    "type":"response.output_item.done",
                    "item":{"type":"function_call", "name":"read_file", "call_id":"call-1", "arguments":"{\"path\":\"README.md\"}"}
                }),
                &mut call_ids,
                &mut sink,
            )
            .unwrap();
        decoder
            .accept(
                &serde_json::json!({
                    "type":"response.completed",
                    "response":{"status":"completed", "usage":{"input_tokens":3, "output_tokens":2}}
                }),
                &mut call_ids,
                &mut sink,
            )
            .unwrap();
        decoder.finish(&mut sink).unwrap();

        assert!(
            matches!(sink.events.first(), Some(ModelStreamEvent::TextDelta { text }) if text == "hello")
        );
        assert!(sink.events.iter().any(|event| matches!(
            event,
            ModelStreamEvent::ToolCallDelta { call }
                if call.name == "read_file" && call.arguments["path"] == "README.md"
        )));
        assert!(sink.events.iter().any(|event| matches!(
            event,
            ModelStreamEvent::Usage { usage } if usage.input_tokens == 3 && usage.output_tokens == 2
        )));
        assert!(matches!(
            sink.events.last(),
            Some(ModelStreamEvent::Completed {
                reason: FinishReason::Stop
            })
        ));

        let mut malformed_call = StreamDecoder::responses("copilot".to_owned());
        assert!(
            malformed_call
                .accept(
                    &serde_json::json!({
                        "type":"response.output_item.done",
                        "item":{"type":"function_call", "arguments":"{}"}
                    }),
                    &mut BTreeMap::new(),
                    &mut CollectingSink::default(),
                )
                .is_err()
        );
        let mut stream_error = StreamDecoder::responses("copilot".to_owned());
        let error = stream_error
            .accept(
                &serde_json::json!({"type":"error", "message":"upstream failed"}),
                &mut BTreeMap::new(),
                &mut CollectingSink::default(),
            )
            .unwrap_err();
        assert!(error.message.contains("upstream failed"));
    }

    #[test]
    fn responses_stream_decoder_preserves_failure_and_incomplete_details() {
        for (event, expected) in [
            (
                serde_json::json!({
                    "type":"response.failed",
                    "response":{"status":"failed", "error":{"message":"model overloaded"}}
                }),
                "copilot request failed: model overloaded",
            ),
            (
                serde_json::json!({
                    "type":"response.incomplete",
                    "response":{"status":"incomplete", "incomplete_details":{"reason":"max_output_tokens"}}
                }),
                "copilot response was incomplete: max_output_tokens",
            ),
        ] {
            let mut decoder = StreamDecoder::responses("copilot".to_owned());
            let mut sink = CollectingSink::default();
            decoder
                .accept(&event, &mut BTreeMap::new(), &mut sink)
                .unwrap();
            decoder.finish(&mut sink).unwrap();
            assert!(matches!(
                sink.events.last(),
                Some(ModelStreamEvent::Completed {
                    reason: FinishReason::ErrorWithMessage { message }
                }) if message == expected
            ));
        }
    }

    #[test]
    fn chat_stream_decoder_emits_tool_usage_and_completion_or_rejects_bad_arguments() {
        let mut decoder = StreamDecoder::chat_completions("fixture".to_owned());
        let mut sink = CollectingSink::default();
        decoder
            .accept(
                &serde_json::json!({
                    "choices":[{"delta":{"tool_calls":[
                        {"index":0,"function":{"name":"read_", "arguments":"{\"path\":"}},
                        {"index":0,"function":{"name":"file", "arguments":"\"README.md\"}"}}
                    ]}}],
                    "usage":{"prompt_tokens":5,"completion_tokens":4}
                }),
                &mut BTreeMap::new(),
                &mut sink,
            )
            .unwrap();
        decoder.finish(&mut sink).unwrap();
        assert!(sink.events.iter().any(|event| matches!(
            event,
            ModelStreamEvent::ToolCallDelta { call }
                if call.name == "read_file" && call.arguments["path"] == "README.md"
        )));
        assert!(matches!(
            sink.events.last(),
            Some(ModelStreamEvent::Completed {
                reason: FinishReason::ToolCall
            })
        ));

        let mut malformed = StreamDecoder::chat_completions("fixture".to_owned());
        let mut malformed_sink = CollectingSink::default();
        malformed
            .accept(
                &serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"read_file","arguments":"{"}}]}}]}),
                &mut BTreeMap::new(),
                &mut malformed_sink,
            )
            .unwrap();
        assert!(malformed.finish(&mut malformed_sink).is_err());

        let missing_name = StreamDecoder {
            tool_calls: BTreeMap::from([(0, PartialToolCall::default())]),
            ..StreamDecoder::chat_completions("fixture".to_owned())
        };
        assert!(missing_name.finish(&mut CollectingSink::default()).is_err());
    }

    #[test]
    fn discovery_limits_are_per_model_and_validate_advertised_values() {
        assert_eq!(
            discovered_context_window(&serde_json::json!({"context_length": 8192})),
            Some(8192)
        );
        assert_eq!(
            discovered_context_window(
                &serde_json::json!({"capabilities": {"limits": {"max_context_window_tokens": 64000, "max_prompt_tokens": 32000}}})
            ),
            Some(32000)
        );
        for model in [
            serde_json::json!({}),
            serde_json::json!({"context_length": 0}),
            serde_json::json!({"context_length": -1}),
            serde_json::json!({"context_length": u64::MAX}),
            serde_json::json!({"context_length": "8192"}),
        ] {
            assert_eq!(discovered_context_window(&model), None);
        }
    }

    #[test]
    fn github_copilot_discovery_lists_only_models_with_tool_call_support() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut token_stream, _) = listener.accept().unwrap();
            read_request_headers(&mut token_stream).unwrap();
            write_response(
                &mut token_stream,
                "application/json",
                &format!(r#"{{"token":"copilot-token","endpoints":{{"api":"http://{address}"}}}}"#),
            )
            .unwrap();

            let (mut models_stream, _) = listener.accept().unwrap();
            read_request_headers(&mut models_stream).unwrap();
            write_response(
                &mut models_stream,
                "application/json",
                r#"{"data":[
                    {"id":"agentic-model","capabilities":{"supports":{"tool_calls":true},"limits":{"max_context_window_tokens":64000,"max_prompt_tokens":32000}}},
                    {"id":"chat-model","capabilities":{"supports":{"tool_calls":false}}},
                    {"id":"unknown-model","capabilities":{"supports":{}}}
                ]}"#,
            )
            .unwrap();
        });
        let descriptor = github_copilot_descriptor();
        let provider = GitHubCopilotProvider::with_endpoints(
            format!("http://{address}"),
            format!("http://{address}/token"),
            "github-token",
            descriptor,
        );

        let models = provider.discover_models().unwrap();
        assert_eq!(models[0].context_window, Some(32_000));

        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["agentic-model"]
        );
        server.join().unwrap();
    }

    fn collect(provider: &mut impl ModelProvider, request: &ModelRequest) -> Vec<ModelStreamEvent> {
        let mut sink = CollectingSink::default();
        provider
            .stream(request, &CancellationToken::new(), &mut sink)
            .unwrap();
        sink.events
    }

    fn serve_once(
        body: &'static str,
        content_type: &'static str,
    ) -> (String, thread::JoinHandle<std::result::Result<(), String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().map_err(|error| error.to_string())?;
            read_request_headers(&mut stream)?;
            write_response(&mut stream, content_type, body)?;
            Ok(())
        });
        (format!("http://{address}/v1/chat/completions"), server)
    }

    fn read_request_headers(stream: &mut TcpStream) -> std::result::Result<String, String> {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|error| error.to_string())?;
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut chunk).map_err(|error| error.to_string())?;
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
            if request.len() > 64 * 1024 {
                return Err("request headers exceeded fixture limit".to_owned());
            }
        }
        Ok(String::from_utf8_lossy(&request).into_owned())
    }

    fn write_response(
        stream: &mut TcpStream,
        content_type: &str,
        body: &str,
    ) -> std::result::Result<(), String> {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .map_err(|error| error.to_string())?;
        stream.flush().map_err(|error| error.to_string())
    }

    #[test]
    fn openai_compatible_provider_emits_text_before_the_stream_ends() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"par\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"tial\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":",
            "{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":",
            "{\"arguments\":\"\\\"README.md\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":4}}\n\n",
            "data: [DONE]\n\n"
        );
        let (endpoint, server) = serve_once(body, "text/event-stream");
        let mut provider =
            OpenAiCompatibleProvider::new(endpoint, "", ModelId::new("fixture/model"));
        let request = ModelRequest {
            model: ModelId::new("fixture/model"),
            messages: vec![ModelMessage::new(MessageRole::User, "hello")],
            tools: Vec::new(),
            options: Default::default(),
        };
        let mut seen = Vec::new();
        provider
            .stream(&request, &CancellationToken::new(), &mut |event| {
                seen.push(event);
                Ok(StreamFlow::Continue)
            })
            .unwrap_or_else(|error| panic!("provider stream failed: {error:?}"));
        server
            .join()
            .expect("fixture server thread panicked")
            .expect("fixture server failed");
        assert!(matches!(
            seen.first(),
            Some(ModelStreamEvent::TextDelta { text }) if text == "par"
        ));
        assert!(matches!(
            seen.get(1),
            Some(ModelStreamEvent::TextDelta { text }) if text == "tial"
        ));
        assert!(seen.iter().any(|event| matches!(
            event,
            ModelStreamEvent::ToolCallDelta { call }
                if call.name == "read_file" && call.arguments["path"] == "README.md"
        )));
        assert!(seen.iter().any(|event| matches!(
            event,
            ModelStreamEvent::Usage { usage } if usage.input_tokens == 7
        )));
        assert!(matches!(
            seen.last(),
            Some(ModelStreamEvent::Completed {
                reason: FinishReason::ToolCall
            })
        ));
    }

    #[test]
    fn a_cancelled_token_stops_a_provider_before_it_sends() {
        let mut provider = DeterministicProvider::demo();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = provider
            .stream(
                &ModelRequest {
                    model: ModelId::new("deterministic/demo"),
                    messages: Vec::new(),
                    tools: Vec::new(),
                    options: Default::default(),
                },
                &cancel,
                &mut CollectingSink::default(),
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::RequestCancelled);
    }

    #[test]
    fn a_sink_can_stop_a_stream_early() {
        let mut provider = DeterministicProvider::demo();
        let mut seen = 0_usize;
        provider
            .stream(
                &ModelRequest {
                    model: ModelId::new("deterministic/demo"),
                    messages: Vec::new(),
                    tools: Vec::new(),
                    options: Default::default(),
                },
                &CancellationToken::new(),
                &mut |_event| {
                    seen += 1;
                    Ok(StreamFlow::Stop)
                },
            )
            .unwrap();
        assert_eq!(seen, 1);
    }

    #[test]
    fn deterministic_provider_streams_tool_calls_and_completion() {
        let mut provider = DeterministicProvider::demo();
        let request = ModelRequest {
            model: ModelId::new("deterministic/demo"),
            messages: Vec::new(),
            tools: Vec::new(),
            options: Default::default(),
        };

        let first = collect(&mut provider, &request);
        assert!(matches!(
            first.get(1),
            Some(ModelStreamEvent::ToolCallDelta { call })
                if call.name == "list_files"
        ));
        let final_step = (0..3)
            .map(|_| collect(&mut provider, &request))
            .last()
            .unwrap();
        assert!(final_step.iter().any(|event| {
            matches!(
                event,
                ModelStreamEvent::Completed {
                    reason: FinishReason::Stop
                }
            )
        }));
    }

    #[test]
    fn openai_compatible_response_normalization_rejects_malformed_tool_arguments() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "function": {"name": "read_file", "arguments": "not-json"}
                    }]
                }
            }]
        });
        let error = normalize_openai_response(&body).unwrap_err();
        assert_eq!(error.code, ErrorCode::ProviderInvalidResponse);
    }

    #[test]
    fn openai_response_normalization_validates_envelopes_and_argument_shapes() {
        for body in [
            serde_json::json!({}),
            serde_json::json!({"choices": []}),
            serde_json::json!({"choices": [{}]}),
        ] {
            assert_eq!(
                normalize_openai_response(&body).unwrap_err().code,
                ErrorCode::ProviderInvalidResponse
            );
        }
        for tool_call in [
            serde_json::json!({}),
            serde_json::json!({"function": {}}),
            serde_json::json!({"function": {"name": "read_file", "arguments": 7}}),
        ] {
            let body = serde_json::json!({
                "choices": [{"message": {"tool_calls": [tool_call]}}]
            });
            assert_eq!(
                normalize_openai_response(&body).unwrap_err().code,
                ErrorCode::ProviderInvalidResponse
            );
        }

        let events = normalize_openai_response(&serde_json::json!({
            "choices": [{"message": {
                "content": "",
                "tool_calls": [{"function": {"name": "read_file"}}]
            }, "finish_reason": null}]
        }))
        .unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events.first(),
            Some(ModelStreamEvent::ToolCallDelta { call })
                if call.name == "read_file" && call.arguments == serde_json::json!({})
        ));
        assert!(matches!(
            events.last(),
            Some(ModelStreamEvent::Completed {
                reason: FinishReason::Stop
            })
        ));
    }

    #[test]
    fn responses_normalization_covers_text_tool_usage_and_invalid_calls() {
        let mut call_ids = BTreeMap::new();
        let events = normalize_responses_response(
            &serde_json::json!({
                "status": "completed",
                "output": [
                    {"type":"message","content":[{"text":"answer"},{"text": 4}]},
                    {"type":"function_call","call_id":"call-1","name":"read_file","arguments":"{\"path\":\"README.md\"}"},
                    {"type":"unknown"}
                ],
                "usage":{"input_tokens":10,"output_tokens":3,"input_tokens_details":{"cached_tokens":2}}
            }),
            &mut call_ids,
        )
        .unwrap();
        assert!(matches!(events[0], ModelStreamEvent::TextDelta { ref text } if text == "answer"));
        assert!(
            matches!(events[1], ModelStreamEvent::ToolCallDelta { ref call } if call.name == "read_file" && call.arguments["path"] == "README.md")
        );
        assert!(
            matches!(events[2], ModelStreamEvent::Usage { ref usage } if usage.cached_input_tokens == 2)
        );
        assert!(matches!(
            events[3],
            ModelStreamEvent::Completed {
                reason: FinishReason::Stop
            }
        ));

        for output in [
            serde_json::json!([{"type":"function_call"}]),
            serde_json::json!([{"type":"function_call","name":"read_file","arguments":"invalid"}]),
        ] {
            assert_eq!(
                normalize_responses_response(&serde_json::json!({"output": output}), &mut call_ids)
                    .unwrap_err()
                    .code,
                ErrorCode::ProviderInvalidResponse
            );
        }
        assert!(matches!(
            normalize_responses_response(&serde_json::json!({"status":"failed"}), &mut call_ids)
                .unwrap()
                .last(),
            Some(ModelStreamEvent::Completed {
                reason: FinishReason::Error
            })
        ));
    }

    #[test]
    fn registry_lists_deterministic_and_local_models_without_secrets() {
        let registry = ProviderRegistry::demo();
        let models = registry.list_models().unwrap();
        assert!(
            models
                .iter()
                .any(|model| model.provider.as_str() == "deterministic")
        );
        assert!(
            models
                .iter()
                .any(|model| model.provider.as_str() == "ollama")
        );
        let serialized = serde_json::to_string(&registry.list_providers().unwrap()).unwrap();
        assert!(!serialized.contains("Bearer"));
    }

    #[test]
    fn registry_validates_provider_and_model_configuration_and_supports_catalog_updates() {
        let registry = ProviderRegistry::new();
        let credential = CredentialRef::new("fixture-token");
        let model = ModelDescriptor {
            id: ModelId::new("fixture/model"),
            provider: ProviderId::new("fixture"),
            display_name: "Fixture model".to_owned(),
            context_window: Some(8_000),
            capabilities: ModelCapabilities::default(),
        };

        assert!(
            registry
                .register(ProviderConfig::openai_compatible(
                    "",
                    "empty",
                    "http://localhost/v1",
                    model.clone(),
                    None
                ))
                .is_err()
        );
        let mut no_models = ProviderConfig::deterministic();
        no_models.models.clear();
        assert!(registry.register(no_models).is_err());
        assert!(
            registry
                .register(ProviderConfig::openai_compatible(
                    "fixture",
                    "fixture",
                    "http://localhost/v1",
                    model.clone(),
                    Some(CredentialRef::new("  ")),
                ))
                .is_err()
        );
        assert!(registry.register(ProviderConfig::deterministic()).is_ok());
        let mut wrong_deterministic = ProviderConfig::deterministic();
        wrong_deterministic.models[0].id = ModelId::new("deterministic/other");
        assert!(registry.register(wrong_deterministic).is_err());

        registry
            .register(ProviderConfig::openai_compatible(
                "fixture",
                "Fixture provider",
                "http://localhost/v1/chat/completions",
                model.clone(),
                Some(credential),
            ))
            .unwrap();
        assert!(
            registry
                .add_model(&ProviderId::new("missing"), model.clone())
                .is_err()
        );
        let mut empty_id = model.clone();
        empty_id.id = ModelId::new(" ");
        assert!(
            registry
                .add_model(&ProviderId::new("fixture"), empty_id)
                .is_err()
        );
        assert!(
            registry
                .add_model(&ProviderId::new("fixture"), model.clone())
                .is_err()
        );
        let second = ModelDescriptor {
            id: ModelId::new("fixture/second"),
            provider: ProviderId::new("fixture"),
            display_name: "Second model".to_owned(),
            context_window: None,
            capabilities: ModelCapabilities::default(),
        };
        registry
            .add_model(&ProviderId::new("fixture"), second.clone())
            .unwrap();
        assert!(registry.describe_model(&second.id).is_ok());
        assert!(
            registry
                .describe_model(&ModelId::new("missing/model"))
                .is_err()
        );
        assert!(registry.pricing(&ModelId::new("fixture/model")).is_ok());
        assert!(
            registry
                .create_provider(&ModelId::new("missing/model"))
                .is_err()
        );
        assert!(
            registry
                .health(&ProviderId::new("missing"))
                .unwrap()
                .checked_at
                .is_none()
        );
    }

    #[test]
    fn registry_restores_copilot_configs_without_trusting_saved_model_lists() {
        let credentials = Arc::new(InMemoryCredentialStore::default());
        let registry = ProviderRegistry::with_credentials(credentials.clone());
        registry
            .configure_github_copilot("github-secret".to_owned())
            .unwrap();
        let copilot_model = ModelId::new(GITHUB_COPILOT_DEFAULT_MODEL);
        assert_eq!(
            registry
                .create_provider(&copilot_model)
                .unwrap()
                .descriptor()
                .id,
            copilot_model
        );
        let mut configs = registry.export_configs().unwrap();
        let copilot_provider = configs[0].id.clone();
        configs[0].models.push(ModelDescriptor {
            id: ModelId::new("untrusted/saved-model"),
            provider: copilot_provider,
            display_name: "Untrusted saved model".to_owned(),
            context_window: None,
            capabilities: ModelCapabilities::default(),
        });
        configs.push(ProviderConfig::github_copilot(CredentialRef::new(
            "missing",
        )));
        configs.push(ProviderConfig::deterministic());
        registry.restore_configs(configs).unwrap();
        let restored = registry.export_configs().unwrap();
        let copilot = restored
            .iter()
            .find(|config| config.kind == ProviderKind::GitHubCopilot)
            .unwrap();
        assert_eq!(copilot.models.len(), 1);
        assert!(
            !copilot
                .models
                .iter()
                .any(|model| model.id.as_str() == "untrusted/saved-model")
        );
        assert_eq!(registry.list_providers().unwrap().len(), 1);
    }

    #[test]
    fn legacy_api_key_configs_migrate_to_separate_backend_credential_files() {
        let root = std::env::temp_dir().join(format!(
            "loom-credential-migration-{}",
            loom_core::RequestId::new()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let legacy_path = root.join("legacy.json");
        let shared = Arc::new(FileCredentialStore::open(&legacy_path).unwrap());
        let old_reference = CredentialRef::new("legacy-openai-key");
        shared
            .store(&old_reference, "legacy-secret-value".to_owned())
            .unwrap();

        let make_registry = || {
            let registry = ProviderRegistry::with_credentials(shared.clone());
            let provider_id = ProviderId::new("openai-compatible");
            registry
                .register(ProviderConfig::openai_compatible(
                    provider_id.clone(),
                    "OpenAI-compatible",
                    "http://127.0.0.1:8000/v1/chat/completions",
                    ModelDescriptor {
                        id: ModelId::new("openai-compatible/model"),
                        provider: provider_id,
                        display_name: "Model".to_owned(),
                        context_window: None,
                        capabilities: ModelCapabilities::default(),
                    },
                    Some(old_reference.clone()),
                ))
                .unwrap();
            registry
        };
        let first = make_registry();
        let second = make_registry();
        let first_scoped =
            Arc::new(FileCredentialStore::open(root.join("first.credentials.json")).unwrap());
        let second_scoped =
            Arc::new(FileCredentialStore::open(root.join("second.credentials.json")).unwrap());
        first
            .scope_api_key_credentials(first_scoped.clone())
            .unwrap();
        second
            .scope_api_key_credentials(second_scoped.clone())
            .unwrap();

        let mut first_configs = first.export_configs().unwrap();
        let mut second_configs = second.export_configs().unwrap();
        assert!(
            first
                .migrate_api_key_credentials(&mut first_configs)
                .unwrap()
        );
        assert!(
            second
                .migrate_api_key_credentials(&mut second_configs)
                .unwrap()
        );
        first.restore_configs(first_configs).unwrap();
        second.restore_configs(second_configs).unwrap();

        let first_reference = first.list_providers().unwrap()[0]
            .credential_id
            .clone()
            .unwrap();
        let second_reference = second.list_providers().unwrap()[0]
            .credential_id
            .clone()
            .unwrap();
        assert_ne!(first_reference, second_reference);
        assert_eq!(
            first_scoped
                .resolve(&CredentialRef::new(first_reference))
                .unwrap(),
            "legacy-secret-value"
        );
        assert_eq!(
            second_scoped
                .resolve(&CredentialRef::new(second_reference))
                .unwrap(),
            "legacy-secret-value"
        );
        assert_eq!(
            shared.resolve(&old_reference).unwrap(),
            "legacy-secret-value"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn registry_health_check_records_healthy_and_degraded_results() {
        let registry = ProviderRegistry::new();
        let provider_id = ProviderId::new("fixture");
        let model = ModelDescriptor {
            id: ModelId::new("fixture/model"),
            provider: provider_id.clone(),
            display_name: "Fixture model".to_owned(),
            context_window: None,
            capabilities: ModelCapabilities::default(),
        };

        let (endpoint, server) =
            serve_once(r#"{"data":[{"id":"fixture/model"}]}"#, "application/json");
        registry
            .register_openai_compatible(
                provider_id.clone(),
                "Fixture",
                format!("{endpoint}/chat/completions"),
                model.clone(),
                None,
            )
            .unwrap();
        let healthy = registry.check_health(&provider_id).unwrap();
        assert_eq!(healthy.state, ProviderHealthState::Healthy);
        assert_eq!(healthy.consecutive_failures, 0);
        server.join().unwrap().unwrap();

        let failed_registry = ProviderRegistry::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let unavailable_address = listener.local_addr().unwrap();
        drop(listener);
        failed_registry
            .register_openai_compatible(
                provider_id.clone(),
                "Fixture",
                format!("http://{unavailable_address}/v1/chat/completions"),
                model,
                None,
            )
            .unwrap();
        let degraded = failed_registry.check_health(&provider_id).unwrap();
        assert_eq!(degraded.state, ProviderHealthState::Degraded);
        assert_eq!(degraded.consecutive_failures, 1);
        assert!(degraded.last_error.is_some());

        assert!(
            failed_registry
                .check_health(&ProviderId::new("missing"))
                .is_err()
        );
        let empty_registry = ProviderRegistry::new();
        let mut empty_provider = ProviderConfig::ollama("http://127.0.0.1:1", "fixture/model");
        empty_provider.models.clear();
        empty_registry.register(empty_provider).unwrap_err();
    }

    #[test]
    fn provider_status_codes_are_normalized_without_response_body_or_keys() {
        let error = normalize_provider_error("openai", 429);
        assert_eq!(error.code, ErrorCode::ProviderRateLimited);
        assert!(error.retryable);
        assert!(!error.message.contains("secret"));
    }

    #[test]
    fn openai_compatible_provider_works_against_a_local_fixture() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || -> std::result::Result<(), String> {
            let (mut stream, _) = listener.accept().map_err(|error| error.to_string())?;
            let request_text = read_request_headers(&mut stream)?;
            if !request_text.contains("raw-key-never-in-events") {
                return Err("fixture request did not contain the expected API key".to_owned());
            }
            let body = r#"{"choices":[{"message":{"content":"fixture response"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#;
            write_response(&mut stream, "application/json", body)
        });
        let mut provider = OpenAiCompatibleProvider::new(
            format!("http://{address}/v1/chat/completions"),
            "raw-key-never-in-events",
            ModelId::new("fixture/model"),
        );
        let events = collect(
            &mut provider,
            &ModelRequest {
                model: ModelId::new("fixture/model"),
                messages: vec![ModelMessage::new(MessageRole::User, "hello")],
                tools: Vec::new(),
                options: Default::default(),
            },
        );
        assert!(events.iter().any(|event| {
            matches!(
                event,
                ModelStreamEvent::TextDelta { text } if text == "fixture response"
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                ModelStreamEvent::Usage { usage }
                    if usage.input_tokens == 3 && usage.output_tokens == 2
            )
        }));
        server
            .join()
            .expect("fixture server thread panicked")
            .expect("fixture server failed");
        let redacted_error = normalize_transport_error("fixture", "Bearer raw-key-never-in-events");
        assert!(!redacted_error.message.contains("raw-key-never-in-events"));
    }

    #[test]
    fn registry_discovers_models_with_a_credential_reference() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || -> std::result::Result<(), String> {
            let (mut stream, _) = listener.accept().map_err(|error| error.to_string())?;
            let request_text = read_request_headers(&mut stream)?;
            if !request_text.contains("discovery-key") {
                return Err("fixture request did not contain the expected discovery key".to_owned());
            }
            let body = r#"{"data":[{"id":"discovered/model"}]}"#;
            write_response(&mut stream, "application/json", body)
        });
        let credentials = Arc::new(InMemoryCredentialStore::default());
        credentials.insert(CredentialRef::new("gateway-key"), "discovery-key");
        let registry = ProviderRegistry::with_credentials(credentials);
        let descriptor = ModelDescriptor {
            id: ModelId::new("configured/model"),
            provider: ProviderId::new("gateway"),
            display_name: "Configured model".to_owned(),
            context_window: Some(4_096),
            capabilities: ModelCapabilities {
                tool_calling: true,
                ..Default::default()
            },
        };
        registry
            .register(
                ProviderConfig::openai_compatible(
                    "gateway",
                    "Fixture gateway",
                    format!("http://{address}/v1/chat/completions"),
                    descriptor,
                    Some(CredentialRef::new("gateway-key")),
                )
                .with_pricing(100, 200),
            )
            .unwrap();
        let models = registry
            .discover_models(&ProviderId::new("gateway"))
            .unwrap();
        assert_eq!(models[0].id.as_str(), "discovered/model");
        assert_eq!(models[0].context_window, None);
        assert_eq!(models[0].provider.as_str(), "gateway");
        assert_eq!(
            registry.pricing(&ModelId::new("discovered/model")).unwrap(),
            (100, 200)
        );
        server
            .join()
            .expect("fixture server thread panicked")
            .expect("fixture server failed");
    }

    #[test]
    fn ollama_uses_the_openai_compatible_local_chat_endpoint() {
        let provider = OllamaProvider::new("http://127.0.0.1:11434/", ModelId::new("llama3.2"));
        assert_eq!(
            provider.endpoint(),
            "http://127.0.0.1:11434/v1/chat/completions"
        );
        assert_eq!(provider.descriptor().provider.as_str(), "ollama");
    }

    #[test]
    fn file_credential_store_round_trips_secrets_without_exposing_them_in_debug() {
        let path =
            std::env::temp_dir().join(format!("loom-credentials-test-{}.json", std::process::id()));
        let store = FileCredentialStore::open(&path).unwrap();
        store
            .store(&CredentialRef::new("test"), "secret-value".to_owned())
            .unwrap();
        assert_eq!(
            store.resolve(&CredentialRef::new("test")).unwrap(),
            "secret-value"
        );
        assert!(!format!("{store:?}").contains("secret-value"));
        let reopened = FileCredentialStore::open(&path).unwrap();
        assert_eq!(
            reopened.resolve(&CredentialRef::new("test")).unwrap(),
            "secret-value"
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn credential_store_handles_missing_entries_removal_and_malformed_files() {
        let suffix = loom_core::ToolCallId::new().to_string();
        let directory = std::env::temp_dir().join(format!("loom-credential-errors-{suffix}"));
        let path = directory.join("credentials.json");
        let store = FileCredentialStore::open(&path).unwrap();
        let reference = CredentialRef::new("provider");
        assert_eq!(
            store.resolve(&reference).unwrap_err().code,
            ErrorCode::ProviderAuthentication
        );
        assert!(!store.remove(&reference).unwrap());
        store
            .insert(reference.clone(), "secret".to_owned())
            .unwrap();
        assert!(store.remove(&reference).unwrap());
        assert_eq!(
            FileCredentialStore::open(&path)
                .unwrap()
                .resolve(&reference)
                .unwrap_err()
                .code,
            ErrorCode::ProviderAuthentication
        );
        fs::write(&path, b"not-json").unwrap();
        assert_eq!(
            FileCredentialStore::open(&path).unwrap_err().code,
            ErrorCode::MalformedPayload
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn github_copilot_exchanges_github_token_before_chat_completion() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut token_stream, _) = listener.accept().unwrap();
            let token_request = read_request_headers(&mut token_stream).unwrap();
            assert!(token_request.lines().any(|line| {
                line.to_ascii_lowercase()
                    .starts_with("authorization: token ")
            }));
            let token_body =
                format!(r#"{{"token":"copilot-token","endpoints":{{"api":"http://{address}"}}}}"#);
            write_response(&mut token_stream, "application/json", &token_body).unwrap();

            let (mut chat_stream, _) = listener.accept().unwrap();
            let chat_request = read_request_headers(&mut chat_stream)
                .unwrap()
                .to_ascii_lowercase();
            assert!(
                chat_request
                    .lines()
                    .any(|line| line.starts_with("authorization: bearer "))
            );
            assert!(chat_request.contains("editor-version: vscode/1.96.2"));
            let chat_body = r#"{"choices":[{"message":{"content":"copilot response"},"finish_reason":"stop"}]}"#;
            write_response(&mut chat_stream, "application/json", chat_body).unwrap();
        });
        let descriptor = ModelDescriptor {
            id: ModelId::new("gpt-4o"),
            provider: ProviderId::new(GITHUB_COPILOT_PROVIDER_ID),
            display_name: "GitHub Copilot".to_owned(),
            context_window: Some(128_000),
            capabilities: ModelCapabilities::default(),
        };
        let mut provider = GitHubCopilotProvider::with_endpoints(
            format!("http://{address}"),
            format!("http://{address}/token"),
            "github-token",
            descriptor,
        );
        let events = collect(
            &mut provider,
            &ModelRequest {
                model: ModelId::new("gpt-4o"),
                messages: vec![ModelMessage::new(MessageRole::User, "hello")],
                tools: Vec::new(),
                options: Default::default(),
            },
        );
        assert!(events.iter().any(|event| {
            matches!(
                event,
                ModelStreamEvent::TextDelta { text } if text == "copilot response"
            )
        }));
        server.join().unwrap();
    }
}
