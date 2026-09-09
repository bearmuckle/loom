use loom_context::{ContextAssembler, ContextAssemblyOptions, ContextInput, ContextInspection};
use loom_core::{
    AgentSessionId, ApprovalPolicy, ErrorCode, LimitKind, LimitStatus, LoomError, PolicyEvaluation,
    Result, RunId, SessionLimits, StepId, Timestamp, UsageSnapshot,
};
use loom_model::{
    CompletionOptions, MessageRole, ModelId, ModelMessage, ModelRequest, ModelStreamEvent,
    TokenUsage, ToolCall,
};
use loom_providers::ModelProvider;
use loom_tools::{ToolExecutor, ToolKind, ToolResult, tool_definitions};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRunState {
    Planning,
    Executing,
    AwaitingApproval,
    Paused,
    Evaluating,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentRunSnapshot {
    pub id: RunId,
    pub session_id: AgentSessionId,
    pub task: String,
    pub model: ModelId,
    pub state: AgentRunState,
    pub started_at: Timestamp,
    pub updated_at: Timestamp,
    pub completed_at: Option<Timestamp>,
    pub summary: Option<String>,
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
    ToolCallRequested {
        run_id: RunId,
        call: ToolCall,
    },
    ToolApprovalRequired {
        run_id: RunId,
        call: ToolCall,
    },
    ToolPolicyEvaluated {
        run_id: RunId,
        call: ToolCall,
        evaluation: PolicyEvaluation,
    },
    ToolApprovalDecided {
        run_id: RunId,
        tool_call_id: loom_core::ToolCallId,
        decision: ApprovalDecision,
    },
    ToolCallStarted {
        run_id: RunId,
        call: ToolCall,
    },
    ToolOutputChunk {
        run_id: RunId,
        tool_call_id: loom_core::ToolCallId,
        chunk: String,
    },
    ToolCallCompleted {
        run_id: RunId,
        result: ToolResult,
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
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentRuntimeState {
    pub session_id: AgentSessionId,
    pub task: AgentTask,
    pub run: AgentRunSnapshot,
    pub plan: AgentPlan,
    pub messages: Vec<ModelMessage>,
    pub pending_approval: Option<ToolCall>,
    pub last_failed_call: Option<ToolCall>,
    pub next_message_id: u64,
    pub active_message_id: Option<u64>,
    pub approval_policy: ApprovalPolicy,
    pub options: AgentRuntimeOptions,
    pub usage: UsageSnapshot,
    pub context_inspection: Option<ContextInspection>,
    pub provider_cursor: usize,
    pub step_id: Option<StepId>,
    pub step_index: u32,
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

struct PendingApproval {
    call: ToolCall,
}

pub struct AgentRuntime {
    session_id: AgentSessionId,
    task: AgentTask,
    run: AgentRunSnapshot,
    plan: AgentPlan,
    provider: Box<dyn ModelProvider>,
    tools: ToolExecutor,
    messages: Vec<ModelMessage>,
    pending_approval: Option<PendingApproval>,
    last_failed_call: Option<ToolCall>,
    next_message_id: u64,
    active_message_id: Option<u64>,
    approval_policy: ApprovalPolicy,
    options: AgentRuntimeOptions,
    usage: UsageSnapshot,
    context_inspection: Option<ContextInspection>,
    provider_cursor: usize,
    step_id: Option<StepId>,
    step_index: u32,
}

impl AgentRuntime {
    pub fn new(
        session_id: AgentSessionId,
        task: AgentTask,
        provider: Box<dyn ModelProvider>,
        tools: ToolExecutor,
    ) -> Self {
        Self::new_with_options(
            session_id,
            task,
            provider,
            tools,
            ApprovalPolicy::default(),
            AgentRuntimeOptions::default(),
        )
    }

    pub fn new_with_options(
        session_id: AgentSessionId,
        task: AgentTask,
        provider: Box<dyn ModelProvider>,
        tools: ToolExecutor,
        approval_policy: ApprovalPolicy,
        options: AgentRuntimeOptions,
    ) -> Self {
        let now = Timestamp::now();
        let run = AgentRunSnapshot {
            id: RunId::new(),
            session_id,
            task: task.task.clone(),
            model: task.model.clone(),
            state: AgentRunState::Planning,
            started_at: now,
            updated_at: now,
            completed_at: None,
            summary: None,
        };
        let plan = AgentPlan {
            steps: vec![
                AgentPlanStep {
                    id: "inspect".to_owned(),
                    description: "Inspect the workspace and relevant files".to_owned(),
                },
                AgentPlanStep {
                    id: "change".to_owned(),
                    description: "Apply the focused repository change".to_owned(),
                },
                AgentPlanStep {
                    id: "validate".to_owned(),
                    description: "Run validation and report the result".to_owned(),
                },
            ],
        };
        let messages = initial_messages(&task);
        Self {
            session_id,
            task,
            run,
            plan,
            provider,
            tools,
            messages,
            pending_approval: None,
            last_failed_call: None,
            next_message_id: 0,
            active_message_id: None,
            approval_policy,
            options,
            usage: UsageSnapshot::default(),
            context_inspection: None,
            provider_cursor: 0,
            step_id: None,
            step_index: 0,
        }
    }

    pub fn new_with_policy(
        session_id: AgentSessionId,
        task: AgentTask,
        provider: Box<dyn ModelProvider>,
        tools: ToolExecutor,
        approval_policy: ApprovalPolicy,
    ) -> Self {
        Self::new_with_options(
            session_id,
            task,
            provider,
            tools,
            approval_policy,
            AgentRuntimeOptions::default(),
        )
    }

    pub fn new_with_policy_and_options(
        session_id: AgentSessionId,
        task: AgentTask,
        provider: Box<dyn ModelProvider>,
        tools: ToolExecutor,
        approval_policy: ApprovalPolicy,
        options: AgentRuntimeOptions,
    ) -> Self {
        Self::new_with_options(session_id, task, provider, tools, approval_policy, options)
    }

    pub fn run_id(&self) -> RunId {
        self.run.id
    }

    pub fn session_id(&self) -> AgentSessionId {
        self.session_id
    }

    pub fn snapshot(&self) -> AgentRunSnapshot {
        self.run.clone()
    }

    pub fn usage(&self) -> UsageSnapshot {
        self.usage.clone()
    }

    pub fn limits(&self) -> &SessionLimits {
        &self.options.limits
    }

    pub fn limit_status(&self) -> LimitStatus {
        let mut usage = self.usage.clone();
        usage.elapsed_ms = Timestamp::now()
            .as_unix_millis()
            .saturating_sub(self.run.started_at.as_unix_millis());
        LimitStatus::new(self.options.limits.clone(), usage)
    }

    pub fn context_inspection(&self) -> Option<ContextInspection> {
        self.context_inspection.clone()
    }

    pub fn messages(&self) -> Vec<ModelMessage> {
        self.messages.clone()
    }

    pub fn checkpoint_id(&self) -> Option<loom_core::CheckpointId> {
        self.options.checkpoint_id
    }

    pub fn export_state(&self) -> AgentRuntimeState {
        AgentRuntimeState {
            session_id: self.session_id,
            task: self.task.clone(),
            run: self.run.clone(),
            plan: self.plan.clone(),
            messages: self.messages.clone(),
            pending_approval: self
                .pending_approval
                .as_ref()
                .map(|pending| pending.call.clone()),
            last_failed_call: self.last_failed_call.clone(),
            next_message_id: self.next_message_id,
            active_message_id: self.active_message_id,
            approval_policy: self.approval_policy.clone(),
            options: self.options.clone(),
            usage: self.usage.clone(),
            context_inspection: self.context_inspection.clone(),
            provider_cursor: self.provider_cursor,
            step_id: self.step_id,
            step_index: self.step_index,
        }
    }

    pub fn from_state(
        state: AgentRuntimeState,
        provider: Box<dyn ModelProvider>,
        tools: ToolExecutor,
    ) -> Result<Self> {
        if state.session_id != state.run.session_id {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "agent runtime state session ids do not match",
                false,
            ));
        }
        if state.pending_approval.is_some()
            && !matches!(
                state.run.state,
                AgentRunState::AwaitingApproval | AgentRunState::Paused
            )
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted approval is attached to a run that is neither awaiting approval nor paused",
                false,
            ));
        }
        if provider.descriptor().id != state.task.model {
            return Err(LoomError::new(
                ErrorCode::ProviderUnavailable,
                format!(
                    "provider model '{}' does not match persisted model '{}'",
                    provider.descriptor().id.as_str(),
                    state.task.model.as_str()
                ),
                false,
            ));
        }
        Ok(Self {
            session_id: state.session_id,
            task: state.task,
            run: state.run,
            plan: state.plan,
            provider,
            tools,
            messages: state.messages,
            pending_approval: state.pending_approval.map(|call| PendingApproval { call }),
            last_failed_call: state.last_failed_call,
            next_message_id: state.next_message_id,
            active_message_id: state.active_message_id,
            approval_policy: state.approval_policy,
            options: state.options,
            usage: state.usage,
            context_inspection: state.context_inspection,
            provider_cursor: state.provider_cursor,
            step_id: state.step_id,
            step_index: state.step_index,
        })
    }

    pub fn start(&mut self) -> Result<Vec<AgentEvent>> {
        if self.run.state != AgentRunState::Planning {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "agent run has already started",
                false,
            ));
        }
        let mut events = vec![
            AgentEvent::RunStarted {
                snapshot: self.run.clone(),
            },
            AgentEvent::PlanProposed {
                run_id: self.run.id,
                plan: self.plan.clone(),
            },
        ];
        events.extend(self.set_state(AgentRunState::Executing));
        events.extend(self.advance()?);
        Ok(events)
    }

    pub fn approve(&mut self, tool_call_id: loom_core::ToolCallId) -> Result<Vec<AgentEvent>> {
        if self.run.state != AgentRunState::AwaitingApproval {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "agent run is not waiting for tool approval",
                false,
            ));
        }
        let pending = self.take_pending(tool_call_id)?;
        let mut events = vec![AgentEvent::ToolApprovalDecided {
            run_id: self.run.id,
            tool_call_id,
            decision: ApprovalDecision::Approved,
        }];
        events.extend(self.set_state(AgentRunState::Executing));
        let (tool_events, result) = self.execute_tool(&pending.call);
        events.extend(tool_events);
        if !result.success {
            self.last_failed_call = Some(pending.call);
            events.extend(self.finish_failed(result.output));
            return Ok(events);
        }
        self.last_failed_call = None;
        events.extend(self.advance()?);
        Ok(events)
    }

    pub fn reject(
        &mut self,
        tool_call_id: loom_core::ToolCallId,
        reason: Option<String>,
    ) -> Result<Vec<AgentEvent>> {
        if self.run.state != AgentRunState::AwaitingApproval {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "agent run is not waiting for tool approval",
                false,
            ));
        }
        let pending = self.take_pending(tool_call_id)?;
        let mut events = vec![AgentEvent::ToolApprovalDecided {
            run_id: self.run.id,
            tool_call_id,
            decision: ApprovalDecision::Rejected,
        }];
        let output = reason.unwrap_or_else(|| "tool call rejected by the user".to_owned());
        let result = ToolResult {
            tool_call_id,
            name: pending.call.name,
            success: false,
            output: output.clone(),
        };
        events.push(AgentEvent::ToolCallCompleted {
            run_id: self.run.id,
            result,
        });
        events.extend(self.finish_failed(output));
        Ok(events)
    }

    pub fn interrupt(&mut self) -> Result<Vec<AgentEvent>> {
        if matches!(
            self.run.state,
            AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
        ) {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "agent run is already finished",
                false,
            ));
        }
        let mut events = self.set_state(AgentRunState::Cancelled);
        self.run.completed_at = Some(Timestamp::now());
        self.run.summary = Some("Agent run interrupted by the user".to_owned());
        events.push(AgentEvent::RunCompleted {
            snapshot: self.run.clone(),
        });
        Ok(events)
    }

    pub fn pause(&mut self) -> Result<Vec<AgentEvent>> {
        if matches!(
            self.run.state,
            AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
        ) {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "agent run is already finished",
                false,
            ));
        }
        if self.run.state == AgentRunState::Paused {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "agent run is already paused",
                false,
            ));
        }
        Ok(self.set_state(AgentRunState::Paused))
    }

    pub fn resume(&mut self) -> Result<Vec<AgentEvent>> {
        if self.run.state != AgentRunState::Paused {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "agent run is not paused",
                false,
            ));
        }
        let mut events = if self.pending_approval.is_some() {
            self.set_state(AgentRunState::AwaitingApproval)
        } else {
            self.set_state(AgentRunState::Executing)
        };
        if self.pending_approval.is_none() {
            events.extend(self.advance()?);
        }
        Ok(events)
    }

    pub fn recover_after_restart(&mut self) -> Result<Vec<AgentEvent>> {
        if matches!(
            self.run.state,
            AgentRunState::Planning | AgentRunState::Executing | AgentRunState::Evaluating
        ) {
            Ok(self.set_state(AgentRunState::Paused))
        } else {
            Ok(Vec::new())
        }
    }

    pub fn retry(&mut self) -> Result<Vec<AgentEvent>> {
        let call = self.last_failed_call.clone().ok_or_else(|| {
            LoomError::new(
                ErrorCode::InvalidState,
                "there is no failed tool step to retry",
                false,
            )
        })?;
        if self.run.state != AgentRunState::Failed {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "agent run is not waiting for a retry",
                false,
            ));
        }
        self.run.completed_at = None;
        self.run.summary = None;
        let mut events = self.set_state(AgentRunState::Executing);
        let (tool_events, result) = self.execute_tool(&call);
        events.extend(tool_events);
        if result.success {
            self.last_failed_call = None;
            events.extend(self.advance()?);
        } else {
            events.extend(self.finish_failed(result.output));
        }
        Ok(events)
    }

    pub fn retry_from_checkpoint(&mut self) -> Result<Vec<AgentEvent>> {
        if matches!(
            self.run.state,
            AgentRunState::Planning
                | AgentRunState::Executing
                | AgentRunState::AwaitingApproval
                | AgentRunState::Evaluating
                | AgentRunState::Paused
        ) {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "agent run must be stopped before retrying from a checkpoint",
                false,
            ));
        }
        self.run.completed_at = None;
        self.run.summary = None;
        self.run.updated_at = Timestamp::now();
        self.run.state = AgentRunState::Planning;
        self.messages = initial_messages(&self.task);
        self.pending_approval = None;
        self.last_failed_call = None;
        self.next_message_id = 0;
        self.active_message_id = None;
        self.usage = UsageSnapshot::default();
        self.context_inspection = None;
        self.provider.reset();
        self.provider_cursor = 0;
        self.step_id = None;
        self.step_index = 0;
        let mut events = vec![AgentEvent::RunStateChanged {
            run_id: self.run.id,
            state: AgentRunState::Planning,
        }];
        events.extend(self.advance()?);
        Ok(events)
    }

    fn advance(&mut self) -> Result<Vec<AgentEvent>> {
        let mut events = Vec::new();
        loop {
            if self.pending_approval.is_some()
                || matches!(
                    self.run.state,
                    AgentRunState::Completed
                        | AgentRunState::Failed
                        | AgentRunState::Cancelled
                        | AgentRunState::Paused
                )
            {
                return Ok(events);
            }
            if let Some(status) = self.exceeded_limits() {
                events.push(AgentEvent::RunLimitReached {
                    run_id: self.run.id,
                    status,
                });
                events.extend(self.finish_failed("agent session limit reached"));
                return Ok(events);
            }
            if self
                .messages
                .last()
                .is_some_and(|message| message.role == MessageRole::Tool)
            {
                events.extend(self.set_state(AgentRunState::Evaluating));
            } else if self.run.state != AgentRunState::Executing {
                events.extend(self.set_state(AgentRunState::Executing));
            }
            let (request, inspection) = match self.model_request() {
                Ok(request) => request,
                Err(error) => {
                    events.push(AgentEvent::ContextError {
                        run_id: self.run.id,
                        error: error.clone(),
                    });
                    if error.code == ErrorCode::ContextLimitExceeded {
                        let mut status = self.limit_status();
                        status.exceeded.push(LimitKind::ContextTokens);
                        events.push(AgentEvent::RunLimitReached {
                            run_id: self.run.id,
                            status,
                        });
                    }
                    events.extend(self.finish_failed(error.message));
                    return Ok(events);
                }
            };
            self.context_inspection = Some(inspection.clone());
            events.push(AgentEvent::ContextInspected {
                run_id: self.run.id,
                inspection,
            });
            let step_id = StepId::new();
            self.step_id = Some(step_id);
            let step_index = self.step_index;
            events.push(AgentEvent::StepStarted {
                run_id: self.run.id,
                step_id,
                index: step_index,
            });
            self.provider_cursor = self.provider_cursor.saturating_add(1);
            let stream = match self.provider.stream(&request) {
                Ok(stream) => stream,
                Err(error) => {
                    self.step_id = None;
                    events.push(AgentEvent::ProviderError {
                        run_id: self.run.id,
                        error: error.clone(),
                    });
                    events.extend(self.finish_failed(error.message));
                    return Ok(events);
                }
            };
            if stream.is_empty() {
                self.step_id = None;
                events.extend(self.finish_failed("model returned an empty stream"));
                return Ok(events);
            }
            let mut saw_tool_call = false;
            let mut completed = false;
            for item in stream {
                match item {
                    ModelStreamEvent::TextDelta { text } => {
                        if !text.is_empty() {
                            self.append_assistant_text(&text);
                            let message_id = self.assistant_message_id();
                            events.push(AgentEvent::AssistantMessageDelta {
                                run_id: self.run.id,
                                message_id,
                                text,
                            });
                        }
                    }
                    ModelStreamEvent::ToolCallDelta { call } => {
                        self.active_message_id = None;
                        if self
                            .options
                            .limits
                            .max_tool_calls
                            .is_some_and(|limit| self.usage.tool_calls >= limit)
                        {
                            let status = self.limit_status();
                            events.push(AgentEvent::RunLimitReached {
                                run_id: self.run.id,
                                status,
                            });
                            events.extend(self.finish_failed("agent session limit reached"));
                            self.step_id = None;
                            return Ok(events);
                        }
                        self.usage.add_tool_call();
                        events.push(AgentEvent::RunUsageUpdated {
                            run_id: self.run.id,
                            usage: self.usage.clone(),
                        });
                        saw_tool_call = true;
                        events.push(AgentEvent::ToolCallRequested {
                            run_id: self.run.id,
                            call: call.clone(),
                        });
                        let Some(kind) = ToolKind::from_name(&call.name) else {
                            let output = format!("unknown tool '{}'", call.name);
                            let result = ToolResult {
                                tool_call_id: call.id,
                                name: call.name.clone(),
                                success: false,
                                output: output.clone(),
                            };
                            events.push(AgentEvent::ToolCallCompleted {
                                run_id: self.run.id,
                                result: result.clone(),
                            });
                            self.messages.push(ModelMessage {
                                role: MessageRole::Tool,
                                content: output.clone(),
                                name: Some(result.name.clone()),
                                tool_call_id: Some(result.tool_call_id),
                            });
                            self.last_failed_call = Some(call);
                            self.step_id = None;
                            self.step_index = self.step_index.saturating_add(1);
                            events.push(AgentEvent::StepCompleted {
                                run_id: self.run.id,
                                step_id,
                                index: step_index,
                            });
                            events.extend(self.finish_failed(output));
                            return Ok(events);
                        };
                        let evaluation = self
                            .tools
                            .policy_evaluation(&call, &self.approval_policy)
                            .unwrap_or_else(|| {
                                PolicyEvaluation::evaluate(
                                    &self.approval_policy,
                                    kind.action_kind(),
                                    &call.name,
                                )
                            });
                        events.push(AgentEvent::ToolPolicyEvaluated {
                            run_id: self.run.id,
                            call: call.clone(),
                            evaluation: evaluation.clone(),
                        });
                        if matches!(evaluation.decision, loom_core::PolicyDecision::Deny) {
                            let result = ToolResult {
                                tool_call_id: call.id,
                                name: call.name.clone(),
                                success: false,
                                output: evaluation.reason,
                            };
                            events.push(AgentEvent::ToolCallCompleted {
                                run_id: self.run.id,
                                result,
                            });
                            self.step_id = None;
                            self.step_index = self.step_index.saturating_add(1);
                            events.push(AgentEvent::StepCompleted {
                                run_id: self.run.id,
                                step_id,
                                index: step_index,
                            });
                            events.extend(
                                self.finish_failed("tool call denied by the workspace policy"),
                            );
                            return Ok(events);
                        }
                        if matches!(
                            evaluation.decision,
                            loom_core::PolicyDecision::RequireApproval
                        ) {
                            self.pending_approval = Some(PendingApproval { call: call.clone() });
                            self.step_id = None;
                            self.step_index = self.step_index.saturating_add(1);
                            events.push(AgentEvent::StepCompleted {
                                run_id: self.run.id,
                                step_id,
                                index: step_index,
                            });
                            events.extend(self.set_state(AgentRunState::AwaitingApproval));
                            events.push(AgentEvent::ToolApprovalRequired {
                                run_id: self.run.id,
                                call,
                            });
                            return Ok(events);
                        }
                        let (tool_events, result) = self.execute_tool(&call);
                        events.extend(tool_events);
                        if result.success {
                            self.last_failed_call = None;
                        } else {
                            self.last_failed_call = Some(call);
                            events.extend(self.finish_failed(result.output));
                            return Ok(events);
                        }
                    }
                    ModelStreamEvent::Usage { usage } => {
                        self.usage.add_tokens(
                            usage.input_tokens,
                            usage.output_tokens,
                            usage.cached_input_tokens,
                        );
                        self.usage.add_cost_micros(
                            usage
                                .input_tokens
                                .saturating_mul(self.options.input_cost_micros_per_1k)
                                .saturating_add(
                                    usage
                                        .output_tokens
                                        .saturating_mul(self.options.output_cost_micros_per_1k),
                                )
                                / 1_000,
                        );
                        events.push(AgentEvent::RunUsage {
                            run_id: self.run.id,
                            usage,
                        });
                        events.push(AgentEvent::RunUsageUpdated {
                            run_id: self.run.id,
                            usage: self.usage.clone(),
                        });
                        if let Some(status) = self.exceeded_limits() {
                            events.push(AgentEvent::RunLimitReached {
                                run_id: self.run.id,
                                status,
                            });
                            events.extend(self.finish_failed("agent session limit reached"));
                            self.step_id = None;
                            return Ok(events);
                        }
                    }
                    ModelStreamEvent::Completed { reason } => {
                        completed = true;
                        self.step_id = None;
                        self.step_index = self.step_index.saturating_add(1);
                        events.push(AgentEvent::StepCompleted {
                            run_id: self.run.id,
                            step_id,
                            index: step_index,
                        });
                        if !saw_tool_call {
                            if matches!(reason, loom_model::FinishReason::Stop) {
                                events.extend(self.finish_completed());
                            } else if matches!(reason, loom_model::FinishReason::Cancelled) {
                                events.extend(self.finish_cancelled());
                            } else {
                                events.extend(
                                    self.finish_failed(format!("model finished with {reason:?}")),
                                );
                            }
                        }
                    }
                }
                if self.pending_approval.is_some()
                    || matches!(
                        self.run.state,
                        AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
                    )
                {
                    return Ok(events);
                }
            }
            if completed && !saw_tool_call {
                return Ok(events);
            }
        }
    }

    fn execute_tool(&mut self, call: &ToolCall) -> (Vec<AgentEvent>, ToolResult) {
        let mut events = vec![AgentEvent::ToolCallStarted {
            run_id: self.run.id,
            call: call.clone(),
        }];
        let result = self.tools.execute(call);
        if !result.output.is_empty() {
            events.push(AgentEvent::ToolOutputChunk {
                run_id: self.run.id,
                tool_call_id: call.id,
                chunk: result.output.clone(),
            });
        }
        events.push(AgentEvent::ToolCallCompleted {
            run_id: self.run.id,
            result: result.clone(),
        });
        self.messages.push(ModelMessage {
            role: MessageRole::Tool,
            content: result.output.clone(),
            name: Some(result.name.clone()),
            tool_call_id: Some(result.tool_call_id),
        });
        (events, result)
    }

    fn take_pending(&mut self, tool_call_id: loom_core::ToolCallId) -> Result<PendingApproval> {
        let pending = self.pending_approval.take().ok_or_else(|| {
            LoomError::new(
                ErrorCode::InvalidState,
                "agent run is not waiting for tool approval",
                false,
            )
        })?;
        if pending.call.id != tool_call_id {
            self.pending_approval = Some(pending);
            return Err(LoomError::invalid_request(format!(
                "tool approval is waiting for '{}'",
                tool_call_id
            )));
        }
        Ok(pending)
    }

    fn model_request(&self) -> Result<(ModelRequest, ContextInspection)> {
        let initial_count = initial_messages(&self.task).len();
        let conversation = self
            .messages
            .get(initial_count..)
            .unwrap_or_default()
            .to_vec();
        let mut context_options = self.options.context.clone();
        if context_options.context_window.is_none() {
            context_options.context_window =
                self.provider.descriptor().context_window.map(u64::from);
        }
        if context_options.max_input_tokens.is_none() {
            context_options.max_input_tokens = self.options.limits.max_input_tokens;
        }
        let assembly = ContextAssembler::assemble(
            &ContextInput {
                system_instructions: self.task.system_instructions.clone(),
                repository_instructions: self.task.repository_instructions.clone(),
                task: self.task.task.clone(),
                conversation,
                existing_summary: self
                    .context_inspection
                    .as_ref()
                    .and_then(|inspection| inspection.summary.as_ref())
                    .map(|summary| summary.text.clone()),
            },
            &context_options,
        )?;
        let tools = if self.provider.descriptor().capabilities.tool_calling {
            tool_definitions()
        } else {
            Vec::new()
        };
        let request = ModelRequest {
            model: self.task.model.clone(),
            messages: assembly.messages,
            tools,
            options: CompletionOptions::default(),
        };
        let request_tokens = self.provider.count_tokens(&request);
        if assembly
            .inspection
            .budget
            .effective_input_tokens
            .is_some_and(|limit| request_tokens > limit)
        {
            return Err(LoomError::new(
                ErrorCode::ContextLimitExceeded,
                format!(
                    "assembled messages and tools use {request_tokens} tokens, above the context budget"
                ),
                false,
            ));
        }
        let mut inspection = assembly.inspection;
        inspection.included_tokens = request_tokens;
        inspection.total_tokens = inspection.total_tokens.max(request_tokens);
        Ok((request, inspection))
    }

    fn exceeded_limits(&mut self) -> Option<LimitStatus> {
        self.usage.elapsed_ms = Timestamp::now()
            .as_unix_millis()
            .saturating_sub(self.run.started_at.as_unix_millis());
        let status = LimitStatus::new(self.options.limits.clone(), self.usage.clone());
        status.is_exceeded().then_some(status)
    }

    fn append_assistant_text(&mut self, text: &str) {
        if let Some(last) = self.messages.last_mut() {
            if last.role == MessageRole::Assistant {
                last.content.push_str(text);
                return;
            }
        }
        self.messages
            .push(ModelMessage::new(MessageRole::Assistant, text));
    }

    fn assistant_message_id(&mut self) -> u64 {
        if let Some(message_id) = self.active_message_id {
            return message_id;
        }
        let message_id = self.next_message_id;
        self.next_message_id = self.next_message_id.saturating_add(1);
        self.active_message_id = Some(message_id);
        message_id
    }

    fn set_state(&mut self, state: AgentRunState) -> Vec<AgentEvent> {
        if self.run.state == state {
            return Vec::new();
        }
        self.run.state = state;
        self.run.updated_at = Timestamp::now();
        vec![AgentEvent::RunStateChanged {
            run_id: self.run.id,
            state,
        }]
    }

    fn finish_completed(&mut self) -> Vec<AgentEvent> {
        let mut events = self.set_state(AgentRunState::Completed);
        self.run.completed_at = Some(Timestamp::now());
        self.run.summary = Some(format!("Completed task: {}", self.task.task));
        events.push(AgentEvent::RunCompleted {
            snapshot: self.run.clone(),
        });
        events
    }

    fn finish_cancelled(&mut self) -> Vec<AgentEvent> {
        let mut events = self.set_state(AgentRunState::Cancelled);
        self.run.completed_at = Some(Timestamp::now());
        self.run.summary = Some("Model cancelled the run".to_owned());
        events.push(AgentEvent::RunCompleted {
            snapshot: self.run.clone(),
        });
        events
    }

    fn finish_failed(&mut self, reason: impl Into<String>) -> Vec<AgentEvent> {
        let reason = reason.into();
        let mut events = self.set_state(AgentRunState::Failed);
        self.run.completed_at = Some(Timestamp::now());
        self.run.summary = Some(format!("Agent failed: {reason}"));
        events.push(AgentEvent::RunCompleted {
            snapshot: self.run.clone(),
        });
        events
    }
}

fn initial_messages(task: &AgentTask) -> Vec<ModelMessage> {
    let mut messages = Vec::new();
    if let Some(system) = &task.system_instructions {
        messages.push(ModelMessage::new(MessageRole::System, system));
    }
    if let Some(repository) = &task.repository_instructions {
        messages.push(ModelMessage::new(
            MessageRole::System,
            format!("Repository instructions:\n{repository}"),
        ));
    }
    messages.push(ModelMessage::new(MessageRole::User, &task.task));
    messages
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use loom_core::{AgentSessionId, LimitKind, ProjectId, SessionLimits};
    use loom_providers::DeterministicProvider;

    use super::*;

    fn workspace() -> PathBuf {
        let root = std::env::temp_dir().join(format!("loom-agent-{}", ProjectId::new()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn deterministic_run_waits_for_approval_and_completes() {
        let root = workspace();
        let tools = ToolExecutor::new(&root).unwrap();
        let task =
            AgentTask::new("create a demo file", ModelId::new("deterministic/demo")).unwrap();
        let mut runtime = AgentRuntime::new(
            AgentSessionId::new(),
            task,
            Box::new(DeterministicProvider::demo()),
            tools,
        );

        let first_events = runtime.start().unwrap();
        let approval = first_events
            .iter()
            .find_map(|event| match event {
                AgentEvent::ToolApprovalRequired { call, .. } => Some(call.id),
                _ => None,
            })
            .unwrap();
        assert_eq!(runtime.snapshot().state, AgentRunState::AwaitingApproval);

        let second_events = runtime.approve(approval).unwrap();
        let second_approval = second_events
            .iter()
            .find_map(|event| match event {
                AgentEvent::ToolApprovalRequired { call, .. } => Some(call.id),
                _ => None,
            })
            .unwrap();
        let final_events = runtime.approve(second_approval).unwrap();

        assert!(final_events.iter().any(|event| {
            matches!(
                event,
                AgentEvent::RunCompleted { snapshot }
                    if snapshot.state == AgentRunState::Completed
            )
        }));
        assert_eq!(runtime.snapshot().state, AgentRunState::Completed);
        assert_eq!(
            fs::read_to_string(root.join("loom-m1-demo.txt")).unwrap(),
            "Loom M1 deterministic demo\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn interrupt_preserves_a_cancelled_run() {
        let root = workspace();
        let tools = ToolExecutor::new(&root).unwrap();
        let task = AgentTask::new("inspect", ModelId::new("deterministic/demo")).unwrap();
        let mut runtime = AgentRuntime::new(
            AgentSessionId::new(),
            task,
            Box::new(DeterministicProvider::demo()),
            tools,
        );
        runtime.start().unwrap();

        let events = runtime.interrupt().unwrap();

        assert!(events.iter().any(|event| {
            matches!(
                event,
                AgentEvent::RunCompleted { snapshot }
                    if snapshot.state == AgentRunState::Cancelled
            )
        }));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retries_a_failed_patch_after_the_workspace_is_fixed() {
        let root = workspace();
        fs::write(root.join("loom-m1-demo.txt"), "existing\n").unwrap();
        let tools = ToolExecutor::new(&root).unwrap();
        let task =
            AgentTask::new("create a demo file", ModelId::new("deterministic/demo")).unwrap();
        let mut runtime = AgentRuntime::new(
            AgentSessionId::new(),
            task,
            Box::new(DeterministicProvider::demo()),
            tools,
        );

        let first_events = runtime.start().unwrap();
        let patch_approval = first_events
            .iter()
            .find_map(|event| match event {
                AgentEvent::ToolApprovalRequired { call, .. } if call.name == "apply_patch" => {
                    Some(call.id)
                }
                _ => None,
            })
            .unwrap();
        runtime.approve(patch_approval).unwrap();
        assert_eq!(runtime.snapshot().state, AgentRunState::Failed);

        fs::remove_file(root.join("loom-m1-demo.txt")).unwrap();
        let retry_events = runtime.retry().unwrap();
        let command_approval = retry_events
            .iter()
            .find_map(|event| match event {
                AgentEvent::ToolApprovalRequired { call, .. } if call.name == "run_command" => {
                    Some(call.id)
                }
                _ => None,
            })
            .unwrap();
        let final_events = runtime.approve(command_approval).unwrap();

        assert!(final_events.iter().any(|event| {
            matches!(
                event,
                AgentEvent::RunCompleted { snapshot }
                    if snapshot.state == AgentRunState::Completed
            )
        }));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pauses_and_resumes_with_pending_approval() {
        let root = workspace();
        let tools = ToolExecutor::new(&root).unwrap();
        let task = AgentTask::new("pause", ModelId::new("deterministic/demo")).unwrap();
        let mut runtime = AgentRuntime::new(
            AgentSessionId::new(),
            task,
            Box::new(DeterministicProvider::demo()),
            tools,
        );
        runtime.start().unwrap();
        let paused = runtime.pause().unwrap();
        assert_eq!(runtime.snapshot().state, AgentRunState::Paused);
        assert!(paused.iter().any(|event| {
            matches!(
                event,
                AgentEvent::RunStateChanged {
                    state: AgentRunState::Paused,
                    ..
                }
            )
        }));
        runtime.resume().unwrap();
        assert_eq!(runtime.snapshot().state, AgentRunState::AwaitingApproval);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn state_round_trips_and_enforces_tool_limits_explicitly() {
        let root = workspace();
        let tools = ToolExecutor::new(&root).unwrap();
        let task = AgentTask::new("limited", ModelId::new("deterministic/demo")).unwrap();
        let mut runtime = AgentRuntime::new_with_options(
            AgentSessionId::new(),
            task,
            Box::new(DeterministicProvider::demo()),
            tools,
            ApprovalPolicy::default(),
            AgentRuntimeOptions {
                limits: SessionLimits {
                    max_tool_calls: Some(0),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let events = runtime.start().unwrap();
        assert!(events.iter().any(|event| {
            matches!(
                event,
                AgentEvent::RunLimitReached { status, .. }
                    if status.exceeded.contains(&LimitKind::ToolCalls)
            )
        }));
        assert_eq!(runtime.snapshot().state, AgentRunState::Failed);
        let restored = AgentRuntime::from_state(
            runtime.export_state(),
            Box::new(DeterministicProvider::demo()),
            ToolExecutor::new(&root).unwrap(),
        )
        .unwrap();
        assert_eq!(restored.messages(), runtime.messages());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn usage_costs_are_counted_against_the_session_budget() {
        let root = workspace();
        let tools = ToolExecutor::new(&root).unwrap();
        let task = AgentTask::new("costed", ModelId::new("deterministic/demo")).unwrap();
        let mut runtime = AgentRuntime::new_with_options(
            AgentSessionId::new(),
            task,
            Box::new(DeterministicProvider::demo()),
            tools,
            ApprovalPolicy::default(),
            AgentRuntimeOptions {
                limits: SessionLimits {
                    max_cost_micros: Some(10),
                    ..Default::default()
                },
                input_cost_micros_per_1k: 1_000,
                output_cost_micros_per_1k: 1_000,
                ..Default::default()
            },
        );
        runtime.start().unwrap();
        let patch = runtime
            .messages()
            .iter()
            .filter(|message| message.role == MessageRole::Tool)
            .count();
        assert_eq!(patch, 1);
        let approval = runtime
            .export_state()
            .pending_approval
            .expect("patch approval");
        let events = runtime.approve(approval.id).unwrap();
        assert!(events.iter().all(|event| {
            !matches!(
                event,
                AgentEvent::RunLimitReached { status, .. }
                    if status.exceeded.contains(&LimitKind::Cost)
            )
        }));
        let command = runtime
            .export_state()
            .pending_approval
            .expect("command approval");
        let events = runtime.approve(command.id).unwrap();
        assert!(events.iter().any(|event| {
            matches!(
                event,
                AgentEvent::RunLimitReached { status, .. }
                    if status.exceeded.contains(&LimitKind::Cost)
            )
        }));
        assert!(runtime.usage().cost_micros >= 10);
        fs::remove_dir_all(root).unwrap();
    }
}
