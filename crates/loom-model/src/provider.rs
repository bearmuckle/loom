use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use loom_core::{LoomError, Result, Timestamp};
use serde::{Deserialize, Serialize};

use crate::{
    ModelCapabilities, ModelDescriptor, ModelId, ModelMessage, ModelRequest, ModelStreamEvent,
    ProviderId, TokenUsage,
};

/// Cooperative cancellation shared between a run and the provider serving it.
///
/// A provider must observe the token while it streams so an interrupt reaches
/// an in-flight completion instead of waiting for it to finish.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Returns `Err(LoomError::cancelled)` once the token has been cancelled.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            return Err(cancelled_error());
        }
        Ok(())
    }
}

pub fn cancelled_error() -> LoomError {
    LoomError::new(
        loom_core::ErrorCode::RequestCancelled,
        "the model request was cancelled",
        false,
    )
}

/// Tells a provider whether the consumer wants more events.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamFlow {
    Continue,
    Stop,
}

/// Sink for incremental model events.
///
/// Providers call this for every event as soon as it is decoded, so the agent
/// runtime can journal a delta before the completion has finished.
pub trait ModelStreamSink {
    fn emit(&mut self, event: ModelStreamEvent) -> Result<StreamFlow>;
}

impl<F> ModelStreamSink for F
where
    F: FnMut(ModelStreamEvent) -> Result<StreamFlow>,
{
    fn emit(&mut self, event: ModelStreamEvent) -> Result<StreamFlow> {
        self(event)
    }
}

/// Collects events into a vector. Used by tests and by callers that genuinely
/// want the whole completion before acting on it.
#[derive(Clone, Debug, Default)]
pub struct CollectingSink {
    pub events: Vec<ModelStreamEvent>,
}

impl ModelStreamSink for CollectingSink {
    fn emit(&mut self, event: ModelStreamEvent) -> Result<StreamFlow> {
        self.events.push(event);
        Ok(StreamFlow::Continue)
    }
}

/// The normalized model interface the agent runtime talks to.
///
/// Adapters in `loom-providers` translate provider wire formats into this
/// interface; the abstraction itself stays free of transport dependencies.
pub trait ModelProvider: Send {
    fn descriptor(&self) -> &ModelDescriptor;

    /// Streams a completion, emitting each event through `sink` as it arrives.
    ///
    /// Implementations must return as soon as `cancel` is cancelled or the sink
    /// asks to stop.
    fn stream(
        &mut self,
        request: &ModelRequest,
        cancel: &CancellationToken,
        sink: &mut dyn ModelStreamSink,
    ) -> Result<()>;

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

impl dyn ModelProvider + '_ {
    /// Convenience for callers that want the whole completion.
    pub fn stream_collected(
        &mut self,
        request: &ModelRequest,
        cancel: &CancellationToken,
    ) -> Result<Vec<ModelStreamEvent>> {
        let mut sink = CollectingSink::default();
        self.stream(request, cancel, &mut sink)?;
        Ok(sink.events)
    }
}

/// A provider that is configured but cannot serve requests.
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

    fn stream(
        &mut self,
        _request: &ModelRequest,
        _cancel: &CancellationToken,
        _sink: &mut dyn ModelStreamSink,
    ) -> Result<()> {
        Err(self.error.clone())
    }

    fn health_check(&mut self) -> Result<()> {
        Err(self.error.clone())
    }
}

pub fn estimate_tokens(request: &ModelRequest) -> u64 {
    let message_tokens = request
        .messages
        .iter()
        .map(|message| estimate_message_tokens_with_model(&request.model, message))
        .sum::<u64>();
    let tool_tokens = request
        .tools
        .iter()
        .map(|tool| {
            estimate_text_tokens(&request.model, &tool.description)
                + estimate_text_tokens(&request.model, &tool.input_schema.to_string())
                + 1
        })
        .sum::<u64>();
    message_tokens.saturating_add(tool_tokens)
}

pub fn estimate_message_tokens(message: &ModelMessage) -> u64 {
    estimate_message_tokens_with_model(&ModelId::new("gpt-4"), message)
}

fn estimate_message_tokens_with_model(model: &ModelId, message: &ModelMessage) -> u64 {
    let mut tokens = estimate_text_tokens(model, &message.content) + 3;
    // Thinking providers round-trip `reasoning_content` on assistant turns, so
    // it occupies context and must be budgeted. Omitting it let compaction and
    // the final budget check undercount thinking-mode requests.
    if let Some(reasoning) = &message.reasoning_content {
        tokens = tokens.saturating_add(estimate_text_tokens(model, reasoning));
    }
    if let Some(name) = &message.name {
        tokens = tokens.saturating_add(estimate_text_tokens(model, name) + 1);
    }
    if let Some(tool_call_id) = message.tool_call_id {
        tokens = tokens.saturating_add(estimate_text_tokens(model, &tool_call_id.to_string()));
    }
    tokens.saturating_add(
        message
            .tool_calls
            .iter()
            .map(|call| {
                estimate_text_tokens(model, &call.name)
                    + estimate_text_tokens(model, &call.arguments.to_string())
                    + 1
            })
            .sum::<u64>(),
    )
}

fn estimate_text_tokens(model: &ModelId, text: &str) -> u64 {
    let tokenizer = tiktoken_rs::bpe_for_model(model.as_str())
        .unwrap_or_else(|_| tiktoken_rs::cl100k_base_singleton());
    tokenizer.count_with_special_tokens(text) as u64
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Deterministic,
    OpenAi,
    OpenAiCompatible,
    #[serde(rename = "deepseek")]
    DeepSeek,
    Ollama,
    GitHubCopilot,
}

impl ProviderKind {
    /// Official hosted APIs cannot serve requests without a stored credential.
    /// Local runtimes, self-hosted gateways, and the deterministic demo do not
    /// require one.
    pub fn requires_credential(self) -> bool {
        matches!(self, ProviderKind::OpenAi | ProviderKind::DeepSeek)
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
    /// Whether this backend supports configuring this provider with an API key.
    /// Missing on older peers, where it defaults to false.
    #[serde(default)]
    pub api_key_configurable: bool,
    pub health: ProviderHealth,
}

impl ProviderSummary {
    /// Whether this provider can currently serve model requests. Providers
    /// that require a credential but have none are not usable, so their seeded
    /// models must stay out of the catalog until a key is configured.
    pub fn is_usable(&self) -> bool {
        !self.kind.requires_credential() || self.credential_id.is_some()
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MessageRole;

    #[test]
    fn reasoning_content_counts_toward_the_message_budget() {
        let mut message = ModelMessage::new(MessageRole::Assistant, "answer");
        let base = estimate_message_tokens(&message);
        message.reasoning_content = Some("thinking ".repeat(200));
        let with_reasoning = estimate_message_tokens(&message);
        assert!(
            with_reasoning > base,
            "reasoning_content must be budgeted: {with_reasoning} vs {base}"
        );
        // Empty reasoning (echoed onto turns that produced none) adds nothing.
        message.reasoning_content = Some(String::new());
        assert_eq!(estimate_message_tokens(&message), base);
    }
}
