use loom_core::{
    AgentSessionId, CheckpointId, EvidenceLink, InteractionId, LimitStatus, LoomError,
    PolicyEvaluation, RunAttemptId, RunId, StepId, Timestamp, ToolCallId, UsageSnapshot,
};
use loom_model::{ModelId, TokenUsage, ToolCall};
use serde::{Deserialize, Serialize};

use crate::{AgentActivityRecord, ContextInspection, ToolResult};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRunState {
    Planning,
    Executing,
    AwaitingApproval,
    Paused,
    NeedsInput,
    Evaluating,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentRunSnapshot {
    pub id: RunId,
    pub attempt_id: RunAttemptId,
    pub control_revision: u64,
    pub session_id: AgentSessionId,
    pub task: String,
    pub model: ModelId,
    pub state: AgentRunState,
    pub started_at: Timestamp,
    pub updated_at: Timestamp,
    pub completed_at: Option<Timestamp>,
    pub summary: Option<String>,
    #[serde(default)]
    pub evidence: Vec<EvidenceLink>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentRunAttemptRecord {
    pub run_id: RunId,
    pub session_id: AgentSessionId,
    pub id: RunAttemptId,
    pub number: u32,
    pub state: AgentRunState,
    pub checkpoint_id: Option<CheckpointId>,
    pub started_at: Timestamp,
    pub completed_at: Option<Timestamp>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentExecutionStateRecord {
    pub run_id: RunId,
    pub session_id: AgentSessionId,
    pub attempt_id: RunAttemptId,
    pub control_revision: u64,
    pub state: AgentRunState,
    pub step_id: Option<StepId>,
    pub step_index: u32,
    pub provider_cursor: u64,
    pub next_message_id: u64,
    pub active_message_id: Option<u64>,
    #[serde(default)]
    pub last_project_message_sequence: u64,
    pub pending_tool_execution: Option<ToolCall>,
    #[serde(default)]
    pub pending_project_join: Option<ProjectJoinContinuation>,
    pub pending_approval: Option<ToolCall>,
    pub pending_input: Option<String>,
    pub last_failed_call: Option<ToolCall>,
}

/// Runtime continuation for a project join waiting on child agents.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectJoinContinuation {
    pub wait_id: String,
    pub call: ToolCall,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentToolCallRecord {
    pub run_id: RunId,
    pub session_id: AgentSessionId,
    pub call: ToolCall,
    pub created_at: Timestamp,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentToolAttemptState {
    Queued,
    Running,
    AwaitingApproval,
    AwaitingInput,
    Completed,
    Failed,
    Cancelled,
    OutcomeUnknown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentToolAttemptRecord {
    pub run_id: RunId,
    pub session_id: AgentSessionId,
    /// An activity ID gives each execution of the logical call a stable identity.
    pub id: loom_core::ActivityId,
    pub call_id: ToolCallId,
    pub attempt_number: u32,
    pub state: AgentToolAttemptState,
    pub started_at: Timestamp,
    pub completed_at: Option<Timestamp>,
    pub result: Option<ToolResult>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentPlan {
    pub steps: Vec<AgentPlanStep>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentPlanStep {
    pub id: String,
    pub description: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approved,
    Rejected,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentInteractionKind {
    ToolApproval,
    UserInput,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentInteractionStatus {
    Pending,
    Approved,
    Rejected,
    Answered,
    Abandoned,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentInteractionRecord {
    pub id: InteractionId,
    pub run_id: RunId,
    pub session_id: AgentSessionId,
    pub attempt_id: RunAttemptId,
    pub control_revision: u64,
    pub kind: AgentInteractionKind,
    pub status: AgentInteractionStatus,
    pub tool_call_id: Option<ToolCallId>,
    pub prompt: String,
    pub decision: Option<ApprovalDecision>,
    pub created_at: Timestamp,
    pub resolved_at: Option<Timestamp>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum AgentEvent {
    RunStarted {
        snapshot: AgentRunSnapshot,
    },
    PlanProposed {
        run_id: RunId,
        plan: AgentPlan,
    },
    StepStarted {
        run_id: RunId,
        step_id: StepId,
        index: u32,
    },
    StepCompleted {
        run_id: RunId,
        step_id: StepId,
        index: u32,
    },
    ContextInspected {
        run_id: RunId,
        inspection: ContextInspection,
    },
    ProviderError {
        run_id: RunId,
        error: LoomError,
    },
    ContextError {
        run_id: RunId,
        error: LoomError,
    },
    AssistantMessageDelta {
        run_id: RunId,
        message_id: u64,
        text: String,
    },
    UserMessage {
        run_id: RunId,
        attempt_id: RunAttemptId,
        control_revision: u64,
        interaction_id: Option<InteractionId>,
        text: String,
    },
    NeedsInput {
        run_id: RunId,
        attempt_id: RunAttemptId,
        control_revision: u64,
        interaction_id: InteractionId,
        prompt: String,
    },
    ToolCallRequested {
        run_id: RunId,
        call: ToolCall,
    },
    ToolApprovalRequired {
        run_id: RunId,
        attempt_id: RunAttemptId,
        control_revision: u64,
        interaction_id: InteractionId,
        call: ToolCall,
    },
    ToolPolicyEvaluated {
        run_id: RunId,
        call: ToolCall,
        evaluation: PolicyEvaluation,
    },
    ToolApprovalDecided {
        run_id: RunId,
        attempt_id: RunAttemptId,
        control_revision: u64,
        interaction_id: InteractionId,
        tool_call_id: ToolCallId,
        decision: ApprovalDecision,
    },
    ToolCallStarted {
        run_id: RunId,
        call: ToolCall,
    },
    ToolOutputChunk {
        run_id: RunId,
        tool_call_id: ToolCallId,
        chunk: String,
    },
    ToolCallCompleted {
        run_id: RunId,
        result: ToolResult,
    },
    ActivityRecorded {
        run_id: RunId,
        activity: AgentActivityRecord,
    },
    RunUsage {
        run_id: RunId,
        usage: TokenUsage,
    },
    RunUsageUpdated {
        run_id: RunId,
        usage: UsageSnapshot,
    },
    RunLimitReached {
        run_id: RunId,
        status: LimitStatus,
    },
    RecoveryRequired {
        run_id: RunId,
        reason: String,
    },
    RunStateChanged {
        run_id: RunId,
        state: AgentRunState,
    },
    RunCompleted {
        snapshot: AgentRunSnapshot,
    },
}
