use loom_core::{AgentSessionId, ErrorCode, LoomError, Result, RunId, Timestamp};
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
    RunStateChanged {
        run_id: RunId,
        state: AgentRunState,
    },
    RunCompleted {
        snapshot: AgentRunSnapshot,
    },
}

pub struct AgentTask {
    pub task: String,
    pub model: ModelId,
    pub system_instructions: Option<String>,
    pub repository_instructions: Option<String>,
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
}

impl AgentRuntime {
    pub fn new(
        session_id: AgentSessionId,
        task: AgentTask,
        provider: Box<dyn ModelProvider>,
        tools: ToolExecutor,
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
        }
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

    fn advance(&mut self) -> Result<Vec<AgentEvent>> {
        let mut events = Vec::new();
        loop {
            if self.pending_approval.is_some()
                || matches!(
                    self.run.state,
                    AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
                )
            {
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
            let request = self.model_request();
            let stream = match self.provider.stream(&request) {
                Ok(stream) => stream,
                Err(error) => {
                    events.extend(self.finish_failed(error.message));
                    return Ok(events);
                }
            };
            if stream.is_empty() {
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
                        saw_tool_call = true;
                        events.push(AgentEvent::ToolCallRequested {
                            run_id: self.run.id,
                            call: call.clone(),
                        });
                        let Some(kind) = ToolKind::from_name(&call.name) else {
                            let result = ToolResult {
                                tool_call_id: call.id,
                                name: call.name.clone(),
                                success: false,
                                output: format!("unknown tool '{}'", call.name),
                            };
                            events.push(AgentEvent::ToolCallCompleted {
                                run_id: self.run.id,
                                result,
                            });
                            continue;
                        };
                        if kind.requires_approval() {
                            self.pending_approval = Some(PendingApproval { call: call.clone() });
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
                        events.push(AgentEvent::RunUsage {
                            run_id: self.run.id,
                            usage,
                        });
                    }
                    ModelStreamEvent::Completed { reason } => {
                        completed = true;
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

    fn model_request(&self) -> ModelRequest {
        ModelRequest {
            model: self.task.model.clone(),
            messages: self.messages.clone(),
            tools: tool_definitions(),
            options: CompletionOptions::default(),
        }
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

    use loom_core::{AgentSessionId, ProjectId};
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
}
