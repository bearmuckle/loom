use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use loom_context::{ContextAssembler, ContextAssemblyOptions, ContextInput, ContextInspection};
use loom_core::{
    ActivityId, AgentSessionId, ApprovalPolicy, ErrorCode, EvidenceLink, LimitKind, LimitStatus,
    LoomError, PolicyEvaluation, Result, RunId, SessionLimits, StepId, Timestamp, UsageSnapshot,
};
use loom_model::{
    CancellationToken, CompletionOptions, MessageRole, ModelId, ModelMessage, ModelProvider,
    ModelRequest, ModelStreamEvent, StreamFlow, ToolCall,
};
pub use loom_protocol::{
    AgentActivityData, AgentActivityKind, AgentActivityRecord, AgentActivityStatus, AgentEvent,
    AgentPlan, AgentPlanStep, AgentRunSnapshot, AgentRunState, ApprovalDecision,
    FileActivityOperation,
};
use loom_tools::{ToolExecutor, ToolKind, ToolResult, tool_definitions};
use serde::{Deserialize, Serialize};

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
    #[serde(default)]
    pub pending_input: Option<String>,
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
    #[serde(default)]
    pub activities: Vec<AgentActivityRecord>,
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
            finished: false,
            activity_id,
        }
    }
}

pub struct AgentRuntime {
    session_id: AgentSessionId,
    task: AgentTask,
    run: AgentRunSnapshot,
    plan: AgentPlan,
    provider: Option<Box<dyn ModelProvider>>,
    tools: ToolExecutor,
    messages: Vec<ModelMessage>,
    pending_approval: Option<PendingApproval>,
    pending_input: Option<String>,
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
    activities: Vec<AgentActivityRecord>,
    control: RunControl,
    observer: Option<AgentEventObserver>,
    flush_offset: usize,
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
            evidence: Vec::new(),
        };
        let plan = AgentPlan { steps: Vec::new() };
        let messages = initial_messages(&task);
        Self {
            session_id,
            task,
            run,
            plan,
            provider: Some(provider),
            tools,
            messages,
            pending_approval: None,
            pending_input: None,
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
            activities: Vec::new(),
            control: RunControl::new(),
            observer: None,
            flush_offset: 0,
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

    /// Handle used to pause or interrupt this run while it is executing.
    pub fn control(&self) -> RunControl {
        self.control.clone()
    }

    /// Installs an observer that receives every event as it is produced,
    /// including assistant deltas that arrive mid-completion.
    ///
    /// When an observer is installed the returned event vectors are still
    /// complete; callers that journal through the observer must not journal the
    /// returned events again.
    pub fn set_event_observer(&mut self, observer: AgentEventObserver) {
        self.observer = Some(observer);
    }

    fn take_provider(&mut self) -> Result<Box<dyn ModelProvider>> {
        self.provider.take().ok_or_else(|| {
            LoomError::new(
                ErrorCode::Internal,
                "agent run has no model provider attached",
                false,
            )
        })
    }

    fn provider(&self) -> Result<&dyn ModelProvider> {
        self.provider.as_deref().ok_or_else(|| {
            LoomError::new(
                ErrorCode::Internal,
                "agent run has no model provider attached",
                false,
            )
        })
    }

    /// Publishes the events of the current call that the observer has not seen.
    fn flush_prefix(&mut self, events: &[AgentEvent]) {
        publish_events(self.observer.as_deref(), events, &mut self.flush_offset);
    }

    fn publish_progress(&mut self, result: Result<RunProgress>) -> Result<RunProgress> {
        match result {
            Ok(progress) => {
                let events = self.publish(Ok(progress.events))?;
                Ok(RunProgress {
                    events,
                    continues: progress.continues,
                })
            }
            Err(error) => {
                self.flush_offset = 0;
                Err(error)
            }
        }
    }

    /// Publishes anything left over and ends the current call.
    fn publish(&mut self, result: Result<Vec<AgentEvent>>) -> Result<Vec<AgentEvent>> {
        match result {
            Ok(events) => {
                self.flush_prefix(&events);
                self.flush_offset = 0;
                Ok(events)
            }
            Err(error) => {
                self.flush_offset = 0;
                Err(error)
            }
        }
    }

    /// Applies a pause or interrupt that was requested while the run was busy.
    fn apply_control_request(&mut self) -> Option<Vec<AgentEvent>> {
        if matches!(
            self.run.state,
            AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
        ) {
            self.control.clear_request();
            return None;
        }
        if self.control.is_interrupt_requested() {
            self.control.clear_request();
            self.step_id = None;
            let mut events = self.set_state(AgentRunState::Cancelled);
            self.run.completed_at = Some(Timestamp::now());
            self.run.summary = Some("Agent run interrupted by the user".to_owned());
            events.push(AgentEvent::RunCompleted {
                snapshot: self.run.clone(),
            });
            return Some(events);
        }
        if self.control.is_pause_requested() {
            self.control.clear_request();
            self.step_id = None;
            if self.run.state == AgentRunState::Paused {
                return Some(Vec::new());
            }
            return Some(self.set_state(AgentRunState::Paused));
        }
        None
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

    pub fn plan(&self) -> AgentPlan {
        self.plan.clone()
    }

    pub fn pending_approval(&self) -> Option<ToolCall> {
        self.pending_approval
            .as_ref()
            .map(|pending| pending.call.clone())
    }

    pub fn pending_input(&self) -> Option<String> {
        self.pending_input.clone()
    }

    pub fn messages(&self) -> Vec<ModelMessage> {
        self.messages.clone()
    }

    pub fn checkpoint_id(&self) -> Option<loom_core::CheckpointId> {
        self.options.checkpoint_id
    }

    pub fn add_evidence(&mut self, links: impl IntoIterator<Item = EvidenceLink>) {
        self.run.evidence.extend(links);
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
            pending_input: self.pending_input.clone(),
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
            activities: self.activities.clone(),
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
        if state.pending_input.is_some()
            && !matches!(
                state.run.state,
                AgentRunState::NeedsInput | AgentRunState::Paused
            )
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted input is attached to a run that is neither waiting for input nor paused",
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
            provider: Some(provider),
            tools,
            messages: state.messages,
            pending_approval: state.pending_approval.map(|call| PendingApproval { call }),
            pending_input: state.pending_input,
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
            activities: state.activities,
            control: RunControl::new(),
            observer: None,
            flush_offset: 0,
        })
    }

    pub fn start(&mut self) -> Result<Vec<AgentEvent>> {
        let result = self.start_inner();
        self.publish(result)
    }

    /// Starts the run without driving it, so an owner can register the run
    /// before any model work happens.
    pub fn begin(&mut self) -> Result<RunProgress> {
        let result = self.begin_inner();
        self.publish_progress(result)
    }

    fn start_inner(&mut self) -> Result<Vec<AgentEvent>> {
        let progress = self.begin_inner()?;
        let mut events = progress.events;
        if progress.continues {
            events.extend(self.advance()?);
        }
        Ok(events)
    }

    fn begin_inner(&mut self) -> Result<RunProgress> {
        if self.run.state != AgentRunState::Planning {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "agent run has already started",
                false,
            ));
        }
        let mut events = vec![AgentEvent::RunStarted {
            snapshot: self.run.clone(),
        }];
        if !self.plan.steps.is_empty() {
            events.push(AgentEvent::PlanProposed {
                run_id: self.run.id,
                plan: self.plan.clone(),
            });
        }
        events.extend(self.set_state(AgentRunState::Executing));
        Ok(RunProgress::running(events))
    }

    pub fn approve(&mut self, tool_call_id: loom_core::ToolCallId) -> Result<Vec<AgentEvent>> {
        let result = self.approve_inner(tool_call_id);
        self.publish(result)
    }

    /// Applies an approval and runs the approved tool without driving the run.
    pub fn approve_entry(&mut self, tool_call_id: loom_core::ToolCallId) -> Result<RunProgress> {
        let result = self.approve_entry_inner(tool_call_id);
        self.publish_progress(result)
    }

    fn approve_inner(&mut self, tool_call_id: loom_core::ToolCallId) -> Result<Vec<AgentEvent>> {
        let progress = self.approve_entry_inner(tool_call_id)?;
        let mut events = progress.events;
        if progress.continues {
            events.extend(self.advance()?);
        }
        Ok(events)
    }

    fn approve_entry_inner(&mut self, tool_call_id: loom_core::ToolCallId) -> Result<RunProgress> {
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
            return Ok(RunProgress::blocked(events));
        }
        self.last_failed_call = None;
        Ok(RunProgress::running(events))
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
            result: result.clone(),
        });
        events.push(self.complete_tool_activity(&result, AgentActivityStatus::Failed));
        events.extend(self.finish_failed(output));
        self.publish(Ok(events))
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
        self.publish(Ok(events))
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
        let events = self.set_state(AgentRunState::Paused);
        self.publish(Ok(events))
    }

    pub fn resume(&mut self) -> Result<Vec<AgentEvent>> {
        let result = self.resume_inner();
        self.publish(result)
    }

    /// Leaves the paused state without driving the run.
    pub fn resume_entry(&mut self) -> Result<RunProgress> {
        let result = self.resume_entry_inner();
        self.publish_progress(result)
    }

    fn resume_inner(&mut self) -> Result<Vec<AgentEvent>> {
        let progress = self.resume_entry_inner()?;
        let mut events = progress.events;
        if progress.continues {
            events.extend(self.advance()?);
        }
        Ok(events)
    }

    fn resume_entry_inner(&mut self) -> Result<RunProgress> {
        if self.run.state != AgentRunState::Paused {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "agent run is not paused",
                false,
            ));
        }
        let events = if self.pending_approval.is_some() {
            self.set_state(AgentRunState::AwaitingApproval)
        } else if self.pending_input.is_some() {
            self.set_state(AgentRunState::NeedsInput)
        } else {
            self.set_state(AgentRunState::Executing)
        };
        if self.pending_approval.is_none() && self.pending_input.is_none() {
            return Ok(RunProgress::running(events));
        }
        Ok(RunProgress::blocked(events))
    }

    pub fn send_message(&mut self, message: impl Into<String>) -> Result<Vec<AgentEvent>> {
        let result = self.send_message_inner(message);
        self.publish(result)
    }

    /// Records a user message without driving the run.
    pub fn message_entry(&mut self, message: impl Into<String>) -> Result<RunProgress> {
        let result = self.message_entry_inner(message);
        self.publish_progress(result)
    }

    fn send_message_inner(&mut self, message: impl Into<String>) -> Result<Vec<AgentEvent>> {
        let progress = self.message_entry_inner(message)?;
        let mut events = progress.events;
        if progress.continues {
            events.extend(self.advance()?);
        }
        Ok(events)
    }

    fn message_entry_inner(&mut self, message: impl Into<String>) -> Result<RunProgress> {
        let message = message.into();
        if message.trim().is_empty() {
            return Err(LoomError::invalid_request(
                "agent message must not be empty",
            ));
        }
        if self.pending_approval.is_some() {
            return Err(LoomError::invalid_state(
                "resolve the pending tool approval before sending a message",
            ));
        }
        if matches!(
            self.run.state,
            AgentRunState::Planning | AgentRunState::Executing | AgentRunState::Evaluating
        ) {
            return Err(LoomError::invalid_state(
                "agent run is currently processing a message",
            ));
        }
        self.messages
            .push(ModelMessage::new(MessageRole::User, message.clone()));
        self.pending_input = None;
        self.active_message_id = None;
        self.last_failed_call = None;
        self.run.completed_at = None;
        self.run.summary = None;
        let mut events = vec![AgentEvent::UserMessage {
            run_id: self.run.id,
            text: message,
        }];
        events.extend(self.set_state(AgentRunState::Executing));
        Ok(RunProgress::running(events))
    }

    pub fn request_input(&mut self, prompt: impl Into<String>) -> Result<Vec<AgentEvent>> {
        let prompt = prompt.into();
        if prompt.trim().is_empty() {
            return Err(LoomError::invalid_request(
                "agent input prompt must not be empty",
            ));
        }
        if self.pending_approval.is_some() {
            return Err(LoomError::invalid_state(
                "resolve the pending tool approval before requesting input",
            ));
        }
        if matches!(
            self.run.state,
            AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
        ) {
            return Err(LoomError::invalid_state(
                "finished agent runs cannot request input",
            ));
        }
        self.pending_input = Some(prompt.clone());
        let mut events = self.set_state(AgentRunState::NeedsInput);
        events.push(AgentEvent::NeedsInput {
            run_id: self.run.id,
            prompt,
        });
        self.publish(Ok(events))
    }

    pub fn recover_after_restart(&mut self) -> Result<Vec<AgentEvent>> {
        let events = if matches!(
            self.run.state,
            AgentRunState::Planning | AgentRunState::Executing | AgentRunState::Evaluating
        ) {
            self.set_state(AgentRunState::Paused)
        } else {
            Vec::new()
        };
        self.publish(Ok(events))
    }

    pub fn retry(&mut self) -> Result<Vec<AgentEvent>> {
        let result = self.retry_inner();
        self.publish(result)
    }

    /// Re-runs the failed tool step without driving the run.
    pub fn retry_entry(&mut self) -> Result<RunProgress> {
        let result = self.retry_entry_inner();
        self.publish_progress(result)
    }

    fn retry_inner(&mut self) -> Result<Vec<AgentEvent>> {
        let progress = self.retry_entry_inner()?;
        let mut events = progress.events;
        if progress.continues {
            events.extend(self.advance()?);
        }
        Ok(events)
    }

    fn retry_entry_inner(&mut self) -> Result<RunProgress> {
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
        events.push(self.start_tool_activity(&call, None));
        let (tool_events, result) = self.execute_tool(&call);
        events.extend(tool_events);
        if result.success {
            self.last_failed_call = None;
            return Ok(RunProgress::running(events));
        }
        events.extend(self.finish_failed(result.output));
        Ok(RunProgress::blocked(events))
    }

    pub fn retry_from_checkpoint(&mut self) -> Result<Vec<AgentEvent>> {
        let result = self.retry_from_checkpoint_inner();
        self.publish(result)
    }

    /// Resets the run to its checkpoint without driving it.
    pub fn checkpoint_retry_entry(&mut self) -> Result<RunProgress> {
        let result = self.checkpoint_retry_entry_inner();
        self.publish_progress(result)
    }

    fn retry_from_checkpoint_inner(&mut self) -> Result<Vec<AgentEvent>> {
        let progress = self.checkpoint_retry_entry_inner()?;
        let mut events = progress.events;
        if progress.continues {
            events.extend(self.advance()?);
        }
        Ok(events)
    }

    fn checkpoint_retry_entry_inner(&mut self) -> Result<RunProgress> {
        if matches!(
            self.run.state,
            AgentRunState::Planning
                | AgentRunState::Executing
                | AgentRunState::AwaitingApproval
                | AgentRunState::Evaluating
                | AgentRunState::Paused
                | AgentRunState::NeedsInput
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
        self.pending_input = None;
        self.last_failed_call = None;
        self.next_message_id = 0;
        self.active_message_id = None;
        self.usage = UsageSnapshot::default();
        self.context_inspection = None;
        if let Some(provider) = self.provider.as_mut() {
            provider.reset();
        }
        self.provider_cursor = 0;
        self.step_id = None;
        self.step_index = 0;
        let events = vec![AgentEvent::RunStateChanged {
            run_id: self.run.id,
            state: AgentRunState::Planning,
        }];
        Ok(RunProgress::running(events))
    }

    /// Drives the run until it finishes or needs a human.
    fn advance(&mut self) -> Result<Vec<AgentEvent>> {
        let mut events = Vec::new();
        loop {
            if self.advance_step(&mut events)? == StepOutcome::Blocked {
                return Ok(events);
            }
        }
    }

    /// Runs one model step. An owner that drives the run itself calls this so
    /// the run does not hold its lock across the whole loop.
    pub fn run_step(&mut self) -> Result<RunProgress> {
        let mut events = Vec::new();
        let outcome = self.advance_step(&mut events);
        match outcome {
            Ok(outcome) => {
                let events = self.publish(Ok(events))?;
                Ok(RunProgress {
                    events,
                    continues: outcome == StepOutcome::Continue,
                })
            }
            Err(error) => {
                let _ = self.publish(Ok(events));
                Err(error)
            }
        }
    }

    fn advance_step(&mut self, events: &mut Vec<AgentEvent>) -> Result<StepOutcome> {
        if let Some(control_events) = self.apply_control_request() {
            events.extend(control_events);
            return Ok(StepOutcome::Blocked);
        }
        if self.pending_approval.is_some()
            || matches!(
                self.run.state,
                AgentRunState::Completed
                    | AgentRunState::Failed
                    | AgentRunState::Cancelled
                    | AgentRunState::Paused
                    | AgentRunState::NeedsInput
            )
        {
            return Ok(StepOutcome::Blocked);
        }
        if let Some(status) = self.exceeded_limits() {
            events.push(AgentEvent::RunLimitReached {
                run_id: self.run.id,
                status,
            });
            events.extend(self.finish_failed("agent session limit reached"));
            return Ok(StepOutcome::Blocked);
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
                return Ok(StepOutcome::Blocked);
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
        let model_activity_id = ActivityId::new();
        let model_started_at = Timestamp::now();
        events.push(self.start_activity(AgentActivityRecord {
            id: model_activity_id,
            run_id: self.run.id,
            parent_id: None,
            step_id: Some(step_id),
            kind: AgentActivityKind::ModelTurn,
            status: AgentActivityStatus::Started,
            started_at: model_started_at,
            completed_at: None,
            elapsed_ms: None,
            data: AgentActivityData::ModelTurn {
                model: self.task.model.clone(),
            },
        }));
        self.provider_cursor = self.provider_cursor.saturating_add(1);
        let mut ctx = StepContext::new(step_id, self.step_index, model_activity_id);
        self.flush_prefix(events);
        let base = self.flush_offset;
        let token = self.control.stream_token();
        let mut provider = self.take_provider()?;
        let stream_result = provider.stream(&request, &token, &mut |event| {
            self.handle_stream_event(event, &mut ctx)
        });
        self.provider = Some(provider);
        let StepContext {
            events: step_events,
            published,
            saw_tool_call,
            completed,
            finished,
            ..
        } = ctx;
        events.extend(step_events);
        self.flush_offset = base.saturating_add(published);
        let model_status = match &stream_result {
            Err(_) if self.control.is_stopping() => AgentActivityStatus::Cancelled,
            Err(_) => AgentActivityStatus::Failed,
            Ok(()) if !saw_tool_call && !completed => AgentActivityStatus::Failed,
            Ok(()) => AgentActivityStatus::Completed,
        };
        events.push(self.complete_activity(model_activity_id, model_status, model_started_at));
        match stream_result {
            Ok(()) => {}
            Err(error) if self.control.is_stopping() => {
                // A pause or interrupt cancelled the in-flight completion.
                debug_assert_eq!(error.code, ErrorCode::RequestCancelled);
            }
            Err(error) => {
                self.step_id = None;
                events.push(AgentEvent::ProviderError {
                    run_id: self.run.id,
                    error: error.clone(),
                });
                events.extend(self.finish_failed(error.message));
                return Ok(StepOutcome::Blocked);
            }
        }
        if let Some(events_from_control) = self.apply_control_request() {
            events.extend(events_from_control);
            return Ok(StepOutcome::Blocked);
        }
        if finished {
            return Ok(StepOutcome::Blocked);
        }
        if !saw_tool_call && !completed {
            self.step_id = None;
            events.extend(self.finish_failed("model returned an empty stream"));
            return Ok(StepOutcome::Blocked);
        }
        if completed && !saw_tool_call {
            return Ok(StepOutcome::Blocked);
        }
        Ok(StepOutcome::Continue)
    }

    /// Applies one streamed model event to the run.
    ///
    /// This runs inside the provider's stream callback, so an assistant delta is
    /// journaled while the completion is still arriving.
    fn handle_stream_event(
        &mut self,
        event: ModelStreamEvent,
        ctx: &mut StepContext,
    ) -> Result<StreamFlow> {
        let flow = self.handle_stream_event_inner(event, ctx);
        publish_events(self.observer.as_deref(), &ctx.events, &mut ctx.published);
        flow
    }

    fn handle_stream_event_inner(
        &mut self,
        event: ModelStreamEvent,
        ctx: &mut StepContext,
    ) -> Result<StreamFlow> {
        match event {
            ModelStreamEvent::TextDelta { text } => {
                if !text.is_empty() {
                    self.append_assistant_text(&text);
                    let message_id = self.assistant_message_id();
                    ctx.events.push(AgentEvent::AssistantMessageDelta {
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
                    ctx.events.push(AgentEvent::RunLimitReached {
                        run_id: self.run.id,
                        status,
                    });
                    ctx.events
                        .extend(self.finish_failed("agent session limit reached"));
                    self.step_id = None;
                    ctx.finished = true;
                    return Ok(StreamFlow::Stop);
                }
                self.usage.add_tool_call();
                self.append_assistant_tool_call(call.clone());
                ctx.events.push(AgentEvent::RunUsageUpdated {
                    run_id: self.run.id,
                    usage: self.usage.clone(),
                });
                ctx.saw_tool_call = true;
                ctx.events
                    .push(self.start_tool_activity(&call, Some(ctx.activity_id)));
                ctx.events.push(AgentEvent::ToolCallRequested {
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
                    ctx.events.push(AgentEvent::ToolCallCompleted {
                        run_id: self.run.id,
                        result: result.clone(),
                    });
                    ctx.events
                        .push(self.complete_tool_activity(&result, AgentActivityStatus::Failed));
                    self.messages.push(ModelMessage {
                        role: MessageRole::Tool,
                        content: output.clone(),
                        name: Some(result.name.clone()),
                        tool_call_id: Some(result.tool_call_id),
                        tool_calls: Vec::new(),
                    });
                    self.last_failed_call = Some(call);
                    self.step_id = None;
                    self.step_index = self.step_index.saturating_add(1);
                    ctx.events.push(AgentEvent::StepCompleted {
                        run_id: self.run.id,
                        step_id: ctx.step_id,
                        index: ctx.step_index,
                    });
                    ctx.events.extend(self.finish_failed(output));
                    ctx.finished = true;
                    return Ok(StreamFlow::Stop);
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
                ctx.events.push(AgentEvent::ToolPolicyEvaluated {
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
                    ctx.events.push(AgentEvent::ToolCallCompleted {
                        run_id: self.run.id,
                        result: result.clone(),
                    });
                    ctx.events
                        .push(self.complete_tool_activity(&result, AgentActivityStatus::Failed));
                    self.step_id = None;
                    self.step_index = self.step_index.saturating_add(1);
                    ctx.events.push(AgentEvent::StepCompleted {
                        run_id: self.run.id,
                        step_id: ctx.step_id,
                        index: ctx.step_index,
                    });
                    ctx.events
                        .extend(self.finish_failed("tool call denied by the workspace policy"));
                    ctx.finished = true;
                    return Ok(StreamFlow::Stop);
                }
                if matches!(
                    evaluation.decision,
                    loom_core::PolicyDecision::RequireApproval
                ) {
                    self.pending_approval = Some(PendingApproval { call: call.clone() });
                    self.step_id = None;
                    self.step_index = self.step_index.saturating_add(1);
                    ctx.events.push(AgentEvent::StepCompleted {
                        run_id: self.run.id,
                        step_id: ctx.step_id,
                        index: ctx.step_index,
                    });
                    ctx.events
                        .extend(self.set_state(AgentRunState::AwaitingApproval));
                    ctx.events.push(
                        self.update_activity_status(call.id, AgentActivityStatus::AwaitingApproval),
                    );
                    ctx.events.push(AgentEvent::ToolApprovalRequired {
                        run_id: self.run.id,
                        call,
                    });
                    ctx.finished = true;
                    return Ok(StreamFlow::Stop);
                }
                if kind == ToolKind::ProposePlan {
                    let steps = call
                        .arguments
                        .get("steps")
                        .and_then(serde_json::Value::as_array)
                        .map(|steps| {
                            steps
                                .iter()
                                .enumerate()
                                .filter_map(|(index, step)| {
                                    step.as_str().filter(|step| !step.trim().is_empty()).map(
                                        |description| AgentPlanStep {
                                            id: format!("step-{}", index + 1),
                                            description: description.to_owned(),
                                        },
                                    )
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    if steps.is_empty() {
                        let output = "propose_plan requires a non-empty steps array".to_owned();
                        let result = ToolResult {
                            tool_call_id: call.id,
                            name: call.name.clone(),
                            success: false,
                            output: output.clone(),
                        };
                        ctx.events.push(AgentEvent::ToolCallCompleted {
                            run_id: self.run.id,
                            result: result.clone(),
                        });
                        ctx.events.push(
                            self.complete_tool_activity(&result, AgentActivityStatus::Failed),
                        );
                        self.step_id = None;
                        self.step_index = self.step_index.saturating_add(1);
                        ctx.events.push(AgentEvent::StepCompleted {
                            run_id: self.run.id,
                            step_id: ctx.step_id,
                            index: ctx.step_index,
                        });
                        ctx.events.extend(self.finish_failed(output));
                        ctx.finished = true;
                        return Ok(StreamFlow::Stop);
                    }
                    self.plan = AgentPlan {
                        steps: steps.clone(),
                    };
                    ctx.events.push(AgentEvent::PlanProposed {
                        run_id: self.run.id,
                        plan: self.plan.clone(),
                    });
                    let result = ToolResult::success(&call, "plan proposed".to_owned());
                    ctx.events.push(AgentEvent::ToolCallCompleted {
                        run_id: self.run.id,
                        result: result.clone(),
                    });
                    ctx.events
                        .push(self.complete_tool_activity(&result, AgentActivityStatus::Completed));
                    self.step_id = None;
                    self.step_index = self.step_index.saturating_add(1);
                    ctx.events.push(AgentEvent::StepCompleted {
                        run_id: self.run.id,
                        step_id: ctx.step_id,
                        index: ctx.step_index,
                    });
                    return Ok(StreamFlow::Stop);
                }
                if kind == ToolKind::AskUser {
                    let Some(prompt) = call
                        .arguments
                        .get("prompt")
                        .and_then(serde_json::Value::as_str)
                        .filter(|prompt| !prompt.trim().is_empty())
                    else {
                        let output = "ask_user requires a non-empty prompt".to_owned();
                        let result = ToolResult {
                            tool_call_id: call.id,
                            name: call.name.clone(),
                            success: false,
                            output: output.clone(),
                        };
                        ctx.events.push(AgentEvent::ToolCallCompleted {
                            run_id: self.run.id,
                            result: result.clone(),
                        });
                        ctx.events.push(
                            self.complete_tool_activity(&result, AgentActivityStatus::Failed),
                        );
                        self.step_id = None;
                        self.step_index = self.step_index.saturating_add(1);
                        ctx.events.push(AgentEvent::StepCompleted {
                            run_id: self.run.id,
                            step_id: ctx.step_id,
                            index: ctx.step_index,
                        });
                        ctx.events.extend(self.finish_failed(output));
                        ctx.finished = true;
                        return Ok(StreamFlow::Stop);
                    };
                    self.pending_input = Some(prompt.to_owned());
                    self.step_id = None;
                    self.step_index = self.step_index.saturating_add(1);
                    ctx.events.push(AgentEvent::StepCompleted {
                        run_id: self.run.id,
                        step_id: ctx.step_id,
                        index: ctx.step_index,
                    });
                    ctx.events.extend(self.set_state(AgentRunState::NeedsInput));
                    ctx.events.push(
                        self.update_activity_status(call.id, AgentActivityStatus::AwaitingInput),
                    );
                    ctx.events.push(AgentEvent::NeedsInput {
                        run_id: self.run.id,
                        prompt: prompt.to_owned(),
                    });
                    ctx.finished = true;
                    return Ok(StreamFlow::Stop);
                }
                let (tool_events, result) = self.execute_tool(&call);
                ctx.events.extend(tool_events);
                if result.success {
                    self.last_failed_call = None;
                } else {
                    self.last_failed_call = Some(call);
                    ctx.events.extend(self.finish_failed(result.output));
                    ctx.finished = true;
                    return Ok(StreamFlow::Stop);
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
                ctx.events.push(AgentEvent::RunUsage {
                    run_id: self.run.id,
                    usage,
                });
                ctx.events.push(AgentEvent::RunUsageUpdated {
                    run_id: self.run.id,
                    usage: self.usage.clone(),
                });
                if let Some(status) = self.exceeded_limits() {
                    ctx.events.push(AgentEvent::RunLimitReached {
                        run_id: self.run.id,
                        status,
                    });
                    ctx.events
                        .extend(self.finish_failed("agent session limit reached"));
                    self.step_id = None;
                    ctx.finished = true;
                    return Ok(StreamFlow::Stop);
                }
            }
            ModelStreamEvent::Completed { reason } => {
                ctx.completed = true;
                self.step_id = None;
                self.step_index = self.step_index.saturating_add(1);
                ctx.events.push(AgentEvent::StepCompleted {
                    run_id: self.run.id,
                    step_id: ctx.step_id,
                    index: ctx.step_index,
                });
                if !ctx.saw_tool_call {
                    if matches!(reason, loom_model::FinishReason::Stop) {
                        ctx.events.extend(self.finish_completed());
                    } else if matches!(reason, loom_model::FinishReason::Cancelled) {
                        ctx.events.extend(self.finish_cancelled());
                    } else {
                        ctx.events
                            .extend(self.finish_failed(format!("model finished with {reason:?}")));
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
            ctx.finished = true;
            return Ok(StreamFlow::Stop);
        }
        if self.control.is_stopping() {
            return Ok(StreamFlow::Stop);
        }
        Ok(StreamFlow::Continue)
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
        events.push(self.complete_tool_activity(
            &result,
            if result.success {
                AgentActivityStatus::Completed
            } else {
                AgentActivityStatus::Failed
            },
        ));
        self.messages.push(ModelMessage {
            role: MessageRole::Tool,
            content: result.output.clone(),
            name: Some(result.name.clone()),
            tool_call_id: Some(result.tool_call_id),
            tool_calls: Vec::new(),
        });
        (events, result)
    }

    fn start_activity(&mut self, activity: AgentActivityRecord) -> AgentEvent {
        let run_id = activity.run_id;
        self.activities.push(activity.clone());
        AgentEvent::ActivityRecorded { run_id, activity }
    }

    fn start_tool_activity(
        &mut self,
        call: &ToolCall,
        parent_id: Option<ActivityId>,
    ) -> AgentEvent {
        let (kind, data) = activity_data_for_call(call, None);
        self.start_activity(AgentActivityRecord {
            id: ActivityId::new(),
            run_id: self.run.id,
            parent_id,
            step_id: self.step_id,
            kind,
            status: AgentActivityStatus::Started,
            started_at: Timestamp::now(),
            completed_at: None,
            elapsed_ms: None,
            data,
        })
    }

    fn update_activity_status(
        &mut self,
        tool_call_id: loom_core::ToolCallId,
        status: AgentActivityStatus,
    ) -> AgentEvent {
        let index = self
            .activities
            .iter()
            .rposition(|activity| activity_contains_call(activity, tool_call_id))
            .expect("tool activity must be started before its status changes");
        let mut activity = self.activities[index].clone();
        activity.status = status;
        self.activities[index] = activity.clone();
        AgentEvent::ActivityRecorded {
            run_id: self.run.id,
            activity,
        }
    }

    fn complete_tool_activity(
        &mut self,
        result: &ToolResult,
        status: AgentActivityStatus,
    ) -> AgentEvent {
        let index = self
            .activities
            .iter()
            .rposition(|activity| activity_contains_call(activity, result.tool_call_id));
        let Some(index) = index else {
            return self.start_activity(AgentActivityRecord {
                id: ActivityId::new(),
                run_id: self.run.id,
                parent_id: None,
                step_id: self.step_id,
                kind: AgentActivityKind::ToolCall,
                status,
                started_at: Timestamp::now(),
                completed_at: Some(Timestamp::now()),
                elapsed_ms: Some(0),
                data: AgentActivityData::ToolCall {
                    call: ToolCall {
                        id: result.tool_call_id,
                        name: result.name.clone(),
                        arguments: serde_json::Value::Null,
                    },
                    result: Some(result.clone()),
                },
            });
        };
        let mut activity = self.activities[index].clone();
        let completed_at = Timestamp::now();
        activity.status = status;
        activity.completed_at = Some(completed_at);
        activity.elapsed_ms = Some(
            completed_at
                .as_unix_millis()
                .saturating_sub(activity.started_at.as_unix_millis()),
        );
        activity.data = activity_data_with_result(activity.data, result.clone());
        self.activities[index] = activity.clone();
        AgentEvent::ActivityRecorded {
            run_id: self.run.id,
            activity,
        }
    }

    fn complete_activity(
        &mut self,
        activity_id: ActivityId,
        status: AgentActivityStatus,
        started_at: Timestamp,
    ) -> AgentEvent {
        let completed_at = Timestamp::now();
        let activity = self
            .activities
            .iter_mut()
            .find(|activity| activity.id == activity_id)
            .expect("started activity must be present");
        activity.status = status;
        activity.completed_at = Some(completed_at);
        activity.elapsed_ms = Some(
            completed_at
                .as_unix_millis()
                .saturating_sub(started_at.as_unix_millis()),
        );
        AgentEvent::ActivityRecorded {
            run_id: self.run.id,
            activity: activity.clone(),
        }
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
                self.provider()?.descriptor().context_window.map(u64::from);
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
        let tools = if self.provider()?.descriptor().capabilities.tool_calling {
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
        let request_tokens = self.provider()?.count_tokens(&request);
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

    fn append_assistant_tool_call(&mut self, call: ToolCall) {
        if let Some(last) = self.messages.last_mut()
            && last.role == MessageRole::Assistant
        {
            last.tool_calls.push(call);
            return;
        }
        self.messages.push(ModelMessage {
            role: MessageRole::Assistant,
            content: String::new(),
            name: None,
            tool_call_id: None,
            tool_calls: vec![call],
        });
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

fn activity_data_for_call(
    call: &ToolCall,
    result: Option<ToolResult>,
) -> (AgentActivityKind, AgentActivityData) {
    let string_argument = |name: &str| {
        call.arguments
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    match call.name.as_str() {
        "list_files" => (
            AgentActivityKind::File,
            AgentActivityData::File {
                call: call.clone(),
                operation: FileActivityOperation::List,
                path: string_argument("path").or_else(|| Some(".".to_owned())),
                result,
            },
        ),
        "read_file" => (
            AgentActivityKind::File,
            AgentActivityData::File {
                call: call.clone(),
                operation: FileActivityOperation::Read,
                path: string_argument("path"),
                result,
            },
        ),
        "apply_patch" => (
            AgentActivityKind::File,
            AgentActivityData::File {
                call: call.clone(),
                operation: FileActivityOperation::Write,
                path: string_argument("path"),
                result,
            },
        ),
        "search_text" => (
            AgentActivityKind::Search,
            AgentActivityData::Search {
                call: call.clone(),
                query: string_argument("query").unwrap_or_default(),
                path: string_argument("path"),
                result,
            },
        ),
        "run_command" => (
            AgentActivityKind::Command,
            AgentActivityData::Command {
                call: call.clone(),
                command: string_argument("command").unwrap_or_default(),
                args: call
                    .arguments
                    .get("args")
                    .and_then(serde_json::Value::as_array)
                    .map(|args| {
                        args.iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default(),
                cwd: string_argument("cwd"),
                result,
            },
        ),
        _ => (
            AgentActivityKind::ToolCall,
            AgentActivityData::ToolCall {
                call: call.clone(),
                result,
            },
        ),
    }
}

fn activity_data_with_result(data: AgentActivityData, result: ToolResult) -> AgentActivityData {
    match data {
        AgentActivityData::ModelTurn { model } => AgentActivityData::ModelTurn { model },
        AgentActivityData::ToolCall { call, .. } => AgentActivityData::ToolCall {
            call,
            result: Some(result),
        },
        AgentActivityData::File {
            call,
            operation,
            path,
            ..
        } => AgentActivityData::File {
            call,
            operation,
            path,
            result: Some(result),
        },
        AgentActivityData::Search {
            call, query, path, ..
        } => AgentActivityData::Search {
            call,
            query,
            path,
            result: Some(result),
        },
        AgentActivityData::Command {
            call,
            command,
            args,
            cwd,
            ..
        } => AgentActivityData::Command {
            call,
            command,
            args,
            cwd,
            result: Some(result),
        },
    }
}

fn activity_contains_call(
    activity: &AgentActivityRecord,
    tool_call_id: loom_core::ToolCallId,
) -> bool {
    match &activity.data {
        AgentActivityData::ToolCall { call, .. } => call.id == tool_call_id,
        AgentActivityData::File { call, .. }
        | AgentActivityData::Search { call, .. }
        | AgentActivityData::Command { call, .. } => call.id == tool_call_id,
        AgentActivityData::ModelTurn { .. } => false,
    }
}

fn initial_messages(task: &AgentTask) -> Vec<ModelMessage> {
    let mut messages = Vec::new();
    messages.push(ModelMessage::new(
        MessageRole::System,
        "Use propose_plan for an ordered plan before workspace changes, and use ask_user when information from the user is required to continue.",
    ));
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

/// Sends the events that the observer has not seen yet and advances `cursor`.
fn publish_events(
    observer: Option<&(dyn Fn(&AgentEvent) + Send + Sync)>,
    events: &[AgentEvent],
    cursor: &mut usize,
) {
    if let Some(observer) = observer {
        for event in events.iter().skip(*cursor) {
            observer(event);
        }
    }
    *cursor = events.len();
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
        let activities = runtime.export_state().activities;
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
        assert_eq!(runtime.snapshot().state, AgentRunState::Completed);
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
        assert_eq!(runtime.snapshot().state, AgentRunState::NeedsInput);
        assert!(requested.iter().any(|event| {
            matches!(event, AgentEvent::NeedsInput { prompt, .. } if prompt.contains("validation"))
        }));

        let continued = runtime
            .send_message("Run the standard validation.")
            .unwrap();
        assert!(continued.iter().any(|event| {
            matches!(event, AgentEvent::UserMessage { text, .. } if text.contains("standard"))
        }));
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
