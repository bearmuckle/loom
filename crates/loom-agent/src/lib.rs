use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use loom_context::{ContextAssembler, ContextAssemblyOptions, ContextInput, ContextInspection};
use loom_core::{
    ActivityId, AgentMessageRecord, AgentSessionId, ApprovalPolicy, ErrorCode, EvidenceLink,
    InteractionId, LimitKind, LimitStatus, LoomError, PolicyEvaluation, Result, RunId,
    SessionLimits, StepId, Timestamp, UsageSnapshot,
};
pub use loom_model::{
    AgentActivityData, AgentActivityKind, AgentActivityRecord, AgentActivityStatus, AgentEvent,
    AgentInteractionRecord, AgentPlan, AgentPlanStep, AgentRunSnapshot, AgentRunState,
    ApprovalDecision, FileActivityOperation, ProjectJoinContinuation,
};
use loom_model::{
    CancellationToken, CompletionOptions, MessageRole, ModelId, ModelMessage, ModelProvider,
    ModelRequest, ModelStreamEvent, StreamFlow, ToolCall,
};
use loom_tools::{ToolExecutor, ToolKind, ToolResult};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

mod accessors;
mod activity;
mod control;
mod events;
mod finalize;
mod lifecycle;
mod pool;
mod run_control;
mod runtime_types;
mod state;
mod steps;
mod tools;

use activity::*;
use run_control::PendingApproval;
pub use run_control::{AgentEventObserver, RunControl, RunProgress};
pub use runtime_types::{AgentRuntimeOptions, AgentRuntimeState, AgentTask};

const CONTEXT_PROJECTION_VERSION: u32 = 1;
const MAX_INTERACTION_PROMPT_BYTES: usize = 65_536;
/// How many times a single output-token truncation is resumed before the run
/// gives up. Each resume costs a model turn, so keep it small.
const MAX_OUTPUT_TRUNCATION_RETRIES: u32 = 3;
/// How many consecutive tool calls whose argument payload could not be decoded
/// are answered with a tool error before the run gives up. Each answered call
/// costs a model turn to resend, so keep it small.
const MAX_INVALID_TOOL_CALL_RETRIES: u32 = 3;

/// How many consecutive turns that end without a goal claim are answered with a
/// "keep working" continuation before the run completes anyway. Each
/// continuation costs a model turn, so keep it small; real tool work between
/// turns resets the count.
const MAX_GOAL_CONTINUATIONS: u32 = 3;
/// Longest goal claim summary accepted from the model, in bytes. It is a short
/// statement of evidence, not a second transcript.
const MAX_GOAL_CLAIM_BYTES: usize = 4096;
/// Name of the hidden user turn that tells a goal-driven run to keep working.
/// A transcript renders it as scaffolding rather than as a user message.
const GOAL_CONTINUATION_MESSAGE: &str = "loom_goal_continuation";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StepOutcome {
    Continue,
    Blocked,
}

/// Mutable state of one model step while its stream is being consumed.
struct StepContext {
    step_id: StepId,
    step_index: u32,
    events: Vec<AgentEvent>,
    published: usize,
    saw_tool_call: bool,
    completed: bool,
    completion_guarded: bool,
    /// Set when an unclaimed goal was answered with a continuation, so the run
    /// makes another model request instead of ending after the completed step.
    continue_after_goal: bool,
    /// Set when a length-truncated turn was resumed, so the run makes another
    /// model request instead of ending after the completed step.
    continue_after_truncation: bool,
    finished: bool,
    activity_id: ActivityId,
    buffered_tools: Vec<ToolCall>,
}

impl StepContext {
    fn new(step_id: StepId, step_index: u32, activity_id: ActivityId) -> Self {
        Self {
            step_id,
            step_index,
            events: Vec::new(),
            published: 0,
            saw_tool_call: false,
            completed: false,
            completion_guarded: false,
            continue_after_goal: false,
            continue_after_truncation: false,
            finished: false,
            activity_id,
            buffered_tools: Vec::new(),
        }
    }
}

pub struct AgentRuntime {
    session_id: AgentSessionId,
    task: AgentTask,
    run: AgentRunSnapshot,
    attempts: Vec<loom_model::AgentRunAttemptRecord>,
    plan: AgentPlan,
    provider: Option<Box<dyn ModelProvider>>,
    tools: ToolExecutor,
    messages: Vec<ModelMessage>,
    message_timeline_ordinals: Vec<u64>,
    next_timeline_ordinal: u64,
    last_project_message_sequence: u64,
    last_queued_direction_sequence: u64,
    pending_approval: Option<PendingApproval>,
    pending_tool_execution: Option<ToolCall>,
    pending_project_join: Option<ProjectJoinContinuation>,
    pending_input: Option<String>,
    last_failed_call: Option<ToolCall>,
    next_message_id: u64,
    active_message_id: Option<u64>,
    approval_policy: ApprovalPolicy,
    options: AgentRuntimeOptions,
    usage: UsageSnapshot,
    context_inspection: Option<ContextInspection>,
    context_checkpoint: Option<loom_core::ContextSummary>,
    provider_cursor: usize,
    step_id: Option<StepId>,
    step_index: u32,
    activities: Vec<AgentActivityRecord>,
    interactions: Vec<AgentInteractionRecord>,
    /// Signatures of tool calls the user rejected this run. Re-requesting one is
    /// answered without prompting again so a rejection cannot loop.
    denied_tool_calls: BTreeSet<String>,
    /// Consecutive `FinishReason::Length` continuations for this run. Reset by a
    /// step that produces a tool call or a normal completion so the bound only
    /// applies to a single truncated turn.
    output_truncation_retries: u32,
    /// Consecutive tool calls whose argument payload could not be decoded and
    /// were answered with a tool error. Reset by a decodable tool call so the
    /// bound only applies to a run of undecodable calls.
    invalid_tool_call_retries: u32,
    /// The session's stated goal, refreshed from the durable session record for
    /// every model turn. A goal-driven run keeps working until it claims the
    /// goal is reached.
    goal: Option<String>,
    /// What the model stated when it claimed the goal is reached, or `None`
    /// while the goal is still open. Runtime-only: a restored run is told its
    /// goal again and re-claims it before finishing.
    goal_claim: Option<String>,
    /// Consecutive turns that ended without a goal claim. A turn that performs
    /// tool work resets it, so the bound only applies to a run of empty stops.
    goal_continuations: u32,
    control: RunControl,
    observer: Option<AgentEventObserver>,
    flush_offset: usize,
    /// Bounded, shared pool for concurrent read-only tool calls.
    read_pool: Arc<pool::BoundedPool>,
}

#[cfg(test)]
mod tests;
