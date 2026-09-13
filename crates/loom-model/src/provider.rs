use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use loom_core::{LoomError, Result, Timestamp};
use serde::{Deserialize, Serialize};

use crate::{
    ModelCapabilities, ModelDescriptor, ModelId, ModelRequest, ModelStreamEvent, ProviderId,
    TokenUsage,
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
    let message_chars = request
        .messages
        .iter()
        .map(|message| {
            let metadata_chars = message.name.as_deref().map_or(0, str::len).saturating_add(
                message
                    .tool_call_id
                    .map_or(0, |tool_call_id| tool_call_id.to_string().len()),
            );
            let tool_call_chars = message
                .tool_calls
                .iter()
                .map(|call| {
                    call.name
                        .len()
                        .saturating_add(call.arguments.to_string().len())
                        .saturating_add(32)
                })
                .sum::<usize>();
            message
                .content
                .chars()
                .count()
                .saturating_add(metadata_chars)
                .saturating_add(tool_call_chars)
        })
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Deterministic,
    OpenAiCompatible,
    Ollama,
    GitHubCopilot,
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
