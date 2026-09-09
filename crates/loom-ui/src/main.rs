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
use loom_model::{ModelId, ToolCall};
use loom_protocol::{
    CURRENT_PROTOCOL_VERSION, ClientRequest, RequestEnvelope, ServerEvent, ServerResponse,
};
use loom_server::{InProcessBackend, InProcessConnection};

struct LoomView {
    connection: InProcessConnection,
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
    Summary(String),
}

impl LoomView {
    fn try_new() -> Result<Self, LoomError> {
        let workspace_root = prepare_workspace()?;
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate(&connection)?;
        let model = ModelId::new("deterministic/demo");
        let (session, run) = start_demo_run(&connection, &workspace_root, &model)?;
        let mut view = Self {
            connection,
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
        };
        view.collect_events()?;
        Ok(view)
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
                AgentEvent::RunStateChanged { state, .. } => {
                    self.run_state = *state;
                    self.session_state = session_state_for_run(*state);
                }
                AgentEvent::RunCompleted { snapshot } => {
                    self.run_state = snapshot.state;
                    self.session_state = session_state_for_run(snapshot.state);
                    self.summary = snapshot.summary.clone();
                    if let Some(summary) = &snapshot.summary {
                        self.timeline.push(TimelineItem::Summary(summary.clone()));
                    }
                }
            },
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
        cx.notify();
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
        self.session_id = session.id;
        self.run_id = run.id;
        self.task = run.task;
        self.after_sequence = None;
        self.session_state = session.state;
        self.run_state = run.state;
        self.pending_approval = None;
        self.timeline.clear();
        self.summary = None;
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
            TimelineItem::Summary(summary) => div()
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
                .child(summary.clone())
                .into_any(),
        }
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
                                .child("M1 agent session"),
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
                                            .child(div().text_sm().child("* M1 demo"))
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
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .id("timeline")
                            .flex_1()
                            .h_full()
                            .overflow_y_scroll()
                            .child(self.render_timeline(cx)),
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
                                div()
                                    .text_sm()
                                    .text_color(rgb(0x8f98a6))
                                    .child(format!("Events  {}", self.timeline.len())),
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
                        "{}  |  {}  |  {} events",
                        session_state_name(self.session_state),
                        self.workspace_root.display(),
                        self.timeline.len()
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
            Capability::ApproveAgentAction,
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
        name: "M1 demo".to_owned(),
    }));
    match response.result? {
        ServerResponse::AgentSessionCreated(snapshot) => Ok(snapshot),
        response => Err(unexpected_response("session creation", response)),
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
        fs::write(readme, "Workspace used by the Loom M1 GPUI demo.\n").map_err(|error| {
            LoomError::new(
                ErrorCode::ToolExecution,
                format!("could not seed UI workspace: {error}"),
                false,
            )
        })?;
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
                    title: Some("Loom M1".into()),
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
