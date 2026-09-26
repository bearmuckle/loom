use loom_core::{
    AgentSessionId, EvidenceLink, LimitStatus, LoomError, PolicyEvaluation, RunAttemptId, RunId,
    StepId, Timestamp, ToolCallId, UsageSnapshot,
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
        text: String,
    },
    NeedsInput {
        run_id: RunId,
        attempt_id: RunAttemptId,
        control_revision: u64,
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
