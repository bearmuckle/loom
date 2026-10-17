use super::*;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentTask {
    pub task: String,
    pub model: ModelId,
    pub system_instructions: Option<String>,
    pub repository_instructions: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentRuntimeOptions {
    pub limits: SessionLimits,
    pub context: ContextAssemblyOptions,
    pub checkpoint_id: Option<loom_core::CheckpointId>,
    pub input_cost_micros_per_1k: u64,
    pub output_cost_micros_per_1k: u64,
    /// Persisted grant for server-provided project delegation tools.
    #[serde(default)]
    pub project_delegation_enabled: bool,
    /// Persisted grant for direct parent-child message tools.
    #[serde(default)]
    pub project_messaging_enabled: bool,
    /// Persisted grant for direct-child project status inspection.
    #[serde(default)]
    pub project_inspection_enabled: bool,
    /// Persisted grant for direct-child lifecycle controls.
    #[serde(default)]
    pub project_child_control_enabled: bool,
    /// Persisted grant for creating isolated project code-task worktrees.
    #[serde(default)]
    pub project_worktree_enabled: bool,
    /// Persisted grant for reviewing direct-child project worktrees.
    #[serde(default)]
    pub project_review_enabled: bool,
    /// Persisted grant for integrating reviewed child worktree commits.
    #[serde(default)]
    pub project_integration_enabled: bool,
    /// Persisted grant for explicitly authorized non-adjacent project messages.
    #[serde(default)]
    pub project_branch_messaging_enabled: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentRuntimeState {
    pub session_id: AgentSessionId,
    pub task: AgentTask,
    pub run: AgentRunSnapshot,
    pub plan: AgentPlan,
    pub messages: Vec<ModelMessage>,
    /// Run-wide order aligned with `messages` for deterministic transcript/activity replay.
    #[serde(default)]
    pub message_timeline_ordinals: Vec<u64>,
    #[serde(default)]
    pub last_project_message_sequence: u64,
    /// Delivery cursor for user directions queued while the run was executing.
    #[serde(default)]
    pub last_queued_direction_sequence: u64,
    #[serde(default)]
    pub attempts: Vec<loom_model::AgentRunAttemptRecord>,
    pub pending_approval: Option<ToolCall>,
    #[serde(default)]
    pub pending_tool_execution: Option<ToolCall>,
    /// A tool call waiting for a durable project join to finish.
    #[serde(default)]
    pub pending_project_join: Option<ProjectJoinContinuation>,
    #[serde(default)]
    pub pending_input: Option<String>,
    pub last_failed_call: Option<ToolCall>,
    pub next_message_id: u64,
    pub active_message_id: Option<u64>,
    pub approval_policy: ApprovalPolicy,
    pub options: AgentRuntimeOptions,
    pub usage: UsageSnapshot,
    pub context_inspection: Option<ContextInspection>,
    /// Summary and exclusive boundary in the repaired conversation history.
    #[serde(default)]
    pub context_checkpoint: Option<loom_core::ContextSummary>,
    pub provider_cursor: usize,
    pub step_id: Option<StepId>,
    pub step_index: u32,
    #[serde(default)]
    pub activities: Vec<AgentActivityRecord>,
    #[serde(default)]
    pub interactions: Vec<AgentInteractionRecord>,
}

impl AgentTask {
    pub fn new(task: impl Into<String>, model: ModelId) -> Result<Self> {
        let task = task.into();
        if task.trim().is_empty() {
            return Err(LoomError::invalid_request("agent task must not be empty"));
        }
        if model.as_str().trim().is_empty() {
            return Err(LoomError::invalid_request("agent model must not be empty"));
        }
        Ok(Self {
            task,
            model,
            system_instructions: None,
            repository_instructions: None,
        })
    }
}
