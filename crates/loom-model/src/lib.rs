mod config;
mod provider;

pub use config::{
    CredentialRef, CredentialReference, DEEPSEEK_API_ENDPOINT, DEEPSEEK_DEFAULT_MODEL,
    DEEPSEEK_PROVIDER_ID, GITHUB_COPILOT_API_ENDPOINT, GITHUB_COPILOT_CREDENTIAL_REF,
    GITHUB_COPILOT_DEFAULT_MODEL, GITHUB_COPILOT_PROVIDER_ID, GITHUB_REPOSITORY_CREDENTIAL_REF,
    OPENAI_API_ENDPOINT, OPENAI_DEFAULT_MODEL, OPENAI_PROVIDER_ID, ProviderConfig,
    ProviderUsageKey, UsageLedger, deterministic_descriptor, github_copilot_descriptor,
};
pub use provider::{
    CancellationToken, CollectingSink, ModelProvider, ModelStreamSink, ProviderDescriptor,
    ProviderHealth, ProviderHealthState, ProviderKind, ProviderSummary, ProviderUsageRecord,
    ProviderUsageSummary, StreamFlow, UnavailableProvider, cancelled_error,
    estimate_message_tokens, estimate_tokens,
};

use loom_core::ToolCallId;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ProviderId(String);

impl ProviderId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ProviderId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for ProviderId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ModelId(String);

impl ModelId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ModelId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for ModelId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelCapabilities {
    pub streaming: bool,
    pub tool_calling: bool,
    pub vision: bool,
    pub json_mode: bool,
}

impl ModelCapabilities {
    pub fn intersect(self, other: Self) -> Self {
        Self {
            streaming: self.streaming && other.streaming,
            tool_calling: self.tool_calling && other.tool_calling,
            vision: self.vision && other.vision,
            json_mode: self.json_mode && other.json_mode,
        }
    }

    pub fn intersection(&self, other: &Self) -> Self {
        self.clone().intersect(other.clone())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelDescriptor {
    pub id: ModelId,
    pub provider: ProviderId,
    pub display_name: String,
    pub context_window: Option<u32>,
    /// Maximum prompt size advertised by the provider for this model.
    #[serde(default)]
    pub max_input_tokens: Option<u32>,
    /// Maximum completion size advertised by the provider for this model.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    pub capabilities: ModelCapabilities,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelMessage {
    pub role: MessageRole,
    pub content: String,
    pub name: Option<String>,
    pub tool_call_id: Option<ToolCallId>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
}

impl ModelMessage {
    pub fn new(role: MessageRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct CompletionOptions {
    pub temperature: Option<f32>,
    pub max_output_tokens: Option<u32>,
    pub stop_sequences: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ModelRequest {
    pub model: ModelId,
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<ToolDefinition>,
    pub options: CompletionOptions,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    ToolCall,
    Length,
    Cancelled,
    Error,
    ErrorWithMessage { message: String },
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ModelStreamEvent {
    TextDelta {
        text: String,
    },
    /// Provider-reported reasoning/thinking summary. Optional and never
    /// required by the agent runtime; providers that do not expose it simply
    /// never emit this variant.
    ReasoningDelta {
        text: String,
    },
    ToolCallDelta {
        call: ToolCall,
    },
    Usage {
        usage: TokenUsage,
    },
    Completed {
        reason: FinishReason,
    },
}

#[cfg(test)]
mod tests {
    use super::{
        CompletionOptions, FinishReason, MessageRole, ModelCapabilities, ModelId, ModelMessage,
        ModelStreamEvent, ProviderId, TokenUsage,
    };

    #[test]
    fn identifiers_and_messages_serialize_as_stable_values() {
        assert_eq!(ModelId::from("model-x").as_str(), "model-x");
        assert_eq!(
            ProviderId::from(String::from("provider-y")).as_str(),
            "provider-y"
        );
        let message = ModelMessage::new(MessageRole::User, "hello");
        assert_eq!(message.content, "hello");
        assert!(message.name.is_none());
        assert!(message.tool_calls.is_empty());
        assert_eq!(serde_json::to_value(&message).unwrap()["role"], "user");
        assert_eq!(
            serde_json::from_value::<MessageRole>(serde_json::json!("assistant")).unwrap(),
            MessageRole::Assistant
        );
        assert_eq!(
            serde_json::to_value(ModelStreamEvent::Completed {
                reason: FinishReason::ToolCall
            })
            .unwrap(),
            serde_json::json!({ "type": "completed", "data": { "reason": "tool_call" } })
        );
    }

    #[test]
    fn model_descriptors_accept_persisted_records_without_advertised_limits() {
        let descriptor: super::ModelDescriptor = serde_json::from_value(serde_json::json!({
            "id": "fixture/model",
            "provider": "fixture",
            "display_name": "Fixture model",
            "context_window": 8192,
            "capabilities": {
                "streaming": false,
                "tool_calling": false,
                "vision": false,
                "json_mode": false
            }
        }))
        .unwrap();
        assert_eq!(descriptor.max_input_tokens, None);
        assert_eq!(descriptor.max_output_tokens, None);
    }

    #[test]
    fn model_capabilities_intersect_every_supported_feature() {
        let all = ModelCapabilities {
            streaming: true,
            tool_calling: true,
            vision: true,
            json_mode: true,
        };
        let partial = ModelCapabilities {
            streaming: true,
            tool_calling: false,
            vision: true,
            json_mode: false,
        };
        assert_eq!(all.clone().intersection(&partial), partial);
        assert_eq!(
            all.intersect(ModelCapabilities::default()),
            ModelCapabilities::default()
        );
        assert_eq!(
            serde_json::from_value::<CompletionOptions>(serde_json::json!({
                "stop_sequences": []
            }))
            .unwrap(),
            CompletionOptions {
                stop_sequences: Vec::new(),
                ..CompletionOptions::default()
            }
        );
        assert_eq!(TokenUsage::default().cached_input_tokens, 0);
    }
}
