use std::{fs, path::PathBuf};

use gpui::{
    App, Application, Bounds, ClickEvent, Context, Render, TitlebarOptions, Window,
    WindowBackgroundAppearance, WindowBounds, WindowDecorations, WindowOptions, div, prelude::*,
    px, rgb, size,
};
use loom_agent::{AgentEvent, AgentRunSnapshot, AgentRunState};
use loom_core::{
    AgentSessionId, AgentSessionState, Capability, CapabilitySet, ErrorCode, EventSequence,
    LoomError, ProjectId, RunId,
};
use loom_language::{Diagnostic, LanguageServiceDescriptor, Symbol};
use loom_model::{ModelId, ToolCall};
use loom_process::{TaskKind, TaskSnapshot, TaskSpec, TaskStatus};
use loom_protocol::{
    CURRENT_PROTOCOL_VERSION, ClientRequest, RequestEnvelope, ServerEvent, ServerResponse,
};
use loom_server::{InProcessBackend, InProcessConnection};
use loom_vcs::{GitRepositoryStatus, GitService};
use loom_workspace::{
    BufferId, BufferSnapshot, EditorLayoutSnapshot, FileTreeEntry, SearchMatch, SearchQuery,
};

struct LoomView {
    connection: InProcessConnection,
    project_id: ProjectId,
    session_id: AgentSessionId,
    run_id: RunId,
    workspace_root: PathBuf,
    model: ModelId,
    task: String,
    after_sequence: Option<EventSequence>,
    session_state: AgentSessionState,
    run_state: AgentRunState,
    pending_approval: Option<ToolCall>,
    timeline: Vec<TimelineItem>,
    summary: Option<String>,
    workspace_entries: usize,
    provider_count: usize,
    file_tree: Vec<FileTreeEntry>,
    open_buffers: Vec<BufferSnapshot>,
    active_buffer: Option<BufferId>,
    editor_layout: Option<EditorLayoutSnapshot>,
    diagnostics: Vec<Diagnostic>,
    symbols: Vec<Symbol>,
    search_matches: Vec<SearchMatch>,
    language_services: Vec<LanguageServiceDescriptor>,
    vcs_status: Option<GitRepositoryStatus>,
    vcs_error: Option<String>,
    task_results: Vec<TaskSnapshot>,
}

#[derive(Clone)]
enum TimelineItem {
    Plan(Vec<String>),
    Assistant(String),
    ToolRequested { name: String, arguments: String },
    Approval { name: String, active: bool },
    ToolStarted(String),
    ToolOutput(String),
    ToolCompleted { name: String, success: bool },
    Status(String),
    Summary { text: String, evidence: Vec<String> },
}

impl LoomView {
    fn try_new() -> Result<Self, LoomError> {
        let workspace_root = prepare_workspace()?;
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate(&connection)?;
        let provider_count = provider_count(&connection)?;
        let model = ModelId::new("deterministic/demo");
        let (session, run) = start_demo_run(&connection, &workspace_root, &model)?;
        let workspace_entries = workspace_snapshot(&connection, session.project_id)?
            .entries
            .len();
        let mut view = Self {
            connection,
            project_id: session.project_id,
            session_id: session.id,
            run_id: run.id,
            workspace_root,
            model,
            task: run.task.clone(),
            after_sequence: None,
            session_state: session.state,
            run_state: run.state,
            pending_approval: None,
            timeline: Vec::new(),
            summary: None,
            workspace_entries,
            provider_count,
            file_tree: Vec::new(),
            open_buffers: Vec::new(),
            active_buffer: None,
            editor_layout: None,
            diagnostics: Vec::new(),
            symbols: Vec::new(),
            search_matches: Vec::new(),
            language_services: Vec::new(),
            vcs_status: None,
            vcs_error: None,
            task_results: Vec::new(),
        };
        view.refresh_workspace_surfaces()?;
        view.collect_events()?;
        Ok(view)
    }

    fn refresh_workspace_surfaces(&mut self) -> Result<(), LoomError> {
        let file_tree = self
            .connection
            .request(RequestEnvelope::new(ClientRequest::GetFileTree {
                project_id: self.project_id,
            }));
        if let Ok(ServerResponse::FileTree { entries }) = file_tree.result {
            self.file_tree = entries;
        }
        let opened =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::OpenEditorBuffer {
                    project_id: self.project_id,
                    path: "README.md".to_owned(),
                }));
        if let Ok(ServerResponse::EditorBuffer(buffer)) = opened.result {
            self.active_buffer = Some(buffer.id);
            self.open_buffers = vec![buffer.clone()];
            self.diagnostics.clear();
            self.symbols.clear();
        }
        let layout =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::GetEditorLayout {
                    project_id: self.project_id,
                }));
        if let Ok(ServerResponse::EditorLayout(layout)) = layout.result {
            self.editor_layout = Some(layout);
        }
        let services = self.connection.request(RequestEnvelope::new(
            ClientRequest::DiscoverLanguageServices {
                project_id: self.project_id,
            },
        ));
        if let Ok(ServerResponse::LanguageServices { services }) = services.result {
            self.language_services = services;
        }
        let started =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::StartLanguageService {
                    project_id: self.project_id,
                    path: "README.md".to_owned(),
                }));
        if let Ok(ServerResponse::LanguageServices { services }) = started.result {
            self.language_services = services;
        }
        let diagnostics =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::GetDiagnostics {
                    project_id: self.project_id,
                    path: "README.md".to_owned(),
                }));
        if let Ok(ServerResponse::Diagnostics {
            diagnostics,
            path: _,
        }) = diagnostics.result
        {
            self.diagnostics = diagnostics;
        }
        let symbols = self
            .connection
            .request(RequestEnvelope::new(ClientRequest::GetSymbols {
                project_id: self.project_id,
                path: "README.md".to_owned(),
            }));
        if let Ok(ServerResponse::Symbols { symbols, path: _ }) = symbols.result {
            self.symbols = symbols;
        }
        let search =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::SearchWorkspace {
                    project_id: self.project_id,
                    query: SearchQuery::literal("TODO"),
                }));
        if let Ok(ServerResponse::SearchMatches { matches }) = search.result {
            self.search_matches = matches;
        }
        let status = self
            .connection
            .request(RequestEnvelope::new(ClientRequest::GetVcsStatus {
                project_id: self.project_id,
            }));
        match status.result {
            Ok(ServerResponse::VcsStatus(status)) => {
                self.vcs_status = Some(status);
                self.vcs_error = None;
            }
            Err(error) => self.vcs_error = Some(error.message),
            Ok(_) => {}
        }
        self.refresh_task_results();
        Ok(())
    }

    fn refresh_task_results(&mut self) {
        let ids = self
            .task_results
            .iter()
            .map(|task| task.id)
            .collect::<Vec<_>>();
        self.task_results = ids
            .into_iter()
            .filter_map(|task_id| {
                let response =
                    self.connection
                        .request(RequestEnvelope::new(ClientRequest::GetTask {
                            project_id: self.project_id,
                            task_id,
                        }));
                match response.result {
                    Ok(ServerResponse::Task(task)) => Some(task),
                    _ => None,
                }
            })
            .collect();
    }

    fn collect_events(&mut self) -> Result<(), LoomError> {
        let response =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
                    session_id: Some(self.session_id),
                    after_sequence: self.after_sequence,
                }));
        let events = match response.result? {
            ServerResponse::SessionEvents { events } => events,
            response => return Err(unexpected_response("event stream", response)),
        };
        for event in events {
            self.after_sequence = Some(event.sequence);
            self.consume_event(&event.event);
        }
        Ok(())
    }

    fn consume_event(&mut self, event: &ServerEvent) {
        match event {
            ServerEvent::AgentSessionCreated { .. } => {}
            ServerEvent::AgentSessionStateChanged { current, .. } => {
                self.session_state = *current;
            }
            ServerEvent::AgentSessionForked { .. } => {}
            ServerEvent::Agent { event } => match event {
                AgentEvent::RunStarted { snapshot } => {
                    self.run_state = snapshot.state;
                    self.timeline
                        .push(TimelineItem::Status("Agent run started".to_owned()));
                }
                AgentEvent::PlanProposed { plan, .. } => self.timeline.push(TimelineItem::Plan(
                    plan.steps
                        .iter()
                        .map(|step| step.description.clone())
                        .collect(),
                )),
                AgentEvent::StepStarted { index, .. } => self
                    .timeline
                    .push(TimelineItem::Status(format!("Step {} started", index + 1))),
                AgentEvent::StepCompleted { index, .. } => self.timeline.push(
                    TimelineItem::Status(format!("Step {} completed", index + 1)),
                ),
                AgentEvent::ContextInspected { inspection, .. } => {
                    self.timeline.push(TimelineItem::Status(format!(
                        "Context: {} input tokens ({} omitted)",
                        inspection.included_tokens, inspection.omitted_tokens
                    )))
                }
                AgentEvent::ProviderError { error, .. } => self
                    .timeline
                    .push(TimelineItem::Status(format!("Provider error: {error}"))),
                AgentEvent::ContextError { error, .. } => self
                    .timeline
                    .push(TimelineItem::Status(format!("Context error: {error}"))),
                AgentEvent::AssistantMessageDelta { text, .. } => {
                    if let Some(TimelineItem::Assistant(message)) = self.timeline.last_mut() {
                        message.push_str(text);
                    } else {
                        self.timeline.push(TimelineItem::Assistant(text.clone()));
                    }
                }
                AgentEvent::ToolCallRequested { call, .. } => {
                    self.timeline.push(TimelineItem::ToolRequested {
                        name: call.name.clone(),
                        arguments: serde_json::to_string(&call.arguments)
                            .unwrap_or_else(|_| "{}".to_owned()),
                    });
                }
                AgentEvent::ToolApprovalRequired { call, .. } => {
                    for item in &mut self.timeline {
                        if let TimelineItem::Approval { active, .. } = item {
                            *active = false;
                        }
                    }
                    self.pending_approval = Some(call.clone());
                    self.timeline.push(TimelineItem::Approval {
                        name: call.name.clone(),
                        active: true,
                    });
                }
                AgentEvent::ToolPolicyEvaluated { evaluation, .. } => {
                    self.timeline.push(TimelineItem::Status(format!(
                        "Policy {:?}: {}",
                        evaluation.decision, evaluation.reason
                    )));
                }
                AgentEvent::ToolApprovalDecided { decision, .. } => {
                    for item in &mut self.timeline {
                        if let TimelineItem::Approval { active, .. } = item {
                            *active = false;
                        }
                    }
                    self.timeline
                        .push(TimelineItem::Status(format!("Approval: {decision:?}")));
                }
                AgentEvent::ToolCallStarted { call, .. } => {
                    self.timeline
                        .push(TimelineItem::ToolStarted(call.name.clone()));
                }
                AgentEvent::ToolOutputChunk { chunk, .. } => {
                    self.timeline.push(TimelineItem::ToolOutput(chunk.clone()));
                }
                AgentEvent::ToolCallCompleted { result, .. } => {
                    self.timeline.push(TimelineItem::ToolCompleted {
                        name: result.name.clone(),
                        success: result.success,
                    });
                }
                AgentEvent::RunUsage { usage, .. } => {
                    self.timeline.push(TimelineItem::Status(format!(
                        "Usage: {} input / {} output tokens",
                        usage.input_tokens, usage.output_tokens
                    )))
                }
                AgentEvent::RunUsageUpdated { usage, .. } => {
                    self.timeline.push(TimelineItem::Status(format!(
                        "Total usage: {} input / {} output / {} tool calls",
                        usage.input_tokens, usage.output_tokens, usage.tool_calls
                    )))
                }
                AgentEvent::RunLimitReached { status, .. } => self.timeline.push(
                    TimelineItem::Status(format!("Limit reached: {:?}", status.exceeded)),
                ),
                AgentEvent::RecoveryRequired { reason, .. } => self
                    .timeline
                    .push(TimelineItem::Status(format!("Recovery required: {reason}"))),
                AgentEvent::RunStateChanged { state, .. } => {
                    self.run_state = *state;
                    self.session_state = session_state_for_run(*state);
                }
                AgentEvent::RunCompleted { snapshot } => {
                    self.run_state = snapshot.state;
                    self.session_state = session_state_for_run(snapshot.state);
                    self.summary = snapshot.summary.clone();
                    if let Some(summary) = &snapshot.summary {
                        self.timeline.push(TimelineItem::Summary {
                            text: summary.clone(),
                            evidence: snapshot
                                .evidence
                                .iter()
                                .map(|link| format!("{} ({})", link.label, link.uri))
                                .collect(),
                        });
                    }
                }
            },
            ServerEvent::WorkspaceChanged { change } => {
                self.timeline.push(TimelineItem::Status(format!(
                    "Workspace {:?}: {}",
                    change.kind, change.path
                )));
            }
            ServerEvent::Terminal { event } => {
                self.timeline
                    .push(TimelineItem::Status(format!("Terminal: {:?}", event.event)));
            }
            ServerEvent::Task { event } => {
                self.timeline
                    .push(TimelineItem::Status(format!("Task: {:?}", event.event)));
            }
            ServerEvent::ProviderHealthChanged {
                provider_id,
                health,
            } => {
                self.timeline.push(TimelineItem::Status(format!(
                    "Provider {} health: {:?}",
                    provider_id.as_str(),
                    health.state
                )));
            }
        }
    }

    fn approve(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(call) = self.pending_approval.take() else {
            return;
        };
        let response =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::ApproveAgentAction {
                    run_id: self.run_id,
                    tool_call_id: call.id,
                }));
        if let Err(error) = response.result {
            self.timeline
                .push(TimelineItem::Status(format!("Approval error: {error}")));
            self.pending_approval = Some(call);
            cx.notify();
            return;
        }
        if let Err(error) = self.collect_events() {
            self.timeline
                .push(TimelineItem::Status(format!("Event error: {error}")));
        }
        cx.notify();
    }

    fn refresh(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Err(error) = self.collect_events() {
            self.timeline
                .push(TimelineItem::Status(format!("Refresh error: {error}")));
        }
        match workspace_snapshot(&self.connection, self.project_id) {
            Ok(snapshot) => self.workspace_entries = snapshot.entries.len(),
            Err(error) => self.timeline.push(TimelineItem::Status(format!(
                "Workspace refresh error: {error}"
            ))),
        }
        if let Err(error) = self.refresh_workspace_surfaces() {
            self.timeline.push(TimelineItem::Status(format!(
                "Coding workspace refresh error: {error}"
            )));
        }
        cx.notify();
    }

    fn open_readme(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.open_file_path("README.md".to_owned());
        cx.notify();
    }

    fn open_file_path(&mut self, path: String) {
        let response =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::OpenEditorBuffer {
                    project_id: self.project_id,
                    path,
                }));
        match response.result {
            Ok(ServerResponse::EditorBuffer(buffer)) => {
                self.active_buffer = Some(buffer.id);
                if let Some(existing) = self
                    .open_buffers
                    .iter_mut()
                    .find(|existing| existing.id == buffer.id)
                {
                    *existing = buffer;
                } else {
                    self.open_buffers.push(buffer);
                }
            }
            Err(error) => self
                .timeline
                .push(TimelineItem::Status(format!("Open file error: {error}"))),
            Ok(response) => self.timeline.push(TimelineItem::Status(format!(
                "Open file returned unexpected response: {response:?}"
            ))),
        }
    }

    fn save_active(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(buffer_id) = self.active_buffer else {
            return;
        };
        let response =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::SaveEditorBuffer {
                    project_id: self.project_id,
                    buffer_id,
                }));
        match response.result {
            Ok(ServerResponse::EditorBuffer(buffer)) => self.replace_buffer(buffer),
            Err(error) => self
                .timeline
                .push(TimelineItem::Status(format!("Save error: {error}"))),
            Ok(_) => {}
        }
        cx.notify();
    }

    fn undo_active(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(buffer_id) = self.active_buffer else {
            return;
        };
        let response =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::UndoEditorBuffer {
                    project_id: self.project_id,
                    buffer_id,
                }));
        match response.result {
            Ok(ServerResponse::EditorBuffer(buffer)) => self.replace_buffer(buffer),
            Err(error) => self
                .timeline
                .push(TimelineItem::Status(format!("Undo error: {error}"))),
            Ok(_) => {}
        }
        cx.notify();
    }

    fn redo_active(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(buffer_id) = self.active_buffer else {
            return;
        };
        let response =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::RedoEditorBuffer {
                    project_id: self.project_id,
                    buffer_id,
                }));
        match response.result {
            Ok(ServerResponse::EditorBuffer(buffer)) => self.replace_buffer(buffer),
            Err(error) => self
                .timeline
                .push(TimelineItem::Status(format!("Redo error: {error}"))),
            Ok(_) => {}
        }
        cx.notify();
    }

    fn run_task(&mut self, kind: TaskKind, cx: &mut Context<Self>) {
        let label = format!("{kind:?} workspace task");
        let response = self
            .connection
            .request(RequestEnvelope::new(ClientRequest::StartTask {
                project_id: self.project_id,
                spec: TaskSpec {
                    kind,
                    label,
                    command: "cargo".to_owned(),
                    args: vec![format!("{kind:?}").to_ascii_lowercase()],
                    cwd: None,
                    output_limit_bytes: Some(16 * 1024),
                    artifact_paths: Vec::new(),
                },
            }));
        match response.result {
            Ok(ServerResponse::TaskStarted(task)) => self.task_results.push(task),
            Err(error) => self
                .timeline
                .push(TimelineItem::Status(format!("Task error: {error}"))),
            Ok(_) => {}
        }
        cx.notify();
    }

    fn run_build(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.run_task(TaskKind::Build, cx);
    }

    fn run_test(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.run_task(TaskKind::Test, cx);
    }

    fn run_lint(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.run_task(TaskKind::Lint, cx);
    }

    fn replace_buffer(&mut self, buffer: BufferSnapshot) {
        self.active_buffer = Some(buffer.id);
        if let Some(existing) = self
            .open_buffers
            .iter_mut()
            .find(|existing| existing.id == buffer.id)
        {
            *existing = buffer;
        } else {
            self.open_buffers.push(buffer);
        }
    }

    fn restart(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Err(error) = self.restart_demo() {
            self.timeline
                .push(TimelineItem::Status(format!("New run error: {error}")));
        }
        cx.notify();
    }

    fn close(&mut self, _: &ClickEvent, window: &mut Window, _: &mut Context<Self>) {
        window.remove_window();
    }

    fn restart_demo(&mut self) -> Result<(), LoomError> {
        if matches!(
            self.run_state,
            AgentRunState::Planning
                | AgentRunState::Executing
                | AgentRunState::AwaitingApproval
                | AgentRunState::Paused
                | AgentRunState::Evaluating
        ) {
            self.connection
                .request(RequestEnvelope::new(ClientRequest::InterruptAgentRun {
                    run_id: self.run_id,
                }))
                .result?;
        }
        reset_demo_workspace(&self.workspace_root)?;
        let (session, run) = start_demo_run(&self.connection, &self.workspace_root, &self.model)?;
        self.project_id = session.project_id;
        self.session_id = session.id;
        self.run_id = run.id;
        self.task = run.task;
        self.after_sequence = None;
        self.session_state = session.state;
        self.run_state = run.state;
        self.pending_approval = None;
        self.timeline.clear();
        self.summary = None;
        self.workspace_entries = workspace_snapshot(&self.connection, self.project_id)?
            .entries
            .len();
        self.collect_events()
    }

    fn reject(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(call) = self.pending_approval.take() else {
            return;
        };
        let response =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::RejectAgentAction {
                    run_id: self.run_id,
                    tool_call_id: call.id,
                    reason: Some("Denied in the GPUI shell".to_owned()),
                }));
        if let Err(error) = response.result {
            self.timeline
                .push(TimelineItem::Status(format!("Rejection error: {error}")));
            self.pending_approval = Some(call);
            cx.notify();
            return;
        }
        if let Err(error) = self.collect_events() {
            self.timeline
                .push(TimelineItem::Status(format!("Event error: {error}")));
        }
        cx.notify();
    }

    fn interrupt(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let response =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::InterruptAgentRun {
                    run_id: self.run_id,
                }));
        if let Err(error) = response.result {
            self.timeline
                .push(TimelineItem::Status(format!("Interrupt error: {error}")));
        } else if let Err(error) = self.collect_events() {
            self.timeline
                .push(TimelineItem::Status(format!("Event error: {error}")));
        }
        cx.notify();
    }

    fn pause(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let response =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::PauseAgentRun {
                    run_id: self.run_id,
                }));
        if let Err(error) = response.result {
            self.timeline
                .push(TimelineItem::Status(format!("Pause error: {error}")));
        } else if let Err(error) = self.collect_events() {
            self.timeline
                .push(TimelineItem::Status(format!("Event error: {error}")));
        }
        cx.notify();
    }

    fn resume(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let response =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::ResumeAgentRun {
                    run_id: self.run_id,
                }));
        if let Err(error) = response.result {
            self.timeline
                .push(TimelineItem::Status(format!("Resume error: {error}")));
        } else if let Err(error) = self.collect_events() {
            self.timeline
                .push(TimelineItem::Status(format!("Event error: {error}")));
        }
        cx.notify();
    }

    fn retry(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let response =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::RetryAgentStep {
                    run_id: self.run_id,
                }));
        if let Err(error) = response.result {
            self.timeline
                .push(TimelineItem::Status(format!("Retry error: {error}")));
        } else if let Err(error) = self.collect_events() {
            self.timeline
                .push(TimelineItem::Status(format!("Event error: {error}")));
        }
        cx.notify();
    }

    fn render_timeline(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut timeline = div().flex().flex_col().gap_1().p_3().text_size(px(13.));
        for item in &self.timeline {
            timeline = timeline.child(self.render_timeline_item(item, cx));
        }
        timeline
    }

    fn render_timeline_item(
        &self,
        item: &TimelineItem,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        match item {
            TimelineItem::Plan(steps) => {
                let mut card = div()
                    .p_2()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(0x30343f))
                    .bg(rgb(0x20242c))
                    .text_color(rgb(0xdbeafe))
                    .child("Plan");
                for (index, step) in steps.iter().enumerate() {
                    card = card.child(div().text_sm().text_color(rgb(0xb7c0d0)).child(format!(
                        "{}. {}",
                        index + 1,
                        step
                    )));
                }
                card.into_any()
            }
            TimelineItem::Assistant(message) => div()
                .px_2()
                .py_2()
                .border_b_1()
                .border_color(rgb(0x2a2d34))
                .text_color(rgb(0xf3f4f6))
                .child(div().text_sm().text_color(rgb(0x9da7b5)).child("Assistant"))
                .child(message.clone())
                .into_any(),
            TimelineItem::ToolRequested { name, arguments } => div()
                .px_2()
                .py_2()
                .rounded_sm()
                .bg(rgb(0x1d2026))
                .text_color(rgb(0xd1d5db))
                .child(format!("Tool requested  {name}"))
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(0x8f98a6))
                        .child(arguments.clone()),
                )
                .into_any(),
            TimelineItem::Approval { name, active } => {
                let mut card = div()
                    .p_2()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(0xf59e0b))
                    .bg(rgb(0x2a2415))
                    .text_color(rgb(0xfef3c7))
                    .child(format!("Approval required  {name}"));
                if *active {
                    card = card.child(
                        div()
                            .flex()
                            .gap_1()
                            .mt_2()
                            .child(
                                div()
                                    .id("deny-action")
                                    .px_2()
                                    .py_1()
                                    .rounded_sm()
                                    .bg(rgb(0x4b2020))
                                    .text_color(rgb(0xfca5a5))
                                    .text_sm()
                                    .cursor_pointer()
                                    .child("Deny")
                                    .on_click(cx.listener(Self::reject)),
                            )
                            .child(
                                div()
                                    .id("approve-action")
                                    .px_2()
                                    .py_1()
                                    .rounded_sm()
                                    .bg(rgb(0x14532d))
                                    .text_color(rgb(0xbbf7d0))
                                    .text_sm()
                                    .cursor_pointer()
                                    .child("Approve")
                                    .on_click(cx.listener(Self::approve)),
                            ),
                    );
                }
                card.into_any()
            }
            TimelineItem::ToolStarted(name) => div()
                .px_2()
                .py_1()
                .rounded_sm()
                .bg(rgb(0x172554))
                .text_color(rgb(0xbfdbfe))
                .child(format!("Tool running  {name}"))
                .into_any(),
            TimelineItem::ToolOutput(output) => div()
                .px_2()
                .py_1()
                .rounded_sm()
                .bg(rgb(0x111827))
                .text_color(rgb(0xcbd5e1))
                .child(div().text_sm().text_color(rgb(0x8f98a6)).child("Output"))
                .child(output.clone())
                .into_any(),
            TimelineItem::ToolCompleted { name, success } => div()
                .px_2()
                .py_1()
                .rounded_sm()
                .bg(if *success {
                    rgb(0x14532d)
                } else {
                    rgb(0x4b2020)
                })
                .text_color(rgb(0xf3f4f6))
                .child(format!(
                    "Tool completed  {name} [{}]",
                    if *success { "ok" } else { "failed" }
                ))
                .into_any(),
            TimelineItem::Status(status) => div()
                .px_2()
                .py_1()
                .text_sm()
                .text_color(rgb(0x94a3b8))
                .child(status.clone())
                .into_any(),
            TimelineItem::Summary { text, evidence } => div()
                .p_2()
                .rounded_sm()
                .bg(rgb(0x064e3b))
                .text_color(rgb(0xd1fae5))
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(0x9ad7bd))
                        .child("Final summary"),
                )
                .child(text.clone())
                .children(evidence.iter().map(|link| {
                    div()
                        .text_sm()
                        .text_color(rgb(0x9ad7bd))
                        .child(format!("Evidence: {link}"))
                }))
                .into_any(),
        }
    }

    fn render_file_tree(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut tree = div().flex().flex_col().gap_1();
        for (index, entry) in self.file_tree.iter().take(160).enumerate() {
            let indent = "  ".repeat(entry.depth as usize);
            let marker = match entry.kind {
                loom_workspace::FileTreeEntryKind::File => "-",
                loom_workspace::FileTreeEntryKind::Directory => "v",
            };
            let row = div()
                .px_1()
                .py_1()
                .text_sm()
                .text_color(if entry.kind == loom_workspace::FileTreeEntryKind::File {
                    rgb(0xb7c0d0)
                } else {
                    rgb(0x8f98a6)
                })
                .child(format!("{indent}{marker} {}", entry.path));
            if entry.kind == loom_workspace::FileTreeEntryKind::File {
                let path = entry.path.clone();
                tree = tree.child(
                    row.id(("file-tree", index))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_file_path(path.clone());
                            cx.notify();
                        }))
                        .into_any(),
                );
            } else {
                tree = tree.child(row);
            }
        }
        tree
    }

    fn render_editor(&self) -> impl IntoElement {
        let panel = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x111318))
            .text_size(px(13.));
        let mut tabs = div()
            .w_full()
            .h(px(34.))
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .bg(rgb(0x1b1d24))
            .border_b_1()
            .border_color(rgb(0x30343f));
        for buffer in &self.open_buffers {
            let active = self.active_buffer == Some(buffer.id);
            tabs = tabs.child(
                div()
                    .px_2()
                    .py_1()
                    .bg(if active { rgb(0x293244) } else { rgb(0x20242c) })
                    .text_color(if active { rgb(0xf3f4f6) } else { rgb(0x8f98a6) })
                    .child(format!(
                        "{}{}",
                        if buffer.dirty { "● " } else { "" },
                        buffer.path
                    )),
            );
        }
        if self.open_buffers.is_empty() {
            tabs = tabs.child(
                div()
                    .text_sm()
                    .text_color(rgb(0x8f98a6))
                    .child("No file open — select a file from the tree"),
            );
        }
        let mut code = div()
            .flex_1()
            .id("editor-code")
            .overflow_y_scroll()
            .p_3()
            .bg(rgb(0x0f1115))
            .text_color(rgb(0xd1d5db))
            .text_size(px(13.));
        if let Some(buffer) = self
            .open_buffers
            .iter()
            .find(|buffer| Some(buffer.id) == self.active_buffer)
        {
            for (index, line) in buffer.text.lines().enumerate().take(240) {
                let has_marker = buffer.agent_markers.iter().any(|marker| {
                    marker.start_line <= index as u32 + 1 && marker.end_line > index as u32
                });
                code = code.child(
                    div()
                        .text_sm()
                        .text_color(if has_marker {
                            rgb(0xfbbf24)
                        } else {
                            rgb(0xd1d5db)
                        })
                        .child(format!("{:>4}  {}", index + 1, line)),
                );
            }
            if buffer.text.is_empty() {
                code = code.child(" ");
            }
            if buffer.external_change {
                code = code.child(
                    div()
                        .mt_2()
                        .p_2()
                        .bg(rgb(0x4b2020))
                        .text_color(rgb(0xfca5a5))
                        .child("External change detected — reload or resolve before saving"),
                );
            }
        } else {
            code = code.child("Open a file to begin editing");
        }
        panel.child(tabs).child(code)
    }

    fn render_diagnostics(&self) -> impl IntoElement {
        let mut panel = div().flex().flex_col().gap_1().p_2();
        panel = panel.child(
            div()
                .text_sm()
                .text_color(rgb(0x8f98a6))
                .child(format!("DIAGNOSTICS ({})", self.diagnostics.len())),
        );
        for diagnostic in self.diagnostics.iter().take(8) {
            panel = panel.child(
                div()
                    .text_sm()
                    .text_color(
                        if matches!(
                            diagnostic.severity,
                            loom_language::DiagnosticSeverity::Error
                        ) {
                            rgb(0xfca5a5)
                        } else {
                            rgb(0xfef3c7)
                        },
                    )
                    .child(format!(
                        "{}:{} {}",
                        diagnostic.range.start.line + 1,
                        diagnostic.range.start.character + 1,
                        diagnostic.message
                    )),
            );
        }
        if !self.symbols.is_empty() {
            panel = panel.child(
                div()
                    .mt_1()
                    .text_sm()
                    .text_color(rgb(0x8f98a6))
                    .child(format!("OUTLINE ({})", self.symbols.len())),
            );
            for symbol in self.symbols.iter().take(8) {
                panel = panel.child(div().text_sm().text_color(rgb(0xb7c0d0)).child(format!(
                    "{}  {}",
                    symbol.location.range.start.line + 1,
                    symbol.name
                )));
            }
        }
        panel
    }

    fn render_task_results(&self) -> impl IntoElement {
        let mut panel = div().flex().flex_col().gap_1().p_2();
        panel = panel.child(
            div()
                .text_sm()
                .text_color(rgb(0x8f98a6))
                .child("TASK RESULTS"),
        );
        for task in self.task_results.iter().rev().take(4) {
            panel = panel.child(
                div()
                    .text_sm()
                    .text_color(if task.status == TaskStatus::Completed {
                        rgb(0x9ad7bd)
                    } else if matches!(task.status, TaskStatus::Failed | TaskStatus::Cancelled) {
                        rgb(0xfca5a5)
                    } else {
                        rgb(0xfef3c7)
                    })
                    .child(format!(
                        "{}  {:?}  {} evidence",
                        task.label,
                        task.status,
                        task.evidence.len()
                    )),
            );
            for evidence in task.evidence.iter().take(2) {
                panel = panel.child(
                    div()
                        .text_sm()
                        .text_color(rgb(0x8f98a6))
                        .child(format!("-> {} ({})", evidence.label, evidence.uri)),
                );
            }
        }
        panel
    }

    fn render_vcs_status(&self) -> impl IntoElement {
        let text = if let Some(status) = &self.vcs_status {
            format!(
                "Git  {}  |  {} changed  |  {} conflicts",
                status.branch.as_deref().unwrap_or("detached"),
                status.files.len(),
                status.conflicts.len()
            )
        } else {
            format!(
                "Git unavailable{}",
                self.vcs_error
                    .as_deref()
                    .map_or(String::new(), |error| format!(": {error}"))
            )
        };
        div()
            .text_sm()
            .text_color(
                if self
                    .vcs_status
                    .as_ref()
                    .is_some_and(|status| !status.conflicts.is_empty())
                {
                    rgb(0xfca5a5)
                } else {
                    rgb(0x8f98a6)
                },
            )
            .child(text)
    }
}

impl Render for LoomView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let approval_visible = self.pending_approval.is_some();
        let run_active = matches!(
            self.run_state,
            AgentRunState::Planning
                | AgentRunState::Executing
                | AgentRunState::AwaitingApproval
                | AgentRunState::Paused
                | AgentRunState::Evaluating
        );
        let run_failed = self.run_state == AgentRunState::Failed;
        let mut header_actions = div().flex().items_center().gap_1();
        if run_active {
            header_actions = header_actions.child(
                div()
                    .id("interrupt-run")
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(rgb(0x4b2020))
                    .text_color(rgb(0xfca5a5))
                    .text_sm()
                    .cursor_pointer()
                    .child("Interrupt")
                    .on_click(cx.listener(Self::interrupt)),
            );
            if self.run_state != AgentRunState::Paused {
                header_actions = header_actions.child(
                    div()
                        .id("pause-run")
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .bg(rgb(0x493b1a))
                        .text_color(rgb(0xfef3c7))
                        .text_sm()
                        .cursor_pointer()
                        .child("Pause")
                        .on_click(cx.listener(Self::pause)),
                );
            }
        }
        if self.run_state == AgentRunState::Paused {
            header_actions = header_actions.child(
                div()
                    .id("resume-run")
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(rgb(0x14532d))
                    .text_color(rgb(0xbbf7d0))
                    .text_sm()
                    .cursor_pointer()
                    .child("Resume")
                    .on_click(cx.listener(Self::resume)),
            );
        }
        if run_failed {
            header_actions = header_actions.child(
                div()
                    .id("retry-run")
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(rgb(0x78350f))
                    .text_color(rgb(0xfef3c7))
                    .text_sm()
                    .cursor_pointer()
                    .child("Retry step")
                    .on_click(cx.listener(Self::retry)),
            );
        }
        header_actions = header_actions
            .child(
                div()
                    .id("refresh-events")
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(rgb(0x293244))
                    .text_color(rgb(0xdbeafe))
                    .text_sm()
                    .cursor_pointer()
                    .child("Refresh")
                    .on_click(cx.listener(Self::refresh)),
            )
            .child(
                div()
                    .id("new-run")
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(rgb(0x293244))
                    .text_color(rgb(0xdbeafe))
                    .text_sm()
                    .cursor_pointer()
                    .child("New run")
                    .on_click(cx.listener(Self::restart)),
            )
            .child(
                div()
                    .id("close-window")
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(rgb(0x4b2020))
                    .text_color(rgb(0xfca5a5))
                    .text_sm()
                    .cursor_pointer()
                    .child("Close")
                    .on_click(cx.listener(Self::close)),
            );
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x111318))
            .text_color(rgb(0xe5e7eb))
            .text_size(px(13.))
            .child(
                div()
                    .h(px(38.))
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .bg(rgb(0x1b1d24))
                    .border_b_1()
                    .border_color(rgb(0x30343f))
                    .child(
                        div().flex().items_center().gap_2().child("Loom").child(
                            div()
                                .text_sm()
                                .text_color(rgb(0x8f98a6))
                                .child("M5 coding workspace"),
                        ),
                    )
                    .child(format!(
                        "{}  -  {}",
                        self.model.as_str(),
                        run_state_name(self.run_state)
                    ))
                    .child(header_actions),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .overflow_hidden()
                    .child(
                        div()
                            .w(px(240.))
                            .h_full()
                            .flex()
                            .bg(rgb(0x17191f))
                            .border_r_1()
                            .border_color(rgb(0x30343f))
                            .child(
                                div()
                                    .w(px(40.))
                                    .h_full()
                                    .p_2()
                                    .flex()
                                    .flex_col()
                                    .items_center()
                                    .gap_2()
                                    .bg(rgb(0x14161a))
                                    .border_r_1()
                                    .border_color(rgb(0x30343f))
                                    .child(
                                        div()
                                            .w(px(24.))
                                            .h(px(24.))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded_sm()
                                            .bg(rgb(0x293d5a))
                                            .text_sm()
                                            .child("L"),
                                    )
                                    .child(
                                        div()
                                            .w(px(24.))
                                            .h(px(24.))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded_sm()
                                            .bg(rgb(0x292d38))
                                            .text_sm()
                                            .text_color(rgb(0xdbeafe))
                                            .child("S"),
                                    )
                                    .child(
                                        div()
                                            .id("activity-refresh")
                                            .w(px(24.))
                                            .h(px(24.))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded_sm()
                                            .text_sm()
                                            .text_color(rgb(0x8f98a6))
                                            .child("R")
                                            .cursor_pointer()
                                            .on_click(cx.listener(Self::refresh)),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .h_full()
                                    .p_3()
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child(
                                        div().text_sm().text_color(rgb(0x8f98a6)).child("SESSIONS"),
                                    )
                                    .child(
                                        div()
                                            .id("session-entry")
                                            .p_2()
                                            .rounded_sm()
                                            .bg(rgb(0x292d38))
                                            .child(div().text_sm().child("* Coding workspace"))
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .text_color(rgb(0x94a3b8))
                                                    .child(session_state_name(self.session_state)),
                                            )
                                            .cursor_pointer()
                                            .on_click(cx.listener(Self::refresh)),
                                    )
                                    .child(
                                        div()
                                            .id("new-session")
                                            .text_sm()
                                            .text_color(rgb(0x8f98a6))
                                            .child("+ New session")
                                            .cursor_pointer()
                                            .on_click(cx.listener(Self::restart)),
                                    )
                                    .child(
                                        div()
                                            .mt_2()
                                            .text_sm()
                                            .text_color(rgb(0x8f98a6))
                                            .child("FILES"),
                                    )
                                    .child(self.render_file_tree(cx)),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .h_full()
                            .flex()
                            .overflow_hidden()
                            .child(
                                div()
                                    .flex_1()
                                    .h_full()
                                    .flex()
                                    .flex_col()
                                    .overflow_hidden()
                                    .child(
                                        div()
                                            .h(px(34.))
                                            .w_full()
                                            .px_2()
                                            .flex()
                                            .items_center()
                                            .gap_1()
                                            .bg(rgb(0x17191f))
                                            .border_b_1()
                                            .border_color(rgb(0x30343f))
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .text_color(rgb(0x8f98a6))
                                                    .child("WORKSPACE"),
                                            )
                                            .child(
                                                div()
                                                    .id("open-readme")
                                                    .px_2()
                                                    .py_1()
                                                    .text_sm()
                                                    .bg(rgb(0x293244))
                                                    .cursor_pointer()
                                                    .child("Open README")
                                                    .on_click(cx.listener(Self::open_readme)),
                                            )
                                            .child(
                                                div()
                                                    .id("save-buffer")
                                                    .px_2()
                                                    .py_1()
                                                    .text_sm()
                                                    .bg(rgb(0x14532d))
                                                    .cursor_pointer()
                                                    .child("Save")
                                                    .on_click(cx.listener(Self::save_active)),
                                            )
                                            .child(
                                                div()
                                                    .id("undo-buffer")
                                                    .px_2()
                                                    .py_1()
                                                    .text_sm()
                                                    .bg(rgb(0x20242c))
                                                    .cursor_pointer()
                                                    .child("Undo")
                                                    .on_click(cx.listener(Self::undo_active)),
                                            )
                                            .child(
                                                div()
                                                    .id("redo-buffer")
                                                    .px_2()
                                                    .py_1()
                                                    .text_sm()
                                                    .bg(rgb(0x20242c))
                                                    .cursor_pointer()
                                                    .child("Redo")
                                                    .on_click(cx.listener(Self::redo_active)),
                                            )
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .text_color(rgb(0x8f98a6))
                                                    .child("Ctrl+P  Ctrl+Shift+F  Ctrl+S"),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .overflow_hidden()
                                            .child(self.render_editor()),
                                    )
                                    .child(
                                        div()
                                            .h(px(150.))
                                            .w_full()
                                            .flex()
                                            .id("workspace-results")
                                            .overflow_y_scroll()
                                            .bg(rgb(0x17191f))
                                            .border_t_1()
                                            .border_color(rgb(0x30343f))
                                            .child(div().flex_1().child(self.render_diagnostics()))
                                            .child(
                                                div().flex_1().child(self.render_task_results()),
                                            ),
                                    ),
                            )
                            .child(
                                div()
                                    .w(px(340.))
                                    .h_full()
                                    .id("agent-timeline")
                                    .overflow_y_scroll()
                                    .bg(rgb(0x17191f))
                                    .border_l_1()
                                    .border_color(rgb(0x30343f))
                                    .child(
                                        div().id("timeline").p_2().child(self.render_timeline(cx)),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .h_full()
                            .w(px(280.))
                            .p_3()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .bg(rgb(0x17191f))
                            .border_l_1()
                            .border_color(rgb(0x30343f))
                            .child(div().text_sm().text_color(rgb(0x8f98a6)).child("INSPECTOR"))
                            .child(
                                div()
                                    .p_2()
                                    .rounded_sm()
                                    .bg(rgb(0x20242c))
                                    .child(format!("Run  {}", run_state_name(self.run_state)))
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(rgb(0x8f98a6))
                                            .child(format!("Model  {}", self.model.as_str())),
                                    ),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(0xb7c0d0))
                                    .child(format!("Task  {}", self.task)),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(0x8f98a6))
                                    .child(format!("Workspace  {}", self.workspace_root.display())),
                            )
                            .child(
                                div().text_sm().text_color(rgb(0x8f98a6)).child(format!(
                                    "Workspace entries  {}",
                                    self.workspace_entries
                                )),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(0x8f98a6))
                                    .child(format!("Providers  {}", self.provider_count)),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(0x8f98a6))
                                    .child(format!("Events  {}", self.timeline.len())),
                            )
                            .child(
                                div().text_sm().text_color(rgb(0x8f98a6)).child(format!(
                                    "Search matches  {}",
                                    self.search_matches.len()
                                )),
                            )
                            .child(div().text_sm().text_color(rgb(0x8f98a6)).child(format!(
                                        "Language services  {}",
                                        self.language_services
                                            .iter()
                                            .filter(|service| {
                                                service.state
                                                    == loom_language::LanguageServiceState::Ready
                                            })
                                            .count()
                                    )))
                            .child(self.render_vcs_status())
                            .child(
                                div()
                                    .flex()
                                    .gap_1()
                                    .child(
                                        div()
                                            .id("build-task")
                                            .px_2()
                                            .py_1()
                                            .text_sm()
                                            .bg(rgb(0x293244))
                                            .cursor_pointer()
                                            .child("Build")
                                            .on_click(cx.listener(Self::run_build)),
                                    )
                                    .child(
                                        div()
                                            .id("test-task")
                                            .px_2()
                                            .py_1()
                                            .text_sm()
                                            .bg(rgb(0x293244))
                                            .cursor_pointer()
                                            .child("Test")
                                            .on_click(cx.listener(Self::run_test)),
                                    )
                                    .child(
                                        div()
                                            .id("lint-task")
                                            .px_2()
                                            .py_1()
                                            .text_sm()
                                            .bg(rgb(0x293244))
                                            .cursor_pointer()
                                            .child("Lint")
                                            .on_click(cx.listener(Self::run_lint)),
                                    ),
                            )
                            .when(approval_visible, |element| {
                                element
                                    .border_1()
                                    .border_color(rgb(0xf59e0b))
                                    .child("Approval is waiting")
                            }),
                    ),
            )
            .child(
                div()
                    .h(px(24.))
                    .w_full()
                    .px_3()
                    .flex()
                    .items_center()
                    .bg(rgb(0x1b1d24))
                    .border_t_1()
                    .border_color(rgb(0x30343f))
                    .text_sm()
                    .text_color(if self.summary.is_some() {
                        rgb(0x9ad7bd)
                    } else {
                        rgb(0x8f98a6)
                    })
                    .child(format!(
                        "{}  |  {}  |  {} events  |  {} buffers{}",
                        session_state_name(self.session_state),
                        self.workspace_root.display(),
                        self.timeline.len(),
                        self.open_buffers.len(),
                        if self.open_buffers.iter().any(|buffer| buffer.dirty) {
                            "  |  unsaved"
                        } else {
                            ""
                        }
                    )),
            )
    }
}

fn negotiate(connection: &InProcessConnection) -> Result<(), LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Negotiate {
        client_version: CURRENT_PROTOCOL_VERSION,
        capabilities: CapabilitySet::new([
            Capability::CreateAgentSession,
            Capability::ReadAgentSession,
            Capability::SubscribeSessionEvents,
            Capability::StartAgentRun,
            Capability::ReadAgentRun,
            Capability::ControlAgentRun,
            Capability::PauseAgentRun,
            Capability::ResumeAgentRun,
            Capability::ForkAgentSession,
            Capability::RetryFromCheckpoint,
            Capability::ApproveAgentAction,
            Capability::ListProviders,
            Capability::ReadProviderHealth,
            Capability::ReadUsage,
            Capability::InspectContext,
            Capability::OpenWorkspace,
            Capability::ReadWorkspace,
            Capability::WriteWorkspace,
            Capability::SubscribeWorkspaceEvents,
            Capability::OpenTerminal,
            Capability::ControlTerminal,
            Capability::ReadTask,
            Capability::StartTask,
            Capability::ControlTask,
            Capability::ConfigureApprovalPolicy,
            Capability::ManageCheckpoints,
            Capability::TakeoverWorkspace,
            Capability::WorkspaceNavigation,
            Capability::SearchWorkspace,
            Capability::ReadWorkspaceInstructions,
            Capability::ReadDiagnostics,
            Capability::ReadSymbols,
            Capability::GoToDefinition,
            Capability::FindReferences,
            Capability::LanguageServiceLifecycle,
            Capability::ReadVcsStatus,
            Capability::ReadVcsDiff,
            Capability::MutateVcsIndex,
            Capability::CreateVcsCommit,
            Capability::ReadTaskEvidence,
            Capability::JsonProtocol,
        ]),
    }));
    match response.result? {
        ServerResponse::Negotiated(_) => Ok(()),
        response => Err(unexpected_response("negotiation", response)),
    }
}

fn create_session(
    connection: &InProcessConnection,
) -> Result<loom_core::AgentSessionSnapshot, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::CreateAgentSession {
        project_id: ProjectId::new(),
        name: "Coding workspace".to_owned(),
    }));
    match response.result? {
        ServerResponse::AgentSessionCreated(snapshot) => Ok(snapshot),
        response => Err(unexpected_response("session creation", response)),
    }
}

fn provider_count(connection: &InProcessConnection) -> Result<usize, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::ListProviders));
    match response.result? {
        ServerResponse::Providers { providers } => Ok(providers.len()),
        response => Err(unexpected_response("provider list", response)),
    }
}

fn start_demo_run(
    connection: &InProcessConnection,
    workspace_root: &std::path::Path,
    model: &ModelId,
) -> Result<(loom_core::AgentSessionSnapshot, AgentRunSnapshot), LoomError> {
    let session = create_session(connection)?;
    let response = connection.request(RequestEnvelope::new(ClientRequest::StartAgentRun {
        session_id: session.id,
        task: "make a small repository change and validate it".to_owned(),
        model: model.clone(),
        workspace_root: workspace_root.display().to_string(),
        system_instructions: Some(
            "Work methodically, use the available tools, and report validation.".to_owned(),
        ),
        repository_instructions: Some(
            "Keep the demonstration change small and workspace-scoped.".to_owned(),
        ),
    }));
    let run = match response.result? {
        ServerResponse::AgentRunStarted(run) => run,
        response => return Err(unexpected_response("agent run start", response)),
    };
    Ok((session, run))
}

fn workspace_snapshot(
    connection: &InProcessConnection,
    project_id: ProjectId,
) -> Result<loom_workspace::WorkspaceSnapshot, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::GetWorkspaceSnapshot {
        project_id,
    }));
    match response.result? {
        ServerResponse::WorkspaceSnapshot(snapshot) => Ok(snapshot),
        response => Err(unexpected_response("workspace snapshot", response)),
    }
}

fn prepare_workspace() -> Result<PathBuf, LoomError> {
    let root = std::env::temp_dir().join("loom-m1-ui");
    fs::create_dir_all(&root).map_err(|error| {
        LoomError::new(
            ErrorCode::ToolExecution,
            format!("could not create UI workspace: {error}"),
            false,
        )
    })?;
    let readme = root.join("README.md");
    if !readme.exists() {
        fs::write(
            readme,
            "Workspace used by the Loom M5 GPUI coding workspace.\n",
        )
        .map_err(|error| {
            LoomError::new(
                ErrorCode::ToolExecution,
                format!("could not seed UI workspace: {error}"),
                false,
            )
        })?;
    }
    if !root.join(".git").is_dir() {
        GitService::init(&root)?;
    }
    reset_demo_workspace(&root)?;
    Ok(root)
}

fn reset_demo_workspace(root: &std::path::Path) -> Result<(), LoomError> {
    let demo_file = root.join("loom-m1-demo.txt");
    if demo_file.exists() {
        fs::remove_file(demo_file).map_err(|error| {
            LoomError::new(
                ErrorCode::ToolExecution,
                format!("could not reset UI workspace: {error}"),
                false,
            )
        })?;
    }
    Ok(())
}

fn unexpected_response(operation: &str, response: ServerResponse) -> LoomError {
    LoomError::new(
        ErrorCode::Internal,
        format!("backend returned unexpected {operation} response: {response:?}"),
        false,
    )
}

fn session_state_for_run(state: AgentRunState) -> AgentSessionState {
    match state {
        AgentRunState::Planning => AgentSessionState::Planning,
        AgentRunState::Executing => AgentSessionState::Executing,
        AgentRunState::AwaitingApproval => AgentSessionState::AwaitingApproval,
        AgentRunState::Paused => AgentSessionState::Paused,
        AgentRunState::Evaluating => AgentSessionState::Evaluating,
        AgentRunState::Completed => AgentSessionState::Completed,
        AgentRunState::Failed => AgentSessionState::Failed,
        AgentRunState::Cancelled => AgentSessionState::Cancelled,
    }
}

const fn session_state_name(state: AgentSessionState) -> &'static str {
    match state {
        AgentSessionState::Idle => "idle",
        AgentSessionState::Queued => "queued",
        AgentSessionState::Planning => "planning",
        AgentSessionState::AwaitingApproval => "awaiting approval",
        AgentSessionState::Paused => "paused",
        AgentSessionState::Executing => "executing",
        AgentSessionState::Evaluating => "evaluating",
        AgentSessionState::NeedsInput => "needs input",
        AgentSessionState::Completed => "completed",
        AgentSessionState::Failed => "failed",
        AgentSessionState::Cancelled => "cancelled",
    }
}

const fn run_state_name(state: AgentRunState) -> &'static str {
    match state {
        AgentRunState::Planning => "planning",
        AgentRunState::Executing => "executing",
        AgentRunState::AwaitingApproval => "awaiting approval",
        AgentRunState::Paused => "paused",
        AgentRunState::Evaluating => "evaluating",
        AgentRunState::Completed => "completed",
        AgentRunState::Failed => "failed",
        AgentRunState::Cancelled => "cancelled",
    }
}

fn main() {
    let view = match LoomView::try_new() {
        Ok(view) => view,
        Err(error) => {
            eprintln!("could not initialize Loom UI: {error}");
            std::process::exit(1);
        }
    };
    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
        if let Err(error) = cx.open_window(
            WindowOptions {
                focus: true,
                titlebar: Some(TitlebarOptions {
                    title: Some("Loom M5".into()),
                    ..Default::default()
                }),
                window_background: WindowBackgroundAppearance::Opaque,
                window_decorations: Some(WindowDecorations::Server),
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| view),
        ) {
            eprintln!("failed to open Loom window: {error}");
            cx.quit();
        } else {
            cx.activate(true);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_state_projection_keeps_agent_and_session_status_aligned() {
        assert_eq!(
            session_state_for_run(AgentRunState::AwaitingApproval),
            AgentSessionState::AwaitingApproval
        );
        assert_eq!(
            session_state_for_run(AgentRunState::Completed),
            AgentSessionState::Completed
        );
        assert_eq!(run_state_name(AgentRunState::Paused), "paused");
        assert_eq!(
            session_state_name(AgentSessionState::NeedsInput),
            "needs input"
        );
    }
}
