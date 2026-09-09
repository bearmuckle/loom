use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex},
};

use loom_core::{ErrorCode, LoomError, Result, Timestamp, ToolCallId, UsageSnapshot};
use loom_model::{
    FinishReason, MessageRole, ModelCapabilities, ModelDescriptor, ModelId, ModelMessage,
    ModelRequest, ModelStreamEvent, ProviderId, TokenUsage, ToolCall,
};
use serde::{Deserialize, Serialize};

pub trait ModelProvider: Send {
    fn descriptor(&self) -> &ModelDescriptor;

    fn stream(&mut self, request: &ModelRequest) -> Result<Vec<ModelStreamEvent>>;

    fn list_models(&self) -> Vec<ModelDescriptor> {
        vec![self.descriptor().clone()]
    }

    fn capabilities(&self) -> ModelCapabilities {
        self.descriptor().capabilities.clone()
    }

    fn count_tokens(&self, request: &ModelRequest) -> u64 {
        estimate_tokens(request)
    }

    fn health_check(&mut self) -> Result<()> {
        Ok(())
    }

    fn reset(&mut self) {}
}

pub struct UnavailableProvider {
    descriptor: ModelDescriptor,
    error: LoomError,
}

impl UnavailableProvider {
    pub fn new(descriptor: ModelDescriptor, error: LoomError) -> Self {
        Self { descriptor, error }
    }
}

impl ModelProvider for UnavailableProvider {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn stream(&mut self, _request: &ModelRequest) -> Result<Vec<ModelStreamEvent>> {
        Err(self.error.clone())
    }

    fn health_check(&mut self) -> Result<()> {
        Err(self.error.clone())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Deterministic,
    OpenAiCompatible,
    Ollama,
}

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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderHealth {
    pub state: ProviderHealthState,
    pub checked_at: Option<Timestamp>,
    pub consecutive_failures: u32,
    pub last_error: Option<LoomError>,
}

impl Default for ProviderHealth {
    fn default() -> Self {
        Self {
            state: ProviderHealthState::Unknown,
            checked_at: None,
            consecutive_failures: 0,
            last_error: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderHealthState {
    Unknown,
    Healthy,
    Degraded,
    Unavailable,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderSummary {
    pub id: ProviderId,
    pub kind: ProviderKind,
    pub display_name: String,
    pub models: Vec<ModelDescriptor>,
    pub credential_id: Option<String>,
    pub health: ProviderHealth,
}

pub type ProviderDescriptor = ProviderSummary;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderUsageSummary {
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub cost_micros: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderUsageRecord {
    pub provider: ProviderId,
    pub model: ModelId,
    pub usage: TokenUsage,
    pub cost_micros: u64,
    pub recorded_at: Timestamp,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct UsageLedger {
    pub records: Vec<ProviderUsageRecord>,
}

impl UsageLedger {
    pub fn record(
        &mut self,
        provider: ProviderId,
        model: ModelId,
        usage: TokenUsage,
        cost_micros: u64,
    ) {
        self.records.push(ProviderUsageRecord {
            provider,
            model,
            usage,
            cost_micros,
            recorded_at: Timestamp::now(),
        });
    }

    pub fn summary(
        &self,
        provider: Option<&ProviderId>,
        model: Option<&ModelId>,
    ) -> ProviderUsageSummary {
        self.records
            .iter()
            .filter(|record| {
                provider.is_none_or(|provider| &record.provider == provider)
                    && model.is_none_or(|model| &record.model == model)
            })
            .fold(ProviderUsageSummary::default(), |mut summary, record| {
                summary.requests = summary.requests.saturating_add(1);
                summary.input_tokens = summary
                    .input_tokens
                    .saturating_add(record.usage.input_tokens);
                summary.output_tokens = summary
                    .output_tokens
                    .saturating_add(record.usage.output_tokens);
                summary.cached_input_tokens = summary
                    .cached_input_tokens
                    .saturating_add(record.usage.cached_input_tokens);
                summary.cost_micros = summary.cost_micros.saturating_add(record.cost_micros);
                summary
            })
    }

    pub fn session_usage(&self) -> UsageSnapshot {
        self.records
            .iter()
            .fold(UsageSnapshot::default(), |mut usage, record| {
                usage.add_tokens(
                    record.usage.input_tokens,
                    record.usage.output_tokens,
                    record.usage.cached_input_tokens,
                );
                usage.add_cost_micros(record.cost_micros);
                usage
            })
    }
}

#[derive(Clone)]
pub struct ProviderRegistry {
    configurations: Arc<Mutex<BTreeMap<ProviderId, ProviderConfig>>>,
    credentials: Arc<dyn CredentialStore>,
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
            health: Arc::new(Mutex::new(BTreeMap::new())),
            usage: Arc::new(Mutex::new(UsageLedger::default())),
        }
    }

    pub fn demo() -> Self {
        let credentials = Arc::new(InMemoryCredentialStore::default());
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
            .map(|config| ProviderSummary {
                id: config.id.clone(),
                kind: config.kind,
                display_name: config.display_name.clone(),
                models: config.models.clone(),
                credential_id: config
                    .credential
                    .as_ref()
                    .map(|reference| reference.as_str().to_owned()),
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
            .flat_map(|config| config.models.iter().cloned())
            .collect())
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
        let endpoint = config.endpoint.as_deref().ok_or_else(|| {
            LoomError::invalid_request(format!(
                "provider '{}' has no endpoint",
                provider_id.as_str()
            ))
        })?;
        let credential = config
            .credential
            .as_ref()
            .map(|reference| self.credentials.resolve(reference))
            .transpose()?
            .unwrap_or_default();
        // Credentials are attached only to the backend request.
        let request = ureq::get(&health_endpoint(endpoint));
        let request = if credential.is_empty() {
            request
        } else {
            request.set("Authorization", &format!("Bearer {credential}"))
        };
        let response = request.call().map_err(|error| match error {
            ureq::Error::Status(status, _) => {
                normalize_provider_error(provider_id.as_str(), status)
            }
            ureq::Error::Transport(error) => {
                normalize_transport_error(provider_id.as_str(), &error.to_string())
            }
        })?;
        let body: serde_json::Value = response.into_json().map_err(|error| {
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
                    context_window: config.models.first().and_then(|model| model.context_window),
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
                    .map(|reference| self.credentials.resolve(reference))
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
        };
        Ok(Box::new(AccountingProvider {
            inner: provider,
            ledger: Arc::clone(&self.usage),
            input_cost_micros_per_1k: config.input_cost_micros_per_1k,
            output_cost_micros_per_1k: config.output_cost_micros_per_1k,
        }))
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
        for config in configs {
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

    fn stream(&mut self, request: &ModelRequest) -> Result<Vec<ModelStreamEvent>> {
        let events = self.inner.stream(request)?;
        let mut usage = TokenUsage::default();
        for event in &events {
            if let ModelStreamEvent::Usage { usage: event_usage } = event {
                usage.input_tokens = usage.input_tokens.saturating_add(event_usage.input_tokens);
                usage.output_tokens = usage
                    .output_tokens
                    .saturating_add(event_usage.output_tokens);
                usage.cached_input_tokens = usage
                    .cached_input_tokens
                    .saturating_add(event_usage.cached_input_tokens);
            }
        }
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
        Ok(events)
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

    fn stream(&mut self, _request: &ModelRequest) -> Result<Vec<ModelStreamEvent>> {
        let events = self.steps.get(self.cursor).cloned().unwrap_or_else(|| {
            vec![ModelStreamEvent::Completed {
                reason: FinishReason::Stop,
            }]
        });
        self.cursor = self.cursor.saturating_add(1);
        Ok(events)
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

    fn stream(&mut self, request: &ModelRequest) -> Result<Vec<ModelStreamEvent>> {
        if request.model != self.descriptor.id {
            return Err(LoomError::invalid_request(format!(
                "request model '{}' does not match provider model '{}'",
                request.model.as_str(),
                self.descriptor.id.as_str()
            )));
        }
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

        let request_builder = ureq::post(&self.endpoint).set("Content-Type", "application/json");
        let request_builder = if self.api_key.is_empty() {
            request_builder
        } else {
            request_builder.set("Authorization", &format!("Bearer {}", self.api_key))
        };
        let response = request_builder
            .send_json(payload)
            .map_err(|error| match error {
                ureq::Error::Status(status, _) => {
                    normalize_provider_error(self.descriptor.provider.as_str(), status)
                }
                ureq::Error::Transport(error) => {
                    normalize_transport_error(self.descriptor.provider.as_str(), &error.to_string())
                }
            })?;
        let body: serde_json::Value = response.into_json().map_err(|error| {
            LoomError::new(
                ErrorCode::ProviderInvalidResponse,
                format!(
                    "{} returned a response that was not valid JSON: {error}",
                    self.descriptor.provider.as_str()
                ),
                false,
            )
        })?;
        normalize_openai_response(&body)
    }

    fn health_check(&mut self) -> Result<()> {
        let response = ureq::get(&health_endpoint(&self.endpoint)).call();
        match response {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(status, _)) => Err(normalize_provider_error(
                self.descriptor.provider.as_str(),
                status,
            )),
            Err(ureq::Error::Transport(error)) => Err(normalize_transport_error(
                self.descriptor.provider.as_str(),
                &error.to_string(),
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

    fn stream(&mut self, request: &ModelRequest) -> Result<Vec<ModelStreamEvent>> {
        self.inner.stream(request)
    }

    fn count_tokens(&self, request: &ModelRequest) -> u64 {
        self.inner.count_tokens(request)
    }

    fn health_check(&mut self) -> Result<()> {
        self.inner.health_check()
    }
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

pub fn estimate_tokens(request: &ModelRequest) -> u64 {
    let message_chars = request
        .messages
        .iter()
        .map(|message| message.content.chars().count())
        .sum::<usize>();
    let tool_chars = request
        .tools
        .iter()
        .map(|tool| {
            tool.description.chars().count() + tool.input_schema.to_string().chars().count()
        })
        .sum::<usize>();
    ((message_chars.saturating_add(tool_chars) as u64).saturating_add(3)) / 4
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
    if let Some(content) = message.get("content").and_then(serde_json::Value::as_str) {
        if !content.is_empty() {
            events.push(ModelStreamEvent::TextDelta {
                text: content.to_owned(),
            });
        }
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
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    use super::*;

    #[test]
    fn deterministic_provider_streams_tool_calls_and_completion() {
        let mut provider = DeterministicProvider::demo();
        let request = ModelRequest {
            model: ModelId::new("deterministic/demo"),
            messages: Vec::new(),
            tools: Vec::new(),
            options: Default::default(),
        };

        let first = provider.stream(&request).unwrap();
        assert!(matches!(
            first.get(1),
            Some(ModelStreamEvent::ToolCallDelta { call })
                if call.name == "list_files"
        ));
        let final_step = (0..3)
            .map(|_| provider.stream(&request).unwrap())
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
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let read = stream.read(&mut request).unwrap();
            let request_text = String::from_utf8_lossy(&request[..read]);
            assert!(request_text.contains("raw-key-never-in-events"));
            let body = r#"{"choices":[{"message":{"content":"fixture response"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        let mut provider = OpenAiCompatibleProvider::new(
            format!("http://{address}/v1/chat/completions"),
            "raw-key-never-in-events",
            ModelId::new("fixture/model"),
        );
        let events = provider
            .stream(&ModelRequest {
                model: ModelId::new("fixture/model"),
                messages: vec![ModelMessage::new(MessageRole::User, "hello")],
                tools: Vec::new(),
                options: Default::default(),
            })
            .unwrap();
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
        server.join().unwrap();
        let redacted_error = normalize_transport_error("fixture", "Bearer raw-key-never-in-events");
        assert!(!redacted_error.message.contains("raw-key-never-in-events"));
    }

    #[test]
    fn registry_discovers_models_with_a_credential_reference() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let read = stream.read(&mut request).unwrap();
            assert!(String::from_utf8_lossy(&request[..read]).contains("discovery-key"));
            let body = r#"{"data":[{"id":"discovered/model"}]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
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
        assert_eq!(models[0].provider.as_str(), "gateway");
        assert_eq!(
            registry.pricing(&ModelId::new("discovered/model")).unwrap(),
            (100, 200)
        );
        server.join().unwrap();
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
}
