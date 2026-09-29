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
use loom_model::{
    CancellationToken, CompletionOptions, MessageRole, ModelId, ModelMessage, ModelProvider,
    ModelRequest, ModelStreamEvent, StreamFlow, ToolCall,
};
pub use loom_protocol::{
    AgentActivityData, AgentActivityKind, AgentActivityRecord, AgentActivityStatus, AgentEvent,
    AgentInteractionRecord, AgentPlan, AgentPlanStep, AgentRunSnapshot, AgentRunState,
    ApprovalDecision, FileActivityOperation, ProjectJoinContinuation,
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
mod state;
mod steps;
mod tools;

use activity::*;

const CONTEXT_PROJECTION_VERSION: u32 = 1;
const MAX_INTERACTION_PROMPT_BYTES: usize = 65_536;

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
    #[serde(default)]
    pub attempts: Vec<loom_protocol::AgentRunAttemptRecord>,
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
    pub context_checkpoint: Option<loom_protocol::ContextSummary>,
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

struct PendingApproval {
    call: ToolCall,
}

/// Out-of-band control of a run that is executing.
///
/// A control request never waits for the run: it raises a flag and cancels the
/// in-flight model stream, and the run loop applies the transition at its next
/// checkpoint.
#[derive(Clone, Debug, Default)]
pub struct RunControl {
    inner: Arc<RunControlState>,
}

#[derive(Debug, Default)]
struct RunControlState {
    interrupt: AtomicBool,
    pause: AtomicBool,
    stream: Mutex<CancellationToken>,
}

impl RunControl {
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks the run to stop and finish as cancelled.
    pub fn request_interrupt(&self) {
        let stream = self.locked_stream();
        self.inner.interrupt.store(true, Ordering::SeqCst);
        stream.cancel();
    }

    /// Asks the run to stop at its next checkpoint and stay resumable.
    pub fn request_pause(&self) {
        let stream = self.locked_stream();
        self.inner.pause.store(true, Ordering::SeqCst);
        stream.cancel();
    }

    pub fn is_interrupt_requested(&self) -> bool {
        self.inner.interrupt.load(Ordering::SeqCst)
    }

    pub fn is_pause_requested(&self) -> bool {
        self.inner.pause.load(Ordering::SeqCst)
    }

    /// True while a requested pause or interrupt has not been applied yet.
    pub fn is_stopping(&self) -> bool {
        self.is_interrupt_requested() || self.is_pause_requested()
    }

    /// Token handed to the provider for the next model call.
    pub fn stream_token(&self) -> CancellationToken {
        self.locked_stream().clone()
    }

    /// Clears a stop request after the owner has applied it directly.
    pub fn clear_request(&self) {
        let mut stream = self.locked_stream();
        self.inner.interrupt.store(false, Ordering::SeqCst);
        self.inner.pause.store(false, Ordering::SeqCst);
        *stream = CancellationToken::new();
    }

    fn locked_stream(&self) -> std::sync::MutexGuard<'_, CancellationToken> {
        self.inner
            .stream
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Receives every agent event as it is produced.
pub type AgentEventObserver = Arc<dyn Fn(&AgentEvent) + Send + Sync>;

/// Events produced by one run operation, and whether the run still has work.
#[derive(Clone, Debug)]
pub struct RunProgress {
    pub events: Vec<AgentEvent>,
    pub continues: bool,
}

impl RunProgress {
    fn running(events: Vec<AgentEvent>) -> Self {
        Self {
            events,
            continues: true,
        }
    }

    fn blocked(events: Vec<AgentEvent>) -> Self {
        Self {
            events,
            continues: false,
        }
    }
}

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
    finished: bool,
    activity_id: ActivityId,
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
            finished: false,
            activity_id,
        }
    }
}

pub struct AgentRuntime {
    session_id: AgentSessionId,
    task: AgentTask,
    run: AgentRunSnapshot,
    attempts: Vec<loom_protocol::AgentRunAttemptRecord>,
    plan: AgentPlan,
    provider: Option<Box<dyn ModelProvider>>,
    tools: ToolExecutor,
    messages: Vec<ModelMessage>,
    message_timeline_ordinals: Vec<u64>,
    next_timeline_ordinal: u64,
    last_project_message_sequence: u64,
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
    context_checkpoint: Option<loom_protocol::ContextSummary>,
    provider_cursor: usize,
    step_id: Option<StepId>,
    step_index: u32,
    activities: Vec<AgentActivityRecord>,
    interactions: Vec<AgentInteractionRecord>,
    /// Signatures of tool calls the user rejected this run. Re-requesting one is
    /// answered without prompting again so a rejection cannot loop.
    denied_tool_calls: BTreeSet<String>,
    control: RunControl,
    observer: Option<AgentEventObserver>,
    flush_offset: usize,
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use loom_core::{AgentSessionId, LimitKind, SessionLimits};
    use loom_providers::DeterministicProvider;
    use loom_tools::ToolExtension;

    use super::*;

    fn workspace() -> PathBuf {
        let root = std::env::temp_dir().join(format!("loom-agent-{}", AgentSessionId::new()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn repairs_unpaired_tool_messages_for_provider_requests() {
        let call = ToolCall {
            id: loom_core::ToolCallId::new(),
            name: "read_file".to_owned(),
            arguments: serde_json::json!({"path": "archive.rs"}),
        };
        let assistant = ModelMessage {
            role: MessageRole::Assistant,
            content: String::new(),
            name: None,
            tool_call_id: None,
            tool_calls: vec![call.clone()],
        };
        let source_output = ModelMessage {
            role: MessageRole::Tool,
            content: "archive implementation".to_owned(),
            name: Some(call.name.clone()),
            tool_call_id: Some(call.id),
            tool_calls: Vec::new(),
        };
        let repaired = repair_tool_transcript(
            vec![
                assistant,
                ModelMessage {
                    role: MessageRole::Tool,
                    content: "orphan".to_owned(),
                    name: None,
                    tool_call_id: Some(loom_core::ToolCallId::new()),
                    tool_calls: Vec::new(),
                },
            ],
            &[source_output],
        );

        assert_eq!(repaired.len(), 2);
        assert_eq!(repaired[1].role, MessageRole::Tool);
        assert_eq!(repaired[1].tool_call_id, Some(call.id));
        assert_eq!(repaired[1].content, "archive implementation");
    }

    /// Provider that blocks in the middle of a completion until it is released
    /// or cancelled, so control requests can be exercised against a busy run.
    struct BlockingProvider {
        descriptor: loom_model::ModelDescriptor,
        released: Arc<AtomicBool>,
        entered: Arc<AtomicBool>,
    }

    impl ModelProvider for BlockingProvider {
        fn descriptor(&self) -> &loom_model::ModelDescriptor {
            &self.descriptor
        }

        fn stream(
            &mut self,
            _request: &ModelRequest,
            cancel: &CancellationToken,
            sink: &mut dyn loom_model::ModelStreamSink,
        ) -> Result<()> {
            sink.emit(ModelStreamEvent::TextDelta {
                text: "working".to_owned(),
            })?;
            self.entered.store(true, Ordering::SeqCst);
            loop {
                cancel.check()?;
                if self.released.load(Ordering::SeqCst) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            sink.emit(ModelStreamEvent::Completed {
                reason: loom_model::FinishReason::Stop,
            })?;
            Ok(())
        }
    }

    struct PlanThenCompleteProvider {
        descriptor: loom_model::ModelDescriptor,
        cursor: usize,
    }

    struct IncompleteProvider {
        descriptor: loom_model::ModelDescriptor,
        calls: Arc<Mutex<u8>>,
    }

    impl ModelProvider for IncompleteProvider {
        fn descriptor(&self) -> &loom_model::ModelDescriptor {
            &self.descriptor
        }

        fn stream(
            &mut self,
            _request: &ModelRequest,
            _cancel: &CancellationToken,
            sink: &mut dyn loom_model::ModelStreamSink,
        ) -> Result<()> {
            *self.calls.lock().unwrap() += 1;
            sink.emit(ModelStreamEvent::TextDelta {
                text: "partial answer".to_owned(),
            })?;
            sink.emit(ModelStreamEvent::Usage {
                usage: loom_model::TokenUsage {
                    output_tokens: 3,
                    ..Default::default()
                },
            })?;
            sink.emit(ModelStreamEvent::Completed {
                reason: loom_model::FinishReason::ErrorWithMessage {
                    message: "github-copilot responses response was incomplete: max_output_tokens"
                        .to_owned(),
                },
            })?;
            Ok(())
        }
    }

    impl ModelProvider for PlanThenCompleteProvider {
        fn descriptor(&self) -> &loom_model::ModelDescriptor {
            &self.descriptor
        }

        fn stream(
            &mut self,
            _request: &ModelRequest,
            cancel: &CancellationToken,
            sink: &mut dyn loom_model::ModelStreamSink,
        ) -> Result<()> {
            let events = if self.cursor == 0 {
                vec![ModelStreamEvent::ToolCallDelta {
                    call: ToolCall {
                        id: loom_core::ToolCallId::new(),
                        name: "propose_plan".to_owned(),
                        arguments: serde_json::json!({
                            "steps": ["Inspect the workspace", "Complete the requested task"]
                        }),
                    },
                }]
            } else {
                vec![
                    ModelStreamEvent::TextDelta {
                        text: "The requested task is complete.".to_owned(),
                    },
                    ModelStreamEvent::Completed {
                        reason: loom_model::FinishReason::Stop,
                    },
                ]
            };
            self.cursor = self.cursor.saturating_add(1);
            for event in events {
                cancel.check()?;
                if sink.emit(event)? == StreamFlow::Stop {
                    break;
                }
            }
            Ok(())
        }

        fn reset(&mut self) {
            self.cursor = 0;
        }
    }

    struct ExtensionToolProvider {
        descriptor: loom_model::ModelDescriptor,
        cursor: usize,
        saw_extension_tool: Arc<AtomicBool>,
    }

    impl ModelProvider for ExtensionToolProvider {
        fn descriptor(&self) -> &loom_model::ModelDescriptor {
            &self.descriptor
        }

        fn stream(
            &mut self,
            request: &ModelRequest,
            cancel: &CancellationToken,
            sink: &mut dyn loom_model::ModelStreamSink,
        ) -> Result<()> {
            let events = if self.cursor == 0 {
                self.saw_extension_tool.store(
                    request
                        .tools
                        .iter()
                        .any(|tool| tool.name == "agent_extension_tool"),
                    Ordering::SeqCst,
                );
                vec![ModelStreamEvent::ToolCallDelta {
                    call: ToolCall {
                        id: loom_core::ToolCallId::new(),
                        name: "agent_extension_tool".to_owned(),
                        arguments: serde_json::json!({}),
                    },
                }]
            } else {
                vec![
                    ModelStreamEvent::TextDelta {
                        text: "The extension ran.".to_owned(),
                    },
                    ModelStreamEvent::Completed {
                        reason: loom_model::FinishReason::Stop,
                    },
                ]
            };
            self.cursor = self.cursor.saturating_add(1);
            for event in events {
                cancel.check()?;
                if sink.emit(event)? == StreamFlow::Stop {
                    break;
                }
            }
            Ok(())
        }

        fn reset(&mut self) {
            self.cursor = 0;
        }
    }

    struct TestToolExtension {
        called: Arc<AtomicBool>,
    }

    impl ToolExtension for TestToolExtension {
        fn definitions(&self) -> Vec<loom_model::ToolDefinition> {
            vec![loom_model::ToolDefinition {
                name: "agent_extension_tool".to_owned(),
                description: "Test-only server extension.".to_owned(),
                input_schema: serde_json::json!({"type": "object", "additionalProperties": false}),
            }]
        }

        fn action_kind(&self, call: &ToolCall) -> Option<loom_core::ActionKind> {
            (call.name == "agent_extension_tool").then_some(loom_core::ActionKind::Read)
        }

        fn execute(&self, call: &ToolCall) -> ToolResult {
            self.called.store(true, Ordering::SeqCst);
            ToolResult {
                tool_call_id: call.id,
                name: call.name.clone(),
                success: true,
                output: "extension completed".to_owned(),
            }
        }
    }

    struct AskThenCompleteProvider {
        descriptor: loom_model::ModelDescriptor,
        cursor: usize,
    }

    impl ModelProvider for AskThenCompleteProvider {
        fn descriptor(&self) -> &loom_model::ModelDescriptor {
            &self.descriptor
        }

        fn stream(
            &mut self,
            _request: &ModelRequest,
            cancel: &CancellationToken,
            sink: &mut dyn loom_model::ModelStreamSink,
        ) -> Result<()> {
            let events = if self.cursor == 0 {
                vec![ModelStreamEvent::ToolCallDelta {
                    call: ToolCall {
                        id: loom_core::ToolCallId::new(),
                        name: "ask_user".to_owned(),
                        arguments: serde_json::json!({
                            "prompt": "Which archive behavior should I preserve?"
                        }),
                    },
                }]
            } else {
                vec![
                    ModelStreamEvent::TextDelta {
                        text: "The archive behavior is fixed.".to_owned(),
                    },
                    ModelStreamEvent::Completed {
                        reason: loom_model::FinishReason::Stop,
                    },
                ]
            };
            self.cursor = self.cursor.saturating_add(1);
            for event in events {
                cancel.check()?;
                if sink.emit(event)? == StreamFlow::Stop {
                    break;
                }
            }
            Ok(())
        }

        fn reset(&mut self) {
            self.cursor = 0;
        }
    }

    /// Emits two executable (read) tool calls in one completion, then finishes.
    struct TwoToolsThenCompleteProvider {
        descriptor: loom_model::ModelDescriptor,
        cursor: usize,
    }

    impl ModelProvider for TwoToolsThenCompleteProvider {
        fn descriptor(&self) -> &loom_model::ModelDescriptor {
            &self.descriptor
        }

        fn stream(
            &mut self,
            _request: &ModelRequest,
            cancel: &CancellationToken,
            sink: &mut dyn loom_model::ModelStreamSink,
        ) -> Result<()> {
            let events = if self.cursor == 0 {
                vec![
                    ModelStreamEvent::TextDelta {
                        text: "Checking two paths.".to_owned(),
                    },
                    ModelStreamEvent::ToolCallDelta {
                        call: ToolCall {
                            id: loom_core::ToolCallId::new(),
                            name: "list_files".to_owned(),
                            arguments: serde_json::json!({"path": "."}),
                        },
                    },
                    ModelStreamEvent::ToolCallDelta {
                        call: ToolCall {
                            id: loom_core::ToolCallId::new(),
                            name: "list_files".to_owned(),
                            arguments: serde_json::json!({"path": "."}),
                        },
                    },
                    ModelStreamEvent::Completed {
                        reason: loom_model::FinishReason::ToolCall,
                    },
                ]
            } else {
                vec![
                    ModelStreamEvent::TextDelta {
                        text: "Both paths checked.".to_owned(),
                    },
                    ModelStreamEvent::Completed {
                        reason: loom_model::FinishReason::Stop,
                    },
                ]
            };
            self.cursor = self.cursor.saturating_add(1);
            for event in events {
                cancel.check()?;
                if sink.emit(event)? == StreamFlow::Stop {
                    break;
                }
            }
            Ok(())
        }

        fn reset(&mut self) {
            self.cursor = 0;
        }
    }

    /// Requests one approval-gated change, then acknowledges the rejection.
    struct RejectThenCompleteProvider {
        descriptor: loom_model::ModelDescriptor,
        cursor: usize,
    }

    impl ModelProvider for RejectThenCompleteProvider {
        fn descriptor(&self) -> &loom_model::ModelDescriptor {
            &self.descriptor
        }

        fn stream(
            &mut self,
            _request: &ModelRequest,
            cancel: &CancellationToken,
            sink: &mut dyn loom_model::ModelStreamSink,
        ) -> Result<()> {
            let events = if self.cursor == 0 {
                vec![
                    ModelStreamEvent::ToolCallDelta {
                        call: ToolCall {
                            id: loom_core::ToolCallId::new(),
                            name: "apply_patch".to_owned(),
                            arguments: serde_json::json!({
                                "path": "rejected.txt",
                                "old_text": "",
                                "new_text": "nope\n"
                            }),
                        },
                    },
                    ModelStreamEvent::Completed {
                        reason: loom_model::FinishReason::ToolCall,
                    },
                ]
            } else {
                vec![
                    ModelStreamEvent::TextDelta {
                        text: "Understood, I will not change that file.".to_owned(),
                    },
                    ModelStreamEvent::Completed {
                        reason: loom_model::FinishReason::Stop,
                    },
                ]
            };
            self.cursor = self.cursor.saturating_add(1);
            for event in events {
                cancel.check()?;
                if sink.emit(event)? == StreamFlow::Stop {
                    break;
                }
            }
            Ok(())
        }

        fn reset(&mut self) {
            self.cursor = 0;
        }
    }

    fn blocking_runtime(entered: Arc<AtomicBool>, released: Arc<AtomicBool>) -> AgentRuntime {
        let root = workspace();
        let tools = ToolExecutor::new(&root).unwrap();
        let task = AgentTask::new("stream slowly", ModelId::new("blocking/demo")).unwrap();
        AgentRuntime::new(
            AgentSessionId::new(),
            task,
            Box::new(BlockingProvider {
                descriptor: loom_model::ModelDescriptor {
                    id: ModelId::new("blocking/demo"),
                    provider: loom_model::ProviderId::new("blocking"),
                    display_name: "Blocking test provider".to_owned(),
                    context_window: Some(8_192),
                    max_input_tokens: None,
                    max_output_tokens: None,
                    capabilities: loom_model::ModelCapabilities {
                        streaming: true,
                        tool_calling: false,
                        vision: false,
                        json_mode: false,
                    },
                },
                released,
                entered,
            }),
            tools,
        )
    }

    #[test]
    fn assistant_deltas_reach_the_observer_before_the_completion_finishes() {
        let entered = Arc::new(AtomicBool::new(false));
        let released = Arc::new(AtomicBool::new(false));
        let mut runtime = blocking_runtime(Arc::clone(&entered), Arc::clone(&released));
        let observed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&observed);
        runtime.set_event_observer(Arc::new(move |event: &AgentEvent| {
            if let AgentEvent::AssistantMessageDelta { text, .. } = event {
                sink.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(text.clone());
            }
        }));
        let control = runtime.control();
        let worker = std::thread::spawn(move || {
            let events = runtime.start().unwrap();
            (runtime, events)
        });
        while !entered.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // The completion has not finished, but the delta is already published.
        assert_eq!(
            observed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            ["working".to_owned()]
        );
        control.request_interrupt();
        let (runtime, events) = worker.join().unwrap();
        assert_eq!(runtime.snapshot().state, AgentRunState::Cancelled);
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::AssistantMessageDelta { text, .. } if text == "working"
        )));
    }

    #[test]
    fn a_pause_request_stops_a_run_that_is_waiting_on_the_model() {
        let entered = Arc::new(AtomicBool::new(false));
        let released = Arc::new(AtomicBool::new(false));
        let mut runtime = blocking_runtime(Arc::clone(&entered), Arc::clone(&released));
        let control = runtime.control();
        let worker = std::thread::spawn(move || {
            runtime.start().unwrap();
            runtime
        });
        while !entered.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        control.request_pause();
        let runtime = worker.join().unwrap();
        assert_eq!(runtime.snapshot().state, AgentRunState::Paused);
        assert!(!control.is_stopping());
    }

    #[test]
    fn a_single_completion_runs_all_of_its_executable_tool_calls() {
        let root = workspace();
        std::fs::write(root.join("a.txt"), "a").unwrap();
        let tools = ToolExecutor::new(&root).unwrap();
        let task = AgentTask::new("list twice", ModelId::new("two-tools/demo")).unwrap();
        let mut runtime = AgentRuntime::new(
            AgentSessionId::new(),
            task,
            Box::new(TwoToolsThenCompleteProvider {
                descriptor: loom_model::ModelDescriptor {
                    id: ModelId::new("two-tools/demo"),
                    provider: loom_model::ProviderId::new("two-tools"),
                    display_name: "Two tool test provider".to_owned(),
                    context_window: Some(8_192),
                    max_input_tokens: None,
                    max_output_tokens: None,
                    capabilities: loom_model::ModelCapabilities {
                        streaming: true,
                        tool_calling: true,
                        vision: false,
                        json_mode: false,
                    },
                },
                cursor: 0,
            }),
            tools,
        );

        let events = runtime.start().unwrap();
        let completed = events
            .iter()
            .filter(|event| matches!(event, AgentEvent::ToolCallCompleted { .. }))
            .count();
        assert_eq!(completed, 2);
        assert_eq!(runtime.snapshot().state, AgentRunState::Completed);
        assert_eq!(
            runtime
                .messages()
                .iter()
                .filter(|message| message.role == MessageRole::Tool)
                .count(),
            2
        );
    }

    #[test]
    fn rejecting_a_tool_call_lets_the_run_continue() {
        let root = workspace();
        let tools = ToolExecutor::new(&root).unwrap();
        let task = AgentTask::new("edit a file", ModelId::new("reject/demo")).unwrap();
        let mut runtime = AgentRuntime::new(
            AgentSessionId::new(),
            task,
            Box::new(RejectThenCompleteProvider {
                descriptor: loom_model::ModelDescriptor {
                    id: ModelId::new("reject/demo"),
                    provider: loom_model::ProviderId::new("reject"),
                    display_name: "Reject test provider".to_owned(),
                    context_window: Some(8_192),
                    max_input_tokens: None,
                    max_output_tokens: None,
                    capabilities: loom_model::ModelCapabilities {
                        streaming: true,
                        tool_calling: true,
                        vision: false,
                        json_mode: false,
                    },
                },
                cursor: 0,
            }),
            tools,
        );

        let first = runtime.start().unwrap();
        let (call_id, attempt_id, revision) = first
            .iter()
            .find_map(|event| match event {
                AgentEvent::ToolApprovalRequired {
                    call,
                    attempt_id,
                    control_revision,
                    ..
                } => Some((call.id, *attempt_id, *control_revision)),
                _ => None,
            })
            .unwrap();
        assert_eq!(runtime.snapshot().state, AgentRunState::AwaitingApproval);

        let progress = runtime
            .reject_entry(
                call_id,
                Some("not this time".to_owned()),
                attempt_id,
                revision,
            )
            .unwrap();
        assert!(progress.continues);
        assert_eq!(runtime.snapshot().state, AgentRunState::Executing);
        assert!(!root.join("rejected.txt").exists());
        assert!(progress.events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCallCompleted { result, .. } if !result.success
        )));

        let second = runtime.advance().unwrap();
        assert_eq!(runtime.snapshot().state, AgentRunState::Completed);
        assert!(second.iter().any(|event| matches!(
            event,
            AgentEvent::AssistantMessageDelta { text, .. }
                if text.contains("will not change that file")
        )));
        assert!(
            runtime
                .messages()
                .iter()
                .any(|message| message.role == MessageRole::Tool
                    && message.content == "not this time")
        );
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
                AgentEvent::ToolApprovalRequired {
                    call,
                    attempt_id,
                    control_revision,
                    ..
                } => Some((call.id, *attempt_id, *control_revision)),
                _ => None,
            })
            .unwrap();
        assert_eq!(runtime.snapshot().state, AgentRunState::AwaitingApproval);

        let approved = runtime
            .approve_entry(approval.0, approval.1, approval.2)
            .unwrap();
        assert!(approved.continues);
        assert!(runtime.export_state().pending_tool_execution.is_some());
        assert!(!root.join("loom-m1-demo.txt").exists());

        let second_events = runtime.advance().unwrap();
        assert!(root.join("loom-m1-demo.txt").exists());
        let second_approval = second_events
            .iter()
            .find_map(|event| match event {
                AgentEvent::ToolApprovalRequired {
                    call,
                    attempt_id,
                    control_revision,
                    ..
                } => Some((call.id, *attempt_id, *control_revision)),
                _ => None,
            })
            .unwrap();
        let final_events = runtime.approve(second_approval.0).unwrap();

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
        let state = runtime.export_state();
        assert_eq!(state.message_timeline_ordinals.len(), state.messages.len());
        let mut timeline_ordinals = state.message_timeline_ordinals.clone();
        timeline_ordinals.extend(
            state
                .activities
                .iter()
                .map(|activity| activity.timeline_ordinal),
        );
        timeline_ordinals.sort_unstable();
        assert!(timeline_ordinals.windows(2).all(|pair| pair[0] < pair[1]));
        let activities = state.activities;
        assert!(activities.iter().any(|activity| {
            activity.kind == AgentActivityKind::ModelTurn
                && activity.status == AgentActivityStatus::Completed
                && activity.completed_at.is_some()
        }));
        assert!(activities.iter().any(|activity| {
            activity.kind == AgentActivityKind::File
                && activity.parent_id.is_some()
                && matches!(
                    &activity.data,
                    AgentActivityData::File {
                        operation: FileActivityOperation::Write,
                        result: Some(result),
                        ..
                    } if result.success
                )
        }));
        assert!(
            activities
                .iter()
                .filter(|activity| activity.kind == AgentActivityKind::ToolCall)
                .all(|activity| activity.parent_id.is_some())
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_messages_append_once_and_advance_the_inbox_cursor() {
        let root = workspace();
        let session_id = AgentSessionId::new();
        let task =
            AgentTask::new("coordinate the project", ModelId::new("deterministic/demo")).unwrap();
        let mut runtime = AgentRuntime::new(
            session_id,
            task,
            Box::new(DeterministicProvider::demo()),
            ToolExecutor::new(&root).unwrap(),
        );
        let message = AgentMessageRecord {
            message_id: loom_core::AgentMessageId::new(),
            project_id: loom_core::ProjectId::from_uuid(*session_id.as_uuid()),
            task_id: None,
            sender_session_id: AgentSessionId::new(),
            target_session_id: session_id,
            kind: loom_core::AgentMessageKind::Direction,
            project_sequence: 3,
            accepted_at: Timestamp::now(),
            body: "Please prioritize the compatibility review".to_owned(),
        };

        runtime.pending_tool_execution = Some(ToolCall {
            id: loom_core::ToolCallId::new(),
            name: "write_file".to_owned(),
            arguments: serde_json::json!({"path": "result.txt"}),
        });
        assert!(!runtime.append_project_message(&message).unwrap());
        assert_eq!(runtime.last_project_message_sequence(), 0);
        runtime.pending_tool_execution = None;
        assert!(runtime.append_project_message(&message).unwrap());
        assert!(!runtime.append_project_message(&message).unwrap());
        assert_eq!(runtime.last_project_message_sequence(), 3);
        let delivered = runtime.messages().pop().unwrap();
        assert_eq!(delivered.role, MessageRole::User);
        assert_eq!(delivered.name.as_deref(), Some("loom_project_message"));
        assert!(delivered.content.contains("compatibility review"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn interrupted_approved_tool_is_not_replayed_during_recovery() {
        let root = workspace();
        let mut runtime = AgentRuntime::new(
            AgentSessionId::new(),
            AgentTask::new("create a demo file", ModelId::new("deterministic/demo")).unwrap(),
            Box::new(DeterministicProvider::demo()),
            ToolExecutor::new(&root).unwrap(),
        );
        let events = runtime.start().unwrap();
        let (call_id, attempt_id, control_revision) = events
            .iter()
            .find_map(|event| match event {
                AgentEvent::ToolApprovalRequired {
                    call,
                    attempt_id,
                    control_revision,
                    ..
                } => Some((call.id, *attempt_id, *control_revision)),
                _ => None,
            })
            .unwrap();
        runtime
            .approve_entry(call_id, attempt_id, control_revision)
            .unwrap();
        let interrupted_call = runtime.export_state().pending_tool_execution.unwrap();
        assert_eq!(interrupted_call.id, call_id);
        assert!(!root.join("loom-m1-demo.txt").exists());

        let mut recovered = AgentRuntime::from_state(
            runtime.export_state(),
            Box::new(DeterministicProvider::demo()),
            ToolExecutor::new(&root).unwrap(),
        )
        .unwrap();
        let recovery_events = recovered.recover_after_restart().unwrap();

        assert!(recovery_events.iter().any(|event| {
            matches!(event, AgentEvent::RecoveryRequired { reason, .. } if reason.contains("not replayed"))
        }));
        let recovered_state = recovered.export_state();
        assert_eq!(recovered_state.run.state, AgentRunState::Failed);
        assert_eq!(recovered_state.pending_tool_execution, None);
        assert_eq!(recovered_state.last_failed_call.unwrap().id, call_id);
        assert!(recovered_state.interactions.iter().any(|interaction| {
            interaction.attempt_id == attempt_id
                && interaction.status == loom_protocol::AgentInteractionStatus::Approved
                && interaction.decision == Some(ApprovalDecision::Approved)
        }));
        assert!(!root.join("loom-m1-demo.txt").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn plan_proposal_continues_to_the_next_model_step() {
        let root = workspace();
        let tools = ToolExecutor::new(&root).unwrap();
        let task = AgentTask::new("finish the task", ModelId::new("deterministic/plan")).unwrap();
        let mut runtime = AgentRuntime::new(
            AgentSessionId::new(),
            task,
            Box::new(PlanThenCompleteProvider {
                descriptor: loom_providers::deterministic_descriptor(),
                cursor: 0,
            }),
            tools,
        );

        let events = runtime.start().unwrap();

        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::PlanProposed { plan, .. } if plan.steps.len() == 2
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::RunCompleted { snapshot }
                if snapshot.state == AgentRunState::Completed
        )));
        assert!(
            runtime
                .export_state()
                .messages
                .iter()
                .any(|message| message.role == MessageRole::Tool
                    && message.content == "plan proposed")
        );
        assert_eq!(runtime.snapshot().state, AgentRunState::Completed);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn agent_runtime_exposes_and_executes_server_tool_extensions() {
        let root = workspace();
        let tool_called = Arc::new(AtomicBool::new(false));
        let schema_exposed = Arc::new(AtomicBool::new(false));
        let mut descriptor = loom_providers::deterministic_descriptor();
        descriptor.id = ModelId::new("deterministic/extension");
        let provider = ExtensionToolProvider {
            descriptor,
            cursor: 0,
            saw_extension_tool: Arc::clone(&schema_exposed),
        };
        let tools = ToolExecutor::new(&root)
            .unwrap()
            .with_extension(Arc::new(TestToolExtension {
                called: Arc::clone(&tool_called),
            }));
        let task = AgentTask::new(
            "run the server extension",
            ModelId::new("deterministic/extension"),
        )
        .unwrap();
        let mut runtime = AgentRuntime::new(AgentSessionId::new(), task, Box::new(provider), tools);

        let events = runtime.start().unwrap();

        assert!(schema_exposed.load(Ordering::SeqCst));
        assert!(tool_called.load(Ordering::SeqCst));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCallCompleted { result, .. }
                if result.name == "agent_extension_tool" && result.success
        )));
        assert_eq!(runtime.snapshot().state, AgentRunState::Completed);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ask_user_records_tool_output_before_follow_up_message() {
        let root = workspace();
        let tools = ToolExecutor::new(&root).unwrap();
        let task =
            AgentTask::new("fix archive behavior", ModelId::new("deterministic/ask")).unwrap();
        let mut runtime = AgentRuntime::new(
            AgentSessionId::new(),
            task,
            Box::new(AskThenCompleteProvider {
                descriptor: loom_providers::deterministic_descriptor(),
                cursor: 0,
            }),
            tools,
        );

        runtime.start().unwrap();
        assert_eq!(runtime.snapshot().state, AgentRunState::NeedsInput);
        let state = runtime.export_state();
        let call_id = state
            .messages
            .iter()
            .find_map(|message| {
                message
                    .tool_calls
                    .iter()
                    .find(|call| call.name == "ask_user")
                    .map(|call| call.id)
            })
            .unwrap();
        assert!(state.messages.iter().any(|message| {
            message.role == MessageRole::Tool
                && message.tool_call_id == Some(call_id)
                && message.content.starts_with("Waiting for user input:")
        }));

        let events = runtime
            .send_message("Preserve the current archive semantics.")
            .unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::RunCompleted { snapshot }
                if snapshot.state == AgentRunState::Completed
        )));
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
        let recovery_events = runtime.approve(patch_approval).unwrap();
        assert_eq!(runtime.snapshot().state, AgentRunState::AwaitingApproval);

        fs::remove_file(root.join("loom-m1-demo.txt")).unwrap();
        let command_approval = recovery_events
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
    fn follow_up_message_resumes_a_run_waiting_for_input() {
        let root = workspace();
        let tools = ToolExecutor::new(&root).unwrap();
        let task = AgentTask::new("follow up", ModelId::new("deterministic/demo")).unwrap();
        let mut runtime = AgentRuntime::new(
            AgentSessionId::new(),
            task,
            Box::new(DeterministicProvider::demo()),
            tools,
        );

        let requested = runtime
            .request_input("Which validation should I run?")
            .unwrap();
        let interaction_id = requested
            .iter()
            .find_map(|event| match event {
                AgentEvent::NeedsInput { interaction_id, .. } => Some(*interaction_id),
                _ => None,
            })
            .unwrap();
        assert_eq!(runtime.snapshot().state, AgentRunState::NeedsInput);
        assert!(requested.iter().any(|event| {
            matches!(event, AgentEvent::NeedsInput { prompt, .. } if prompt.contains("validation"))
        }));
        assert!(
            runtime
                .export_state()
                .interactions
                .iter()
                .any(|interaction| {
                    interaction.id == interaction_id
                        && interaction.status == loom_protocol::AgentInteractionStatus::Pending
                })
        );

        let continued = runtime
            .send_message("Run the standard validation.")
            .unwrap();
        assert!(continued.iter().any(|event| {
            matches!(
                event,
                AgentEvent::UserMessage {
                    interaction_id: Some(event_interaction_id),
                    text,
                    ..
                } if *event_interaction_id == interaction_id && text.contains("standard")
            )
        }));
        assert_eq!(runtime.snapshot().state, AgentRunState::AwaitingApproval);
        assert!(
            runtime
                .export_state()
                .interactions
                .iter()
                .any(|interaction| {
                    interaction.id == interaction_id
                        && interaction.status == loom_protocol::AgentInteractionStatus::Answered
                        && interaction.resolved_at.is_some()
                })
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn input_commands_require_the_current_attempt_and_control_revision() {
        let root = workspace();
        let mut runtime = context_runtime(&root);
        let attempt_id = runtime.snapshot().attempt_id;
        let requested = runtime
            .request_input("Which validation should I run?")
            .unwrap();
        let control_revision = requested
            .iter()
            .find_map(|event| match event {
                AgentEvent::NeedsInput {
                    attempt_id,
                    control_revision,
                    ..
                } => Some((*attempt_id, *control_revision)),
                _ => None,
            })
            .unwrap();
        assert_eq!(control_revision.0, attempt_id);

        let wrong_attempt = runtime.message_entry_at_revision(
            "stale attempt",
            loom_core::RunAttemptId::new(),
            control_revision.1,
        );
        assert_eq!(wrong_attempt.unwrap_err().code, ErrorCode::Conflict);

        let stale_revision = runtime.message_entry_at_revision(
            "stale revision",
            attempt_id,
            control_revision.1.saturating_sub(1),
        );
        assert_eq!(stale_revision.unwrap_err().code, ErrorCode::Conflict);
        assert_eq!(
            runtime.pending_input().as_deref(),
            Some("Which validation should I run?")
        );

        let accepted = runtime
            .message_entry_at_revision(
                "Run the standard validation.",
                attempt_id,
                control_revision.1,
            )
            .unwrap();
        assert!(accepted.events.iter().any(|event| {
            matches!(
                event,
                AgentEvent::UserMessage {
                    attempt_id: event_attempt,
                    control_revision: event_revision,
                    text,
                    ..
                } if *event_attempt == attempt_id
                    && *event_revision == control_revision.1 + 1
                    && text.contains("standard")
            )
        }));
        assert_eq!(runtime.snapshot().control_revision, control_revision.1 + 1);
        assert_eq!(runtime.pending_input(), None);

        let restored = AgentRuntime::from_state(
            runtime.export_state(),
            Box::new(DeterministicProvider::demo()),
            ToolExecutor::new(&root).unwrap(),
        )
        .unwrap();
        assert_eq!(restored.snapshot().attempt_id, attempt_id);
        assert_eq!(restored.snapshot().control_revision, control_revision.1 + 1);
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
    fn recovery_discards_waiting_state_from_finished_runs() {
        let root = workspace();
        let task = AgentTask::new(
            "recover stale waiting state",
            ModelId::new("deterministic/demo"),
        )
        .unwrap();
        let runtime = AgentRuntime::new(
            AgentSessionId::new(),
            task,
            Box::new(DeterministicProvider::demo()),
            ToolExecutor::new(&root).unwrap(),
        );
        let mut state = runtime.export_state();
        state.run.state = AgentRunState::Completed;
        state.attempts[0].state = AgentRunState::Completed;
        state.attempts[0].completed_at = Some(Timestamp::now());
        state.pending_approval = Some(ToolCall {
            id: loom_core::ToolCallId::new(),
            name: "read_file".to_owned(),
            arguments: serde_json::json!({"path": "README.md"}),
        });
        state.pending_input = Some("stale input".to_owned());

        let restored = AgentRuntime::from_state(
            state,
            Box::new(DeterministicProvider::demo()),
            ToolExecutor::new(&root).unwrap(),
        )
        .unwrap();

        assert!(restored.pending_approval().is_none());
        assert!(restored.export_state().pending_input.is_none());
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
    fn context_runtime(root: &std::path::Path) -> AgentRuntime {
        AgentRuntime::new(
            AgentSessionId::new(),
            AgentTask::new("Complete the task", ModelId::new("deterministic/demo")).unwrap(),
            Box::new(DeterministicProvider::demo()),
            ToolExecutor::new(root).unwrap(),
        )
    }

    #[test]
    fn tool_schema_tokens_are_charged_once_and_output_reserve_is_sent() {
        let root = workspace();
        let mut runtime = context_runtime(&root);
        let (baseline, _) = runtime.model_request().unwrap();
        let count = runtime.provider().unwrap().count_tokens(&baseline);
        runtime.options.context.max_input_tokens = Some(count);
        runtime.options.context.reserved_output_tokens = Some(512);
        let (request, inspection) = runtime.model_request().unwrap();
        assert_eq!(inspection.included_tokens, count);
        assert_eq!(inspection.budget.effective_input_tokens, Some(count));
        assert_eq!(request.options.max_output_tokens, Some(512));
        assert!(inspection.within_budget());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn model_request_uses_provider_and_remaining_session_token_limits() {
        let root = workspace();
        let descriptor = loom_model::ModelDescriptor {
            id: ModelId::new("limits/demo"),
            provider: loom_model::ProviderId::new("limits"),
            display_name: "Limits test model".to_owned(),
            context_window: Some(8_192),
            max_input_tokens: Some(2_000),
            max_output_tokens: Some(2_048),
            capabilities: loom_model::ModelCapabilities {
                streaming: true,
                tool_calling: false,
                vision: false,
                json_mode: false,
            },
        };
        let mut runtime = AgentRuntime::new(
            AgentSessionId::new(),
            AgentTask::new("Respect provider limits", descriptor.id.clone()).unwrap(),
            Box::new(PlanThenCompleteProvider {
                descriptor,
                cursor: 0,
            }),
            ToolExecutor::new(&root).unwrap(),
        );
        runtime.options.limits.max_output_tokens = Some(512);
        runtime.usage.output_tokens = 100;

        let (request, inspection) = runtime.model_request().unwrap();
        assert_eq!(inspection.budget.context_window, Some(8_192));
        assert_eq!(inspection.budget.requested_input_tokens, Some(2_000));
        assert_eq!(inspection.budget.reserved_output_tokens, 412);
        assert_eq!(request.options.max_output_tokens, Some(412));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn incomplete_model_response_keeps_partial_text_and_fails_without_retrying() {
        let root = workspace();
        let calls = Arc::new(Mutex::new(0));
        let descriptor = loom_model::ModelDescriptor {
            id: ModelId::new("incomplete/demo"),
            provider: loom_model::ProviderId::new("incomplete"),
            display_name: "Incomplete test model".to_owned(),
            context_window: Some(8_192),
            max_input_tokens: None,
            max_output_tokens: Some(4_096),
            capabilities: loom_model::ModelCapabilities {
                streaming: true,
                tool_calling: false,
                vision: false,
                json_mode: false,
            },
        };
        let mut runtime = AgentRuntime::new(
            AgentSessionId::new(),
            AgentTask::new("Keep partial output", descriptor.id.clone()).unwrap(),
            Box::new(IncompleteProvider {
                descriptor,
                calls: Arc::clone(&calls),
            }),
            ToolExecutor::new(&root).unwrap(),
        );

        runtime.advance_step(&mut Vec::new()).unwrap();
        assert_eq!(runtime.run.state, AgentRunState::Failed);
        assert_eq!(runtime.messages.last().unwrap().content, "partial answer");
        assert_eq!(runtime.usage.output_tokens, 3);
        assert_eq!(*calls.lock().unwrap(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn context_checkpoint_survives_recovery_and_advances_only_for_new_history() {
        let root = workspace();
        let mut runtime = context_runtime(&root);
        runtime.options.context.max_input_tokens = Some(4_000);
        runtime.messages.push(ModelMessage::new(
            MessageRole::User,
            "Keep all public signatures unchanged.",
        ));
        let missing_output_call = ToolCall {
            id: loom_core::ToolCallId::new(),
            name: "read_file".to_owned(),
            arguments: serde_json::json!({"path": "archive.rs"}),
        };
        runtime.messages.push(ModelMessage {
            role: MessageRole::Assistant,
            content: String::new(),
            name: None,
            tool_call_id: None,
            tool_calls: vec![missing_output_call],
        });
        for i in 0..30 {
            runtime.messages.push(ModelMessage::new(
                MessageRole::Assistant,
                format!("Step {i}: {}", "work details ".repeat(100)),
            ));
        }
        runtime
            .messages
            .push(ModelMessage::new(MessageRole::Assistant, "latest result"));
        let (request, inspection) = runtime.model_request().unwrap();
        assert!(inspection.compacted);
        let boundary = inspection.summary.as_ref().unwrap().source_message_count;
        assert!(boundary >= 3);
        let initial_count = initial_messages(&runtime.task).len();
        let source = &runtime.messages[initial_count..];
        let repaired = repair_tool_transcript(source.to_vec(), source);
        assert!(repaired[..boundary].iter().any(|message| {
            message.role == MessageRole::Tool
                && message.content
                    == "No tool output was recorded; continue from the current state."
        }));
        assert_eq!(
            inspection.summary.as_ref().unwrap().projection_version,
            CONTEXT_PROJECTION_VERSION
        );
        assert!(
            !inspection
                .summary
                .as_ref()
                .unwrap()
                .source_digest
                .is_empty()
        );
        let digest = inspection.summary.as_ref().unwrap().source_digest.clone();
        runtime.context_checkpoint = inspection.summary.clone();
        runtime.context_inspection = Some(inspection);
        let state = runtime.export_state();
        let mut legacy = serde_json::to_value(&state).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("message_timeline_ordinals");
        let restored_legacy = AgentRuntime::from_state(
            serde_json::from_value(legacy).unwrap(),
            Box::new(DeterministicProvider::demo()),
            ToolExecutor::new(&root).unwrap(),
        )
        .unwrap()
        .export_state();
        assert_eq!(
            restored_legacy.message_timeline_ordinals.len(),
            restored_legacy.messages.len()
        );
        let mut restored_ordinals = restored_legacy.message_timeline_ordinals.clone();
        restored_ordinals.extend(
            restored_legacy
                .activities
                .iter()
                .map(|activity| activity.timeline_ordinal),
        );
        restored_ordinals.sort_unstable();
        assert!(restored_ordinals.windows(2).all(|pair| pair[0] < pair[1]));
        let serialized = serde_json::to_string(&state).unwrap();
        let mut restored = AgentRuntime::from_state(
            serde_json::from_str(&serialized).unwrap(),
            Box::new(DeterministicProvider::demo()),
            ToolExecutor::new(&root).unwrap(),
        )
        .unwrap();
        let (next_request, next) = restored.model_request().unwrap();
        assert!(!next.compacted);
        assert_eq!(
            next.summary.as_ref().unwrap().source_message_count,
            boundary
        );
        assert_eq!(next.summary.as_ref().unwrap().source_digest, digest);
        assert_eq!(next_request.messages, request.messages);
        assert!(
            next_request
                .messages
                .iter()
                .any(|message| message.content == "Keep all public signatures unchanged.")
        );
        for _ in 0..20 {
            restored.messages.push(ModelMessage::new(
                MessageRole::Assistant,
                "more progress ".repeat(100),
            ));
        }
        restored.messages.push(ModelMessage::new(
            MessageRole::User,
            "Now add a regression test.",
        ));
        let (request, next) = restored.model_request().unwrap();
        assert!(next.compacted);
        assert!(next.summary.unwrap().source_message_count > boundary);
        assert_eq!(
            request.messages.last().unwrap().content,
            "Now add a regression test."
        );
        assert_eq!(state.messages.len(), runtime.messages.len());
        let mut legacy = serde_json::to_value(state).unwrap();
        legacy.as_object_mut().unwrap().remove("context_checkpoint");
        assert!(
            serde_json::from_value::<AgentRuntimeState>(legacy)
                .unwrap()
                .context_checkpoint
                .is_none()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_context_checkpoint_is_rebuilt_from_repaired_history() {
        let root = workspace();
        let mut runtime = context_runtime(&root);
        runtime.options.context.max_input_tokens = Some(4_000);
        runtime.messages.push(ModelMessage::new(
            MessageRole::User,
            "Preserve the existing public API.",
        ));
        for i in 0..30 {
            runtime.messages.push(ModelMessage::new(
                MessageRole::Assistant,
                format!("Step {i}: {}", "work details ".repeat(100)),
            ));
        }
        runtime
            .messages
            .push(ModelMessage::new(MessageRole::Assistant, "latest result"));
        let (_, inspection) = runtime.model_request().unwrap();
        let checkpoint = inspection.summary.unwrap();
        assert!(checkpoint.source_message_count > 0);

        let first_conversation_index = initial_messages(&runtime.task).len();
        runtime.messages[first_conversation_index].content =
            "Revised canonical request. ".to_owned() + &"updated details ".repeat(100);
        runtime.context_checkpoint = Some(checkpoint.clone());

        let (_, rebuilt) = runtime.model_request().unwrap();
        assert!(rebuilt.compacted);
        let rebuilt_summary = rebuilt.summary.unwrap();
        assert_ne!(rebuilt_summary.source_digest, checkpoint.source_digest);
        assert_eq!(
            rebuilt_summary.projection_version,
            CONTEXT_PROJECTION_VERSION
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_context_window_cannot_raise_the_model_limit() {
        let root = workspace();
        let mut runtime = context_runtime(&root);
        runtime.options.context.context_window = Some(1_000_000);
        let (_, inspection) = runtime.model_request().unwrap();
        assert_eq!(
            inspection.budget.context_window,
            runtime
                .provider()
                .unwrap()
                .descriptor()
                .context_window
                .map(u64::from)
        );
        runtime.options.context.context_window = Some(4_000);
        let (_, inspection) = runtime.model_request().unwrap();
        assert_eq!(inspection.budget.context_window, Some(4_000));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn unknown_model_uses_a_visible_fallback_and_changed_limits_are_reapplied() {
        let root = workspace();
        let mut runtime = context_runtime(&root);
        let mut descriptor = runtime.provider().unwrap().descriptor().clone();
        descriptor.context_window = None;
        runtime.provider = Some(Box::new(
            loom_providers::OpenAiCompatibleProvider::with_descriptor(
                "http://unused",
                "unused",
                descriptor.clone(),
            ),
        ));
        let (_, inspection) = runtime.model_request().unwrap();
        assert_eq!(inspection.budget.context_window, Some(8_192));
        assert!(
            inspection
                .items
                .iter()
                .any(|item| item.label.contains("fallback"))
        );
        descriptor.context_window = Some(4_096);
        runtime.provider = Some(Box::new(
            loom_providers::OpenAiCompatibleProvider::with_descriptor(
                "http://unused",
                "unused",
                descriptor,
            ),
        ));
        let (_, inspection) = runtime.model_request().unwrap();
        assert_eq!(inspection.budget.context_window, Some(4_096));
        assert!(
            !inspection
                .items
                .iter()
                .any(|item| item.label.contains("fallback"))
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// Acceptance fixture: a representative task must run to completion,
    /// execute its tools in model order, and produce the expected workspace
    /// change without recovery.
    #[test]
    fn acceptance_demo_task_completes_with_ordered_tool_calls() {
        let root = workspace();
        let tools = ToolExecutor::new(&root).unwrap();
        let task = AgentTask::new(
            "inspect, change, and validate the workspace",
            ModelId::new("deterministic/demo"),
        )
        .unwrap();
        let mut runtime = AgentRuntime::new(
            AgentSessionId::new(),
            task,
            Box::new(DeterministicProvider::demo()),
            tools,
        );

        let mut events = runtime.start().unwrap();
        let mut approved = Vec::new();
        loop {
            if let Some(AgentEvent::RunCompleted { snapshot }) = events
                .iter()
                .find(|event| matches!(event, AgentEvent::RunCompleted { .. }))
            {
                assert_eq!(snapshot.state, AgentRunState::Completed);
                break;
            }
            if let Some(call) = events.iter().find_map(|event| match event {
                AgentEvent::ToolApprovalRequired { call, .. } => Some(call.clone()),
                _ => None,
            }) {
                approved.push(call.name.clone());
                events = runtime.approve(call.id).unwrap();
            } else {
                events = runtime.advance().unwrap();
            }
        }

        assert_eq!(runtime.snapshot().state, AgentRunState::Completed);
        assert_eq!(
            approved,
            vec!["apply_patch".to_owned(), "run_command".to_owned()]
        );
        assert_eq!(
            fs::read_to_string(root.join("loom-m1-demo.txt")).unwrap(),
            "Loom M1 deterministic demo\n"
        );
        assert!(runtime.usage().tool_calls >= 3);
        fs::remove_dir_all(root).unwrap();
    }
}
