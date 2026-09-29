use std::collections::BTreeMap;

use loom_core::UsageSnapshot;
use serde::{Deserialize, Serialize};

use crate::{
    ModelCapabilities, ModelDescriptor, ModelId, ProviderId, ProviderKind, ProviderUsageSummary,
    TokenUsage,
};

pub const GITHUB_COPILOT_PROVIDER_ID: &str = "github-copilot";
pub const GITHUB_COPILOT_CREDENTIAL_REF: &str = "github-copilot";
pub const GITHUB_COPILOT_DEFAULT_MODEL: &str = "gpt-6-luna";
pub const OPENAI_PROVIDER_ID: &str = "openai";
pub const OPENAI_API_ENDPOINT: &str = "https://api.openai.com/v1/chat/completions";
pub const OPENAI_DEFAULT_MODEL: &str = "gpt-6-luna";
pub const GITHUB_COPILOT_API_ENDPOINT: &str = "https://api.githubcopilot.com";

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

    /// Official OpenAI API, exposed separately from OpenAI-compatible gateways.
    pub fn openai(model: impl Into<ModelId>) -> Self {
        let id = ProviderId::new(OPENAI_PROVIDER_ID);
        Self {
            id: id.clone(),
            kind: ProviderKind::OpenAi,
            display_name: "OpenAI".to_owned(),
            endpoint: Some(OPENAI_API_ENDPOINT.to_owned()),
            models: vec![ModelDescriptor {
                id: model.into(),
                provider: id,
                display_name: "OpenAI model".to_owned(),
                context_window: None,
                max_input_tokens: None,
                max_output_tokens: None,
                capabilities: ModelCapabilities {
                    streaming: true,
                    tool_calling: true,
                    vision: true,
                    json_mode: true,
                },
            }],
            credential: None,
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
            max_input_tokens: None,
            max_output_tokens: None,
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
        display_name: "GitHub Copilot GPT-6 Luna".to_owned(),
        context_window: Some(128_000),
        max_input_tokens: Some(8_192),
        max_output_tokens: Some(4_096),
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

pub fn deterministic_descriptor() -> ModelDescriptor {
    ModelDescriptor {
        id: ModelId::new("deterministic/demo"),
        provider: ProviderId::new("deterministic"),
        display_name: "Deterministic M1 demo".to_owned(),
        context_window: Some(16_384),
        max_input_tokens: None,
        max_output_tokens: None,
        capabilities: ModelCapabilities {
            streaming: true,
            tool_calling: true,
            vision: false,
            json_mode: true,
        },
    }
}
