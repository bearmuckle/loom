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
    ApprovalDecision, FileActivityOperation,
};
use loom_tools::{ToolExecutor, ToolKind, ToolResult};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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
    /// Persisted grant for integrating reviewed child worktree commits.
    #[serde(default)]
    pub project_integration_enabled: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentRuntimeState {
    pub session_id: AgentSessionId,
    pub task: AgentTask,
    pub run: AgentRunSnapshot,
    pub plan: AgentPlan,
    pub messages: Vec<ModelMessage>,
    #[serde(default)]
    pub last_project_message_sequence: u64,
    #[serde(default)]
    pub attempts: Vec<loom_protocol::AgentRunAttemptRecord>,
    pub pending_approval: Option<ToolCall>,
    #[serde(default)]
    pub pending_tool_execution: Option<ToolCall>,
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
    attempts: Vec<loom_protocol::AgentRunAttemptRecord>,
    plan: AgentPlan,
    provider: Option<Box<dyn ModelProvider>>,
    tools: ToolExecutor,
    messages: Vec<ModelMessage>,
    last_project_message_sequence: u64,
    pending_approval: Option<PendingApproval>,
    pending_tool_execution: Option<ToolCall>,
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
            attempt_id: loom_core::RunAttemptId::new(),
            control_revision: 0,
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
        let attempts = vec![loom_protocol::AgentRunAttemptRecord {
            run_id: run.id,
            session_id,
            id: run.attempt_id,
            number: 1,
            state: run.state,
            checkpoint_id: options.checkpoint_id,
            started_at: now,
            completed_at: None,
        }];
        let plan = AgentPlan { steps: Vec::new() };
        let messages = initial_messages(&task);
        Self {
            session_id,
            task,
            run,
            attempts,
            plan,
            provider: Some(provider),
            tools,
            messages,
            last_project_message_sequence: 0,
            pending_approval: None,
            pending_tool_execution: None,
            pending_input: None,
            last_failed_call: None,
            next_message_id: 0,
            active_message_id: None,
            approval_policy,
            options,
            usage: UsageSnapshot::default(),
            context_inspection: None,
            context_checkpoint: None,
            provider_cursor: 0,
            step_id: None,
            step_index: 0,
            activities: Vec::new(),
            interactions: Vec::new(),
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

    pub fn last_project_message_sequence(&self) -> u64 {
        self.last_project_message_sequence
    }

    /// Sets the durable inbox cursor before a new run begins in this session.
    pub fn set_project_message_cursor(&mut self, sequence: u64) {
        self.last_project_message_sequence = self.last_project_message_sequence.max(sequence);
    }

    /// Injects one durable project message at a safe model-turn boundary.
    /// Returns false when it was already delivered or the runtime is blocked.
    pub fn append_project_message(&mut self, message: &AgentMessageRecord) -> Result<bool> {
        if message.project_sequence <= self.last_project_message_sequence {
            return Ok(false);
        }
        if self.pending_tool_execution.is_some()
            || self.pending_approval.is_some()
            || self.pending_input.is_some()
            || !matches!(
                self.run.state,
                AgentRunState::Planning | AgentRunState::Executing | AgentRunState::Evaluating
            )
        {
            return Ok(false);
        }
        let kind = match message.kind {
            loom_core::AgentMessageKind::Progress => "progress",
            loom_core::AgentMessageKind::Result => "result",
            loom_core::AgentMessageKind::Question => "question",
            loom_core::AgentMessageKind::Blocker => "blocker",
            loom_core::AgentMessageKind::Direction => "direction",
            loom_core::AgentMessageKind::Answer => "answer",
        };
        let task = message
            .task_id
            .map(|task_id| format!("; task {task_id}"))
            .unwrap_or_default();
        let mut model_message = ModelMessage::new(
            MessageRole::User,
            format!(
                "[Project message {} from agent {} ({kind}{task})]\n{}",
                message.project_sequence, message.sender_session_id, message.body
            ),
        );
        model_message.name = Some("loom_project_message".to_owned());
        self.messages.push(model_message);
        self.last_project_message_sequence = message.project_sequence;
        Ok(true)
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
            last_project_message_sequence: self.last_project_message_sequence,
            attempts: self.attempts.clone(),
            pending_approval: self
                .pending_approval
                .as_ref()
                .map(|pending| pending.call.clone()),
            pending_tool_execution: self.pending_tool_execution.clone(),
            pending_input: self.pending_input.clone(),
            last_failed_call: self.last_failed_call.clone(),
            next_message_id: self.next_message_id,
            active_message_id: self.active_message_id,
            approval_policy: self.approval_policy.clone(),
            options: self.options.clone(),
            usage: self.usage.clone(),
            context_inspection: self.context_inspection.clone(),
            context_checkpoint: self.context_checkpoint.clone(),
            provider_cursor: self.provider_cursor,
            step_id: self.step_id,
            step_index: self.step_index,
            activities: self.activities.clone(),
            interactions: self.interactions.clone(),
        }
    }

    pub fn from_state(
        mut state: AgentRuntimeState,
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
            state.pending_approval = None;
        }
        if state.pending_input.is_some()
            && !matches!(
                state.run.state,
                AgentRunState::NeedsInput | AgentRunState::Paused
            )
        {
            state.pending_input = None;
        }
        let resolved_at = Timestamp::now();
        for interaction in &mut state.interactions {
            if interaction.status != loom_protocol::AgentInteractionStatus::Pending {
                continue;
            }
            let active = interaction.attempt_id == state.run.attempt_id
                && interaction.control_revision == state.run.control_revision
                && match interaction.kind {
                    loom_protocol::AgentInteractionKind::ToolApproval => state
                        .pending_approval
                        .as_ref()
                        .is_some_and(|call| interaction.tool_call_id == Some(call.id)),
                    loom_protocol::AgentInteractionKind::UserInput => state.pending_input.is_some(),
                };
            if !active {
                interaction.status = loom_protocol::AgentInteractionStatus::Abandoned;
                interaction.resolved_at = Some(resolved_at);
            }
        }
        if state.attempts.is_empty() {
            state.attempts.push(loom_protocol::AgentRunAttemptRecord {
                run_id: state.run.id,
                session_id: state.session_id,
                id: state.run.attempt_id,
                number: 1,
                state: state.run.state,
                checkpoint_id: state.options.checkpoint_id,
                started_at: state.run.started_at,
                completed_at: state.run.completed_at,
            });
        }
        if !state.attempts.iter().any(|attempt| {
            attempt.id == state.run.attempt_id
                && attempt.run_id == state.run.id
                && attempt.session_id == state.session_id
                && attempt.state == state.run.state
        }) {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "agent runtime state does not contain its current attempt",
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
            attempts: state.attempts,
            plan: state.plan,
            provider: Some(provider),
            tools,
            messages: state.messages,
            last_project_message_sequence: state.last_project_message_sequence,
            pending_approval: state.pending_approval.map(|call| PendingApproval { call }),
            pending_tool_execution: state.pending_tool_execution,
            pending_input: state.pending_input,
            last_failed_call: state.last_failed_call,
            next_message_id: state.next_message_id,
            active_message_id: state.active_message_id,
            approval_policy: state.approval_policy,
            options: state.options,
            usage: state.usage,
            context_inspection: state.context_inspection,
            context_checkpoint: state.context_checkpoint,
            provider_cursor: state.provider_cursor,
            step_id: state.step_id,
            step_index: state.step_index,
            activities: state.activities,
            interactions: state.interactions,
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
        let attempt_id = self.run.attempt_id;
        let control_revision = self.run.control_revision;
        let result = self.approve_inner(tool_call_id, attempt_id, control_revision);
        self.publish(result)
    }

    /// Applies an approval and queues the tool for the run driver.
    pub fn approve_entry(
        &mut self,
        tool_call_id: loom_core::ToolCallId,
        attempt_id: loom_core::RunAttemptId,
        expected_control_revision: u64,
    ) -> Result<RunProgress> {
        let result = self.approve_entry_inner(tool_call_id, attempt_id, expected_control_revision);
        self.publish_progress(result)
    }

    fn approve_inner(
        &mut self,
        tool_call_id: loom_core::ToolCallId,
        attempt_id: loom_core::RunAttemptId,
        expected_control_revision: u64,
    ) -> Result<Vec<AgentEvent>> {
        let progress =
            self.approve_entry_inner(tool_call_id, attempt_id, expected_control_revision)?;
        let mut events = progress.events;
        if progress.continues {
            events.extend(self.advance()?);
        }
        Ok(events)
    }

    fn approve_entry_inner(
        &mut self,
        tool_call_id: loom_core::ToolCallId,
        attempt_id: loom_core::RunAttemptId,
        expected_control_revision: u64,
    ) -> Result<RunProgress> {
        self.validate_control_revision(attempt_id, expected_control_revision)?;
        if self.run.state != AgentRunState::AwaitingApproval {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "agent run is not waiting for tool approval",
                false,
            ));
        }
        let interaction_id = self.pending_interaction_id(
            loom_protocol::AgentInteractionKind::ToolApproval,
            attempt_id,
            expected_control_revision,
            Some(tool_call_id),
        )?;
        let control_revision = self.next_control_revision()?;
        let pending = self.take_pending(tool_call_id)?;
        self.run.control_revision = control_revision;
        self.resolve_interaction(
            interaction_id,
            loom_protocol::AgentInteractionStatus::Approved,
            Some(ApprovalDecision::Approved),
        )?;
        self.pending_tool_execution = Some(pending.call.clone());
        let mut events = vec![AgentEvent::ToolApprovalDecided {
            run_id: self.run.id,
            attempt_id: self.run.attempt_id,
            control_revision,
            interaction_id,
            tool_call_id,
            decision: ApprovalDecision::Approved,
        }];
        events.extend(self.set_state(AgentRunState::Executing));
        events.push(self.update_activity_status(tool_call_id, AgentActivityStatus::Started));
        Ok(RunProgress::running(events))
    }

    pub fn reject(
        &mut self,
        tool_call_id: loom_core::ToolCallId,
        reason: Option<String>,
    ) -> Result<Vec<AgentEvent>> {
        let attempt_id = self.run.attempt_id;
        let control_revision = self.run.control_revision;
        let result = self.reject_entry_inner(tool_call_id, reason, attempt_id, control_revision);
        self.publish(result.map(|progress| progress.events))
    }

    /// Applies a rejection using the interaction revision observed by the client.
    pub fn reject_entry(
        &mut self,
        tool_call_id: loom_core::ToolCallId,
        reason: Option<String>,
        attempt_id: loom_core::RunAttemptId,
        expected_control_revision: u64,
    ) -> Result<RunProgress> {
        let result =
            self.reject_entry_inner(tool_call_id, reason, attempt_id, expected_control_revision);
        self.publish_progress(result)
    }

    fn reject_entry_inner(
        &mut self,
        tool_call_id: loom_core::ToolCallId,
        reason: Option<String>,
        attempt_id: loom_core::RunAttemptId,
        expected_control_revision: u64,
    ) -> Result<RunProgress> {
        self.validate_control_revision(attempt_id, expected_control_revision)?;
        if self.run.state != AgentRunState::AwaitingApproval {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "agent run is not waiting for tool approval",
                false,
            ));
        }
        let interaction_id = self.pending_interaction_id(
            loom_protocol::AgentInteractionKind::ToolApproval,
            attempt_id,
            expected_control_revision,
            Some(tool_call_id),
        )?;
        let control_revision = self.next_control_revision()?;
        let pending = self.take_pending(tool_call_id)?;
        self.run.control_revision = control_revision;
        self.resolve_interaction(
            interaction_id,
            loom_protocol::AgentInteractionStatus::Rejected,
            Some(ApprovalDecision::Rejected),
        )?;
        let mut events = vec![AgentEvent::ToolApprovalDecided {
            run_id: self.run.id,
            attempt_id: self.run.attempt_id,
            control_revision,
            interaction_id,
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
        Ok(RunProgress::blocked(events))
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
        let attempt_id = self.run.attempt_id;
        let control_revision = self.run.control_revision;
        let result = self.message_entry_inner(message, attempt_id, control_revision);
        self.publish_progress(result)
    }

    /// Records a user message only if it targets the current attempt revision.
    pub fn message_entry_at_revision(
        &mut self,
        message: impl Into<String>,
        attempt_id: loom_core::RunAttemptId,
        expected_control_revision: u64,
    ) -> Result<RunProgress> {
        let result = self.message_entry_inner(message, attempt_id, expected_control_revision);
        self.publish_progress(result)
    }

    fn send_message_inner(&mut self, message: impl Into<String>) -> Result<Vec<AgentEvent>> {
        let attempt_id = self.run.attempt_id;
        let control_revision = self.run.control_revision;
        let progress = self.message_entry_inner(message, attempt_id, control_revision)?;
        let mut events = progress.events;
        if progress.continues {
            events.extend(self.advance()?);
        }
        Ok(events)
    }

    fn message_entry_inner(
        &mut self,
        message: impl Into<String>,
        attempt_id: loom_core::RunAttemptId,
        expected_control_revision: u64,
    ) -> Result<RunProgress> {
        self.validate_control_revision(attempt_id, expected_control_revision)?;
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
        let interaction_id = if self.pending_input.is_some() {
            Some(self.pending_interaction_id(
                loom_protocol::AgentInteractionKind::UserInput,
                attempt_id,
                expected_control_revision,
                None,
            )?)
        } else {
            None
        };
        let control_revision = self.next_control_revision()?;
        if let Some(interaction_id) = interaction_id {
            self.resolve_interaction(
                interaction_id,
                loom_protocol::AgentInteractionStatus::Answered,
                None,
            )?;
        }
        self.messages
            .push(ModelMessage::new(MessageRole::User, message.clone()));
        self.pending_input = None;
        self.run.control_revision = control_revision;
        self.active_message_id = None;
        self.last_failed_call = None;
        self.run.completed_at = None;
        self.run.summary = None;
        let mut events = vec![AgentEvent::UserMessage {
            run_id: self.run.id,
            attempt_id: self.run.attempt_id,
            control_revision,
            interaction_id,
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
        if prompt.len() > MAX_INTERACTION_PROMPT_BYTES {
            return Err(LoomError::invalid_request(
                "agent input prompt exceeds the supported size limit",
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
        let previous_interaction = if self.pending_input.is_some() {
            Some(self.pending_interaction_id(
                loom_protocol::AgentInteractionKind::UserInput,
                self.run.attempt_id,
                self.run.control_revision,
                None,
            )?)
        } else {
            None
        };
        let control_revision = self.next_control_revision()?;
        if let Some(interaction_id) = previous_interaction {
            self.resolve_interaction(
                interaction_id,
                loom_protocol::AgentInteractionStatus::Abandoned,
                None,
            )?;
        }
        let interaction_id = self.open_interaction(
            loom_protocol::AgentInteractionKind::UserInput,
            prompt.clone(),
            None,
            control_revision,
        );
        self.pending_input = Some(prompt.clone());
        self.run.control_revision = control_revision;
        let mut events = self.set_state(AgentRunState::NeedsInput);
        events.push(AgentEvent::NeedsInput {
            run_id: self.run.id,
            attempt_id: self.run.attempt_id,
            control_revision,
            interaction_id,
            prompt,
        });
        self.publish(Ok(events))
    }

    pub fn recover_after_restart(&mut self) -> Result<Vec<AgentEvent>> {
        let mut events = Vec::new();
        if let Some(call) = self.pending_tool_execution.take() {
            self.last_failed_call = Some(call);
            events.push(AgentEvent::RecoveryRequired {
                run_id: self.run.id,
                reason: "an approved tool execution was interrupted; its external outcome is unknown and it was not replayed".to_owned(),
            });
            events.extend(
                self.finish_failed(
                    "approved tool execution was interrupted with an unknown outcome",
                ),
            );
            return self.publish(Ok(events));
        }
        if matches!(
            self.run.state,
            AgentRunState::Planning | AgentRunState::Executing | AgentRunState::Evaluating
        ) {
            events.extend(self.set_state(AgentRunState::Paused));
        }
        self.publish(Ok(events))
    }

    pub fn retry(&mut self) -> Result<Vec<AgentEvent>> {
        let result = self.retry_inner();
        self.publish(result)
    }

    /// Queues the failed tool step for the run driver to retry.
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
        self.pending_tool_execution = Some(call);
        Ok(RunProgress::running(events))
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
        let previous_attempt_id = self.run.attempt_id;
        self.abandon_pending_interactions(previous_attempt_id);
        let attempt_number = self
            .attempts
            .iter()
            .map(|attempt| attempt.number)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::InvalidState,
                    "run attempt number is exhausted",
                    false,
                )
            })?;
        self.run.attempt_id = loom_core::RunAttemptId::new();
        self.run.control_revision = 0;
        self.run.completed_at = None;
        self.run.summary = None;
        let started_at = Timestamp::now();
        self.run.updated_at = started_at;
        self.run.state = AgentRunState::Planning;
        self.attempts.push(loom_protocol::AgentRunAttemptRecord {
            run_id: self.run.id,
            session_id: self.session_id,
            id: self.run.attempt_id,
            number: attempt_number,
            state: AgentRunState::Planning,
            checkpoint_id: self.options.checkpoint_id,
            started_at,
            completed_at: None,
        });
        self.messages = initial_messages(&self.task);
        self.pending_approval = None;
        self.pending_tool_execution = None;
        self.pending_input = None;
        self.last_failed_call = None;
        self.next_message_id = 0;
        self.active_message_id = None;
        self.usage = UsageSnapshot::default();
        self.context_inspection = None;
        self.context_checkpoint = None;
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
        if let Some(call) = self.pending_tool_execution.take() {
            let (tool_events, _result) = self.execute_tool(&call);
            events.extend(tool_events);
            self.last_failed_call = None;
            return Ok(StepOutcome::Continue);
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
        self.context_checkpoint = inspection.summary.clone();
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
                let kind = ToolKind::from_name(&call.name);
                let Some(action_kind) = self.tools.action_kind(&call) else {
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
                    self.step_id = None;
                    self.step_index = self.step_index.saturating_add(1);
                    ctx.events.push(AgentEvent::StepCompleted {
                        run_id: self.run.id,
                        step_id: ctx.step_id,
                        index: ctx.step_index,
                    });
                    return Ok(StreamFlow::Stop);
                };
                let evaluation = self
                    .tools
                    .policy_evaluation(&call, &self.approval_policy)
                    .unwrap_or_else(|| {
                        PolicyEvaluation::evaluate(&self.approval_policy, action_kind, &call.name)
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
                    self.messages.push(ModelMessage {
                        role: MessageRole::Tool,
                        content: result.output,
                        name: Some(result.name),
                        tool_call_id: Some(result.tool_call_id),
                        tool_calls: Vec::new(),
                    });
                    return Ok(StreamFlow::Stop);
                }
                if matches!(
                    evaluation.decision,
                    loom_core::PolicyDecision::RequireApproval
                ) {
                    let control_revision = self.next_control_revision()?;
                    self.pending_approval = Some(PendingApproval { call: call.clone() });
                    self.run.control_revision = control_revision;
                    let interaction_id = self.open_interaction(
                        loom_protocol::AgentInteractionKind::ToolApproval,
                        "Tool approval requested".to_owned(),
                        Some(call.id),
                        control_revision,
                    );
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
                        attempt_id: self.run.attempt_id,
                        control_revision,
                        interaction_id,
                        call,
                    });
                    ctx.finished = true;
                    return Ok(StreamFlow::Stop);
                }
                if kind == Some(ToolKind::ProposePlan) {
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
                        self.messages.push(ModelMessage {
                            role: MessageRole::Tool,
                            content: output,
                            name: Some(call.name.clone()),
                            tool_call_id: Some(call.id),
                            tool_calls: Vec::new(),
                        });
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
                    self.messages.push(ModelMessage {
                        role: MessageRole::Tool,
                        content: result.output.clone(),
                        name: Some(result.name.clone()),
                        tool_call_id: Some(result.tool_call_id),
                        tool_calls: Vec::new(),
                    });
                    self.step_id = None;
                    self.step_index = self.step_index.saturating_add(1);
                    ctx.events.push(AgentEvent::StepCompleted {
                        run_id: self.run.id,
                        step_id: ctx.step_id,
                        index: ctx.step_index,
                    });
                    return Ok(StreamFlow::Stop);
                }
                if kind == Some(ToolKind::AskUser) {
                    let Some(prompt) = call
                        .arguments
                        .get("prompt")
                        .and_then(serde_json::Value::as_str)
                        .filter(|prompt| {
                            !prompt.trim().is_empty()
                                && prompt.len() <= MAX_INTERACTION_PROMPT_BYTES
                        })
                    else {
                        let output = format!(
                            "ask_user requires a non-empty prompt of at most {MAX_INTERACTION_PROMPT_BYTES} bytes"
                        );
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
                        self.messages.push(ModelMessage {
                            role: MessageRole::Tool,
                            content: output,
                            name: Some(call.name.clone()),
                            tool_call_id: Some(call.id),
                            tool_calls: Vec::new(),
                        });
                        return Ok(StreamFlow::Stop);
                    };
                    let control_revision = self.next_control_revision()?;
                    let interaction_id = self.open_interaction(
                        loom_protocol::AgentInteractionKind::UserInput,
                        prompt.to_owned(),
                        None,
                        control_revision,
                    );
                    self.pending_input = Some(prompt.to_owned());
                    self.run.control_revision = control_revision;
                    self.messages.push(ModelMessage {
                        role: MessageRole::Tool,
                        content: format!("Waiting for user input: {prompt}"),
                        name: Some(call.name.clone()),
                        tool_call_id: Some(call.id),
                        tool_calls: Vec::new(),
                    });
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
                        attempt_id: self.run.attempt_id,
                        control_revision,
                        interaction_id,
                        prompt: prompt.to_owned(),
                    });
                    ctx.finished = true;
                    return Ok(StreamFlow::Stop);
                }
                self.pending_tool_execution = Some(call.clone());
                self.step_id = None;
                self.step_index = self.step_index.saturating_add(1);
                ctx.events.push(AgentEvent::StepCompleted {
                    run_id: self.run.id,
                    step_id: ctx.step_id,
                    index: ctx.step_index,
                });
                return Ok(StreamFlow::Stop);
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
                    } else if let loom_model::FinishReason::ErrorWithMessage { message } = reason {
                        ctx.events.extend(self.finish_failed(message));
                    } else if matches!(reason, loom_model::FinishReason::Error) {
                        log::error!(
                            "model stream ended with an error finish reason and no provider details (run {})",
                            self.run.id
                        );
                        ctx.events.extend(self.finish_failed(
                            "the model reported an error, but the provider supplied no details",
                        ));
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

    fn open_interaction(
        &mut self,
        kind: loom_protocol::AgentInteractionKind,
        prompt: String,
        tool_call_id: Option<loom_core::ToolCallId>,
        control_revision: u64,
    ) -> InteractionId {
        let id = InteractionId::new();
        self.interactions.push(AgentInteractionRecord {
            id,
            run_id: self.run.id,
            session_id: self.session_id,
            attempt_id: self.run.attempt_id,
            control_revision,
            kind,
            status: loom_protocol::AgentInteractionStatus::Pending,
            tool_call_id,
            prompt,
            decision: None,
            created_at: Timestamp::now(),
            resolved_at: None,
        });
        id
    }

    fn pending_interaction_id(
        &self,
        kind: loom_protocol::AgentInteractionKind,
        attempt_id: loom_core::RunAttemptId,
        control_revision: u64,
        tool_call_id: Option<loom_core::ToolCallId>,
    ) -> Result<InteractionId> {
        self.interactions
            .iter()
            .rev()
            .find(|interaction| {
                interaction.kind == kind
                    && interaction.status == loom_protocol::AgentInteractionStatus::Pending
                    && interaction.attempt_id == attempt_id
                    && interaction.control_revision == control_revision
                    && interaction.tool_call_id == tool_call_id
            })
            .map(|interaction| interaction.id)
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::InvalidState,
                    "pending run interaction is missing from durable history",
                    false,
                )
            })
    }

    fn resolve_interaction(
        &mut self,
        interaction_id: InteractionId,
        status: loom_protocol::AgentInteractionStatus,
        decision: Option<ApprovalDecision>,
    ) -> Result<()> {
        let interaction = self
            .interactions
            .iter_mut()
            .find(|interaction| interaction.id == interaction_id)
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::InvalidState,
                    "run interaction is missing from runtime history",
                    false,
                )
            })?;
        if interaction.status != loom_protocol::AgentInteractionStatus::Pending {
            return Err(LoomError::invalid_state(
                "run interaction has already been resolved",
            ));
        }
        interaction.status = status;
        interaction.decision = decision;
        interaction.resolved_at = Some(Timestamp::now());
        Ok(())
    }

    fn abandon_pending_interactions(&mut self, attempt_id: loom_core::RunAttemptId) {
        let resolved_at = Timestamp::now();
        for interaction in &mut self.interactions {
            if interaction.attempt_id == attempt_id
                && interaction.status == loom_protocol::AgentInteractionStatus::Pending
            {
                interaction.status = loom_protocol::AgentInteractionStatus::Abandoned;
                interaction.resolved_at = Some(resolved_at);
            }
        }
    }

    fn validate_control_revision(
        &self,
        attempt_id: loom_core::RunAttemptId,
        expected_control_revision: u64,
    ) -> Result<()> {
        if self.run.attempt_id != attempt_id
            || self.run.control_revision != expected_control_revision
        {
            return Err(LoomError::new(
                ErrorCode::Conflict,
                "agent interaction belongs to a different attempt or control revision",
                false,
            ));
        }
        Ok(())
    }

    fn next_control_revision(&self) -> Result<u64> {
        self.run.control_revision.checked_add(1).ok_or_else(|| {
            LoomError::new(
                ErrorCode::InvalidState,
                "agent control revision is exhausted",
                false,
            )
        })
    }

    fn model_request(&self) -> Result<(ModelRequest, ContextInspection)> {
        let provider = self.provider()?;
        let initial_count = initial_messages(&self.task).len();
        let source = self.messages.get(initial_count..).unwrap_or_default();
        // Repair legacy transcripts before budgeting, so repair cannot reinsert
        // an oversized output after the assembler has removed it.
        let conversation = repair_tool_transcript(source.to_vec(), source);
        let checkpoint = match self.context_checkpoint.as_ref() {
            Some(summary)
                if summary.projection_version == CONTEXT_PROJECTION_VERSION
                    && summary.source_message_count <= conversation.len()
                    && summary.source_digest
                        == context_projection_digest(
                            &conversation[..summary.source_message_count],
                        )? =>
            {
                Some(summary)
            }
            _ => None,
        };
        let boundary = checkpoint.map_or(0, |summary| summary.source_message_count);
        let mut options = self.options.context.clone();
        // Explicit overrides may lower, but never raise, a known model limit.
        // Unknown models use a conservative bounded fallback instead of an
        // unlimited conversation. Users can supply a known window explicitly.
        options.context_window =
            match (options.context_window, provider.descriptor().context_window) {
                (Some(requested), Some(model)) => Some(requested.min(u64::from(model))),
                (Some(requested), None) => Some(requested),
                (None, model) => Some(model.map_or(8_192, u64::from)),
            };
        options.max_input_tokens = match (
            options.max_input_tokens,
            self.options.limits.max_input_tokens,
        ) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let reserve = loom_context::output_reserve(&options).min(u64::from(u32::MAX));
        options.reserved_output_tokens = Some(reserve);
        let budget = loom_protocol::ContextBudget::new(
            options.context_window,
            options.max_input_tokens,
            reserve,
        )?;
        let tools = if provider.descriptor().capabilities.tool_calling {
            self.tools.definitions()
        } else {
            Vec::new()
        };
        let completion = CompletionOptions {
            max_output_tokens: Some(reserve as u32),
            ..Default::default()
        };
        let tool_tokens = provider.count_tokens(&ModelRequest {
            model: self.task.model.clone(),
            messages: Vec::new(),
            tools: tools.clone(),
            options: completion.clone(),
        });
        let message_limit = budget
            .effective_input_tokens
            .unwrap_or(u64::MAX)
            .saturating_sub(tool_tokens);
        let message_options = ContextAssemblyOptions {
            context_window: None,
            max_input_tokens: Some(message_limit),
            reserved_output_tokens: Some(reserve),
        };
        let assembly = ContextAssembler::assemble_with_counter(
            &ContextInput {
                system_instructions: self.task.system_instructions.clone(),
                repository_instructions: self.task.repository_instructions.clone(),
                task: self.task.task.clone(),
                conversation: conversation[boundary..].to_vec(),
                existing_summary: checkpoint.map(|summary| summary.text.clone()),
                latest_user_message: conversation
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(_, message)| message.role == MessageRole::User)
                    .filter(|(index, _)| *index < boundary)
                    .map(|(_, message)| message.content.clone()),
            },
            &message_options,
            |message| {
                provider.count_tokens(&ModelRequest {
                    model: self.task.model.clone(),
                    messages: vec![message.clone()],
                    tools: Vec::new(),
                    options: completion.clone(),
                })
            },
        )?;
        let request = ModelRequest {
            model: self.task.model.clone(),
            messages: assembly.messages,
            tools,
            options: completion,
        };
        let request_tokens = provider.count_tokens(&request);
        if budget
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
        inspection.budget = budget;
        inspection.included_tokens = request_tokens;
        inspection.total_tokens = inspection
            .total_tokens
            .saturating_add(tool_tokens)
            .max(request_tokens);
        if let Some(summary) = &mut inspection.summary {
            summary.source_message_count += boundary;
            summary.projection_version = CONTEXT_PROJECTION_VERSION;
            summary.source_digest =
                context_projection_digest(&conversation[..summary.source_message_count])?;
        }
        if provider.descriptor().context_window.is_none()
            && self.options.context.context_window.is_none()
        {
            inspection.items.push(loom_protocol::ContextItem {
                kind: loom_protocol::ContextItemKind::SystemInstructions,
                label: "Model context window unknown; using an 8192-token fallback".to_owned(),
                estimated_tokens: 0,
                included: true,
                omission_reason: None,
            });
        }
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
        if let Some(last) = self.messages.last_mut()
            && last.role == MessageRole::Assistant
        {
            last.content.push_str(text);
            return;
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
        if matches!(
            state,
            AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
        ) {
            self.abandon_pending_interactions(self.run.attempt_id);
            self.pending_approval = None;
            self.pending_tool_execution = None;
            self.pending_input = None;
        }
        if self.run.state == state {
            return Vec::new();
        }
        self.run.state = state;
        let updated_at = Timestamp::now();
        self.run.updated_at = updated_at;
        let attempt = self
            .attempts
            .iter_mut()
            .rfind(|attempt| attempt.id == self.run.attempt_id)
            .expect("the current run attempt must be recorded");
        attempt.state = state;
        attempt.completed_at = if matches!(
            state,
            AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
        ) {
            Some(updated_at)
        } else {
            None
        };
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

/// Keeps Responses API function calls paired with their tool outputs after
/// context assembly or recovery from an older persisted runtime state.
fn context_projection_digest(messages: &[ModelMessage]) -> Result<String> {
    let projection = serde_json::to_vec(messages).map_err(|error| {
        LoomError::new(
            ErrorCode::Internal,
            format!("failed to encode context projection: {error}"),
            false,
        )
    })?;
    let digest = Sha256::digest(projection);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(encoded)
}

fn repair_tool_transcript(
    messages: Vec<ModelMessage>,
    source_messages: &[ModelMessage],
) -> Vec<ModelMessage> {
    let source_outputs: BTreeMap<_, _> = source_messages
        .iter()
        .filter_map(|message| {
            (message.role == MessageRole::Tool)
                .then_some(message.tool_call_id)
                .flatten()
                .map(|id| (id, message.clone()))
        })
        .collect();
    let call_ids: BTreeSet<_> = messages
        .iter()
        .flat_map(|message| message.tool_calls.iter().map(|call| call.id))
        .collect();
    let output_ids: BTreeSet<_> = messages
        .iter()
        .filter_map(|message| {
            (message.role == MessageRole::Tool)
                .then_some(message.tool_call_id)
                .flatten()
        })
        .collect();
    let mut repaired = Vec::with_capacity(messages.len());
    for message in messages {
        if message.role == MessageRole::Tool {
            if message
                .tool_call_id
                .is_some_and(|tool_call_id| call_ids.contains(&tool_call_id))
            {
                repaired.push(message);
            }
            continue;
        }
        let tool_calls = message.tool_calls.clone();
        repaired.push(message);
        for call in tool_calls {
            if output_ids.contains(&call.id) {
                continue;
            }
            repaired.push(
                source_outputs
                    .get(&call.id)
                    .cloned()
                    .unwrap_or_else(|| ModelMessage {
                        role: MessageRole::Tool,
                        content: "No tool output was recorded; continue from the current state."
                            .to_owned(),
                        name: Some(call.name.clone()),
                        tool_call_id: Some(call.id),
                        tool_calls: Vec::new(),
                    }),
            );
        }
    }
    repaired
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
}
