//! The GPUI view: session navigator, run canvas, composer, and review drawer.

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
    path::PathBuf,
    time::Duration,
};

use gpui::{
    App, Bounds, ClickEvent, ClipboardItem, Context, CursorStyle, Decorations, Element, Entity,
    EntityInputHandler, FocusHandle, Focusable, HitboxBehavior, ListAlignment, ListState,
    MouseButton, MouseDownEvent, Pixels, Point, Render, ResizeEdge, Subscription, Tiling,
    UTF16Selection, Window, WindowAppearance, WindowControlArea, canvas, div, list, point,
    prelude::*, px, transparent_black,
};
use gpui_base::{SelectableText, TextSelectionLayer};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{
    Icon, IconName, Sizable,
    menu::{ContextMenuExt, DropdownMenu, PopupMenuItem},
    text::TextView,
};
use loom_core::{
    ActivityId, AgentSessionId, AgentSessionSnapshot, AgentSessionState, ErrorCode, EventSequence,
    LoomError, ProjectId, RunId,
};
use loom_model::{MessageRole, ModelId, ProviderKind, ProviderSummary, ToolCall};
use loom_protocol::{
    AgentActivityData, AgentActivityRecord, AgentActivityStatus, AgentEvent, AgentRunSnapshot,
    AgentRunSnapshotProjection, AgentRunState, ClientRequest, FileActivityOperation,
    ProjectSnapshot, RequestEnvelope, ResponseEnvelope, ServerEvent, ServerResponse, TaskSnapshot,
    TaskStatus,
};
#[cfg(not(target_family = "wasm"))]
use loom_providers::{
    CredentialRef, CredentialStore, FileCredentialStore, GITHUB_COPILOT_CREDENTIAL_REF,
    GITHUB_COPILOT_DEFAULT_MODEL, GitHubCopilotAuthenticator, GitHubDeviceCode,
};
#[cfg(not(target_family = "wasm"))]
use loom_server::InProcessBackend;

use crate::{
    MAX_REVIEW_CHANGES, MAX_REVIEW_DIFF,
    connection::{BackendWorker, ClientConnection, select_remote_project, unexpected_response},
    state::{
        AgentMode, GitHubLoginState, RenameDialogState, ReviewPanel, ReviewState, ThemeChoice,
        TimelineItem, activity_status_label, bounded, bounded_to, session_state_for_run,
        session_title_from_task, upsert_activity,
    },
    text_input::{
        Backspace, Copy, Delete, End, Home, InputField, Left, LoomTooltip, Paste, Right, SelectAll,
        Submit, TextBufferState, TextInputElement,
    },
    theme::{CLIENT_DECORATION_SHADOW, ClientCorners, change_color, resize_edge, rgb, state_color},
};
#[cfg(target_family = "wasm")]
use crate::{
    browser::BrowserOptions,
    connection::{
        create_session_async, list_models_async, list_projects_async, list_sessions_async,
        negotiate_async, open_workspace_async,
    },
};
#[cfg(not(target_family = "wasm"))]
use crate::{
    connection::{
        create_session, list_models, list_projects, list_provider_ids, list_sessions, negotiate,
        open_workspace, start_run,
    },
    platform::{UiOptions, backend_persistence_path, prepare_workspace, stable_project_id},
};
use log::info;

/// Opens a URL in a new tab/window. Natively this shells out to the OS's
/// "open" handler; in the browser it's just `window.open`.
#[cfg(not(target_family = "wasm"))]
fn open_external_url(url: &str) -> Result<(), std::io::Error> {
    open::that(url)
}

#[cfg(target_family = "wasm")]
fn open_external_url(url: &str) -> Result<(), std::io::Error> {
    let opened = web_sys::window().and_then(|window| window.open_with_url(url).ok().flatten());
    if opened.is_some() {
        Ok(())
    } else {
        Err(std::io::Error::other(
            "the browser blocked opening a new tab",
        ))
    }
}

fn format_duration(elapsed_ms: u64) -> String {
    if elapsed_ms < 1_000 {
        format!("{elapsed_ms}ms")
    } else if elapsed_ms < 60_000 {
        format!("{:.1}s", elapsed_ms as f64 / 1_000.0)
    } else {
        format!(
            "{}m {}s",
            elapsed_ms / 60_000,
            (elapsed_ms % 60_000) / 1_000
        )
    }
}

fn session_state_label(state: AgentSessionState) -> &'static str {
    match state {
        AgentSessionState::Idle => "Ready",
        AgentSessionState::Queued => "Queued",
        AgentSessionState::Planning => "Planning",
        AgentSessionState::AwaitingApproval => "Needs approval",
        AgentSessionState::Paused => "Paused",
        AgentSessionState::Executing => "Working",
        AgentSessionState::Evaluating => "Reviewing",
        AgentSessionState::NeedsInput => "Needs your input",
        AgentSessionState::Completed => "Complete",
        AgentSessionState::Failed => "Something went wrong",
        AgentSessionState::Cancelled => "Cancelled",
        AgentSessionState::Archived => "Archived",
    }
}

fn run_state_label(state: Option<AgentRunState>) -> &'static str {
    match state {
        None => "Ready",
        Some(AgentRunState::Planning) => "Planning",
        Some(AgentRunState::Executing) => "Working",
        Some(AgentRunState::AwaitingApproval) => "Needs approval",
        Some(AgentRunState::Paused) => "Paused",
        Some(AgentRunState::NeedsInput) => "Needs your input",
        Some(AgentRunState::Evaluating) => "Reviewing",
        Some(AgentRunState::Completed) => "Complete",
        Some(AgentRunState::Failed) => "Something went wrong",
        Some(AgentRunState::Cancelled) => "Cancelled",
    }
}

fn change_kind_label(kind: loom_workspace::WorkspaceChangeKind) -> &'static str {
    match kind {
        loom_workspace::WorkspaceChangeKind::Created => "New",
        loom_workspace::WorkspaceChangeKind::Deleted => "Removed",
        loom_workspace::WorkspaceChangeKind::Modified => "Updated",
    }
}

fn activity_marker(status: AgentActivityStatus) -> &'static str {
    match status {
        AgentActivityStatus::Started => "›",
        AgentActivityStatus::Completed => "✓",
        AgentActivityStatus::Failed => "×",
        AgentActivityStatus::AwaitingApproval => "!",
        AgentActivityStatus::AwaitingInput => "?",
        AgentActivityStatus::Cancelled => "–",
    }
}

fn render_timeline_text(id: String, text: String, color: u32) -> gpui::AnyElement {
    let has_markdown = text.lines().any(|line| {
        let line = line.trim_start();
        (line.starts_with('#')
            && line
                .strip_prefix('#')
                .is_some_and(|rest| rest.trim_start_matches('#').starts_with(' ')))
            || line.starts_with("- ")
            || line.starts_with("* ")
            || line.starts_with("+ ")
            || line.starts_with("> ")
            || line.starts_with("```")
            || line.starts_with("~~~")
            || line.starts_with("1. ")
    }) || text.contains("**")
        || text.contains("__")
        || text.contains('`')
        || text.contains("~~")
        || text.contains("![")
        || text.contains("](");

    if !has_markdown {
        div()
            .w_full()
            .text_color(rgb(color))
            .child(SelectableText::new(id, text))
            .into_any()
    } else {
        TextView::markdown(id, text)
            .selectable(true)
            .w_full()
            .text_color(rgb(color))
            .into_any()
    }
}

fn activity_label(activity: &AgentActivityRecord) -> (String, Option<String>) {
    let compact_arguments = |call: &loom_model::ToolCall| {
        bounded_to(
            &serde_json::to_string(&call.arguments).unwrap_or_default(),
            180,
        )
    };
    match &activity.data {
        AgentActivityData::ModelTurn { model } => {
            (format!("Agent turn · {}", model.as_str()), None)
        }
        AgentActivityData::ToolCall { call, .. } => {
            (call.name.clone(), Some(compact_arguments(call)))
        }
        AgentActivityData::File {
            call,
            operation,
            path,
            ..
        } => {
            let operation = match operation {
                FileActivityOperation::List => "List files",
                FileActivityOperation::Read => "Read file",
                FileActivityOperation::Write => "Write file",
            };
            (
                operation.to_owned(),
                Some(format!(
                    "{} · {}",
                    path.as_deref().unwrap_or("."),
                    compact_arguments(call)
                )),
            )
        }
        AgentActivityData::Search {
            call, query, path, ..
        } => (
            "Search".to_owned(),
            Some(format!(
                "\"{}\"{} · {}",
                bounded_to(query, 120),
                path.as_deref()
                    .map_or_else(String::new, |path| format!(" in {path}")),
                compact_arguments(call)
            )),
        ),
        AgentActivityData::Command {
            call,
            command,
            args,
            cwd,
            ..
        } => {
            let command_line = std::iter::once(command.as_str())
                .chain(args.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" ");
            (
                "Run command".to_owned(),
                Some(format!(
                    "{}{} · {}",
                    bounded_to(&command_line, 180),
                    cwd.as_deref()
                        .map_or_else(String::new, |cwd| format!(" in {cwd}")),
                    compact_arguments(call)
                )),
            )
        }
    }
}

fn activity_output(activity: &AgentActivityRecord) -> Option<&str> {
    match &activity.data {
        AgentActivityData::ModelTurn { .. } => None,
        AgentActivityData::ToolCall { result, .. }
        | AgentActivityData::File { result, .. }
        | AgentActivityData::Search { result, .. }
        | AgentActivityData::Command { result, .. } => result
            .as_ref()
            .map(|result| result.output.as_str())
            .filter(|output| !output.is_empty()),
    }
}

fn activity_turn_title(activities: &[AgentActivityRecord]) -> &'static str {
    if activities.iter().any(|activity| {
        matches!(
            &activity.data,
            AgentActivityData::File {
                operation: FileActivityOperation::Write,
                ..
            }
        ) || matches!(
            &activity.data,
            AgentActivityData::ToolCall { call, .. } if call.name == "apply_patch"
        )
    }) {
        "Making changes"
    } else if activities
        .iter()
        .any(|activity| matches!(&activity.data, AgentActivityData::Command { .. }))
    {
        "Running commands"
    } else if activities
        .iter()
        .any(|activity| matches!(&activity.data, AgentActivityData::Search { .. }))
    {
        "Searching the codebase"
    } else if activities
        .iter()
        .any(|activity| matches!(&activity.data, AgentActivityData::File { .. }))
    {
        "Inspecting the workspace"
    } else if activities
        .iter()
        .any(|activity| matches!(&activity.data, AgentActivityData::ToolCall { .. }))
    {
        "Using tools"
    } else {
        "Working on the task"
    }
}

fn is_redundant_completion_summary(summary: &str) -> bool {
    summary.starts_with("Completed task:")
}

pub(crate) struct LoomView {
    /// Used for the synchronous bootstrap before the window exists.
    pub(crate) connection: ClientConnection,
    /// Used for every request made once the view is interactive.
    pub(crate) backend: BackendWorker,
    pub(crate) project_id: ProjectId,
    pub(crate) project: Option<ProjectSnapshot>,
    pub(crate) workspace_root: PathBuf,
    pub(crate) projects: Vec<ProjectSnapshot>,
    pub(crate) sessions: Vec<AgentSessionSnapshot>,
    pub(crate) active_session: AgentSessionSnapshot,
    pub(crate) active_run: Option<AgentRunSnapshot>,
    pub(crate) active_run_id: Option<RunId>,
    pub(crate) model: ModelId,
    pub(crate) default_model: ModelId,
    pub(crate) session_models: BTreeMap<AgentSessionId, ModelId>,
    pub(crate) agent_mode: AgentMode,
    pub(crate) agent_mode_picker_open: bool,
    pub(crate) session_task_cache: BTreeMap<AgentSessionId, String>,
    pub(crate) optimistic_messages: Vec<String>,
    pub(crate) sending_message: bool,
    pub(crate) models: Vec<ModelId>,
    pub(crate) model_picker_open: bool,
    pub(crate) settings_open: bool,
    pub(crate) providers_open: bool,
    pub(crate) providers: Vec<ProviderSummary>,
    pub(crate) theme_choice: ThemeChoice,
    appearance_subscription: Option<Subscription>,
    pub(crate) after_sequence: Option<EventSequence>,
    pub(crate) timeline: Vec<TimelineItem>,
    timeline_view: Option<Entity<TimelineView>>,
    pub(crate) activity_records_seen: bool,
    pub(crate) expanded_activities: BTreeSet<ActivityId>,
    pub(crate) approval_request_in_flight: bool,
    pub(crate) archive_request_in_flight: bool,
    pub(crate) pending_approval: Option<ToolCall>,
    pub(crate) pending_input: Option<String>,
    pub(crate) composer: TextBufferState,
    pub(crate) composer_focus_handle: FocusHandle,
    pub(crate) input_field: InputField,
    pub(crate) session_state: AgentSessionState,
    pub(crate) run_state: Option<AgentRunState>,
    pub(crate) summary: Option<String>,
    pub(crate) review: ReviewState,
    pub(crate) tasks: Vec<TaskSnapshot>,
    pub(crate) rename_dialog: Option<RenameDialogState>,
    pub(crate) rename_focus_handle: FocusHandle,
    pub(crate) demo_workspace: bool,
    pub(crate) login_enabled: bool,
    pub(crate) github_connected: bool,
    pub(crate) github_login: Option<GitHubLoginState>,
    pub(crate) run_poll_scheduled: bool,
}

struct TimelineView {
    parent: Entity<LoomView>,
    list_state: ListState,
    parent_subscription: Option<Subscription>,
    session_id: Option<AgentSessionId>,
    timeline_revision: (usize, usize),
}

impl TimelineView {
    fn new(parent: Entity<LoomView>) -> Self {
        Self {
            parent,
            list_state: ListState::new(0, ListAlignment::Top, px(120.)),
            parent_subscription: None,
            session_id: None,
            timeline_revision: (0, 0),
        }
    }

    fn sync_list(
        &mut self,
        item_count: usize,
        timeline_revision: (usize, usize),
        session_changed: bool,
    ) {
        let content_changed = self.timeline_revision != timeline_revision;
        self.timeline_revision = timeline_revision;
        if self.list_state.item_count() == item_count {
            if session_changed || content_changed {
                self.list_state.scroll_to_end();
            }
            return;
        }

        self.list_state.reset(item_count);
        self.list_state.scroll_to_end();
    }
}

impl Render for TimelineView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.parent_subscription.is_none() {
            let parent = self.parent.clone();
            self.parent_subscription = Some(cx.observe(&parent, |_, _, cx| cx.notify()));
        }

        let parent_state = self.parent.read(cx);
        let item_count = parent_state.timeline.len();
        let session_id = parent_state.active_session.id;
        let session_changed = self.session_id != Some(session_id);
        if session_changed {
            self.session_id = Some(session_id);
        }
        let timeline_revision = (
            item_count,
            parent_state
                .timeline
                .last()
                .map(|item| format!("{item:?}").len())
                .unwrap_or_default(),
        );
        self.sync_list(item_count, timeline_revision, session_changed);
        if item_count == 0 {
            return div()
                .size_full()
                .p_6()
                .child(
                    div()
                        .w_full()
                        .p_5()
                        .rounded_lg()
                        .bg(rgb(0x171c25))
                        .border_1()
                        .border_color(rgb(0x293244))
                        .text_sm()
                        .text_color(rgb(0xb7c0d0))
                        .child(
                            div()
                                .text_base()
                                .text_color(rgb(0xf3f4f6))
                                .child("Ready when you are"),
                        )
                        .child(
                            div()
                                .mt_1()
                                .text_sm()
                                .text_color(rgb(0x8f98a6))
                                .child("Describe a task below and Loom will keep the work, decisions, and results together."),
                        ),
                );
        }

        let parent = self.parent.clone();
        let timeline = list(self.list_state.clone(), move |index, _window, cx| {
            let view = parent.read(cx);
            let item = &view.timeline[index];
            view.render_timeline_item(item, index, &parent)
        })
        .size_full();

        div().size_full().p_3().child(timeline)
    }
}

impl LoomView {
    pub(crate) fn input_state(&self, field: InputField) -> Option<&TextBufferState> {
        match field {
            InputField::Composer => Some(&self.composer),
            InputField::Rename => self.rename_dialog.as_ref().map(|dialog| &dialog.input),
        }
    }

    pub(crate) fn input_state_mut(&mut self, field: InputField) -> Option<&mut TextBufferState> {
        match field {
            InputField::Composer => Some(&mut self.composer),
            InputField::Rename => self.rename_dialog.as_mut().map(|dialog| &mut dialog.input),
        }
    }

    pub(crate) fn input_focus_handle(&self, field: InputField) -> FocusHandle {
        match field {
            InputField::Composer => self.composer_focus_handle.clone(),
            InputField::Rename => self.rename_focus_handle.clone(),
        }
    }

    pub(crate) fn edit_input(&self) -> &TextBufferState {
        self.input_state(self.input_field)
            .expect("focused input field is present")
    }

    pub(crate) fn edit_input_mut(&mut self) -> &mut TextBufferState {
        let field = self.input_field;
        self.input_state_mut(field)
            .expect("focused input field is present")
    }
}

impl EntityInputHandler for LoomView {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let input = self.input_state(self.input_field)?;
        let range = input.range_from_utf16(&range_utf16);
        actual_range.replace(input.range_to_utf16(&range));
        input.text.get(range).map(str::to_owned)
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let input = self.input_state(self.input_field)?;
        Some(UTF16Selection {
            range: input.range_to_utf16(&input.selected_range),
            reversed: input.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.input_state(self.input_field)?
            .marked_range
            .as_ref()
            .map(|range| {
                self.input_state(self.input_field)
                    .expect("composer input exists")
                    .range_to_utf16(range)
            })
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        if let Some(input) = self.input_state_mut(self.input_field) {
            input.marked_range = None;
        }
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(input) = self.input_state_mut(self.input_field) {
            input.replace_utf16(range_utf16, new_text);
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(input) = self.input_state_mut(self.input_field) else {
            return;
        };
        let replacement = input.replace_utf16(range_utf16, new_text);
        if let Some(selected) = new_selected_range_utf16 {
            let selected = input.range_from_utf16(&selected);
            input.selected_range =
                replacement.start + selected.start..replacement.start + selected.end;
            input.marked_range = Some(replacement);
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        None
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

impl Focusable for LoomView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.composer_focus_handle.clone()
    }
}

impl LoomView {
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn try_new(
        options: &UiOptions,
        focus_handle: FocusHandle,
        rename_focus_handle: FocusHandle,
    ) -> Result<Self, LoomError> {
        info!("bootstrapping backend connection");
        let (connection, workspace_root, project_id, demo_workspace) =
            if let Some(remote_url) = &options.remote {
                info!("connecting to remote backend at {remote_url}");
                let token = options.token.as_deref().ok_or_else(|| {
                    LoomError::invalid_request("remote connections require LOOM_TOKEN to be set")
                })?;
                let connection = ClientConnection::remote(remote_url.clone(), token.to_owned())?;
                info!("remote transport connected; negotiating protocol");
                negotiate(&connection)?;
                let projects = list_projects(&connection)?;
                info!("remote backend returned {} project(s)", projects.len());
                let project = select_remote_project(
                    &projects,
                    options.workspace.as_deref().and_then(|path| path.to_str()),
                )?;
                let workspace_root = project.root.clone().map(PathBuf::from).ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::WorkspaceAccessDenied,
                        "selected remote project has no configured workspace root",
                        false,
                    )
                })?;
                info!("selected remote project {}", project.id);
                (connection, workspace_root, project.id, false)
            } else {
                let (workspace_root, demo_workspace) = prepare_workspace(options)?;
                info!(
                    "using workspace '{}'{}",
                    workspace_root.display(),
                    if demo_workspace { " (demo)" } else { "" }
                );
                let project_id = if demo_workspace {
                    ProjectId::new()
                } else {
                    stable_project_id(&workspace_root)
                };
                let backend = if demo_workspace {
                    info!("starting demo backend");
                    InProcessBackend::demo_with_github_copilot()?
                } else if let Some(endpoint) = &options.endpoint {
                    let persistence_path = backend_persistence_path(&workspace_root)?;
                    info!(
                        "starting local backend with OpenAI-compatible endpoint; state '{}'",
                        persistence_path.display()
                    );
                    InProcessBackend::with_openai_compatible_persistent_with_github_copilot(
                        endpoint,
                        options.api_key.as_deref().unwrap_or_default(),
                        options.model.clone(),
                        persistence_path,
                    )?
                } else {
                    let persistence_path = backend_persistence_path(&workspace_root)?;
                    info!(
                        "starting local backend with GitHub Copilot; state '{}'",
                        persistence_path.display()
                    );
                    InProcessBackend::new_persistent_with_github_copilot(persistence_path)?
                };
                (
                    ClientConnection::InProcess(backend.connect()),
                    workspace_root,
                    project_id,
                    demo_workspace,
                )
            };
        if options.remote.is_none() {
            info!("negotiating protocol and opening workspace");
            negotiate(&connection)?;
            open_workspace(&connection, project_id, &workspace_root)?;
        }
        let sessions = list_sessions(&connection, project_id)?;
        info!("loaded {} session(s)", sessions.len());
        let session = sessions.into_iter().next().map_or_else(
            || {
                info!("creating a new session");
                create_session(&connection, project_id, "New session")
            },
            |session| {
                info!("resuming session {}", session.id);
                Ok(session)
            },
        )?;
        let models = list_models(&connection)?;
        let model = if models.contains(&options.model) {
            options.model.clone()
        } else {
            models
                .iter()
                .find(|model| model.as_str() == GITHUB_COPILOT_DEFAULT_MODEL)
                .cloned()
                .or_else(|| models.first().cloned())
                .unwrap_or_else(|| options.model.clone())
        };
        info!(
            "loaded {} model(s); selected '{}'",
            models.len(),
            model.as_str()
        );
        let run = if demo_workspace {
            info!("starting demo agent run");
            Some(start_run(
                &connection,
                &session,
                &workspace_root,
                &model,
                &options.task,
            )?)
        } else {
            None
        };
        let mut view = Self {
            backend: BackendWorker::spawn(connection.clone()),
            connection,
            project_id,
            project: None,
            workspace_root,
            projects: Vec::new(),
            sessions: vec![session.clone()],
            active_session: session.clone(),
            active_run: run.clone(),
            active_run_id: run.as_ref().map(|run| run.id),
            default_model: model.clone(),
            session_models: BTreeMap::new(),
            agent_mode: AgentMode::Agent,
            agent_mode_picker_open: false,
            session_task_cache: BTreeMap::new(),
            optimistic_messages: Vec::new(),
            sending_message: false,
            model,
            models,
            model_picker_open: false,
            settings_open: false,
            providers_open: false,
            providers: Vec::new(),
            theme_choice: ThemeChoice::System,
            appearance_subscription: None,
            after_sequence: None,
            timeline: Vec::new(),
            timeline_view: None,
            activity_records_seen: false,
            expanded_activities: BTreeSet::new(),
            approval_request_in_flight: false,
            archive_request_in_flight: false,
            pending_approval: None,
            pending_input: None,
            composer: TextBufferState::new(""),
            composer_focus_handle: focus_handle,
            input_field: InputField::Composer,
            session_state: session.state,
            run_state: run.as_ref().map(|run| run.state),
            summary: run.as_ref().and_then(|run| run.summary.clone()),
            review: ReviewState::default(),
            tasks: Vec::new(),
            rename_dialog: None,
            rename_focus_handle,
            demo_workspace,
            login_enabled: options.remote.is_none(),
            github_connected: FileCredentialStore::open(FileCredentialStore::default_path())
                .ok()
                .and_then(|credentials| {
                    credentials
                        .resolve(&CredentialRef::new(GITHUB_COPILOT_CREDENTIAL_REF))
                        .ok()
                })
                .is_some(),
            github_login: None,
            run_poll_scheduled: false,
        };
        view.refresh_models();
        view.refresh_sessions()?;
        let active_session = view.active_session.clone();
        view.load_session(active_session);
        info!("initial session state loaded");
        Ok(view)
    }

    /// Builds the view for the browser client: connects to a remote backend
    /// over the in-page WebSocket transport and resolves the same project /
    /// session / model state that native's remote-mode bootstrap resolves,
    /// using the `_async` request helpers since nothing may block the page's
    /// single JS thread. Unlike [`Self::try_new`], this does not load the
    /// active session's snapshot/events itself (that requires a `Context`,
    /// which does not exist yet); the caller finishes bootstrapping once the
    /// view is mounted, via [`Self::select_session`], [`Self::reload_sessions`]
    /// and [`Self::refresh_models_async`].
    #[cfg(target_family = "wasm")]
    pub(crate) async fn try_new_browser(
        options: &BrowserOptions,
        focus_handle: FocusHandle,
        rename_focus_handle: FocusHandle,
    ) -> Result<Self, LoomError> {
        let connection = ClientConnection::browser(options.remote(), options.token())?;
        negotiate_async(&connection).await?;
        let projects = list_projects_async(&connection).await?;
        // A freshly started `--serve` backend has no projects open yet; if
        // none match (or none exist), open the requested workspace as a new
        // project ourselves, the same way the native remote-mode M4 demo
        // does.
        let (project, project_id, workspace_root) =
            match select_remote_project(&projects, options.workspace()) {
                Ok(project) => {
                    let workspace_root =
                        project.root.clone().map(PathBuf::from).ok_or_else(|| {
                            LoomError::new(
                                ErrorCode::WorkspaceAccessDenied,
                                "selected remote project has no configured workspace root",
                                false,
                            )
                        })?;
                    let project_id = project.id;
                    (Some(project), project_id, workspace_root)
                }
                Err(error) => {
                    let root = options.workspace().ok_or(error)?;
                    let project_id = ProjectId::new();
                    open_workspace_async(&connection, project_id, root).await?;
                    (None, project_id, PathBuf::from(root))
                }
            };
        let sessions = list_sessions_async(&connection, project_id).await?;
        let session = match sessions.into_iter().next() {
            Some(session) => session,
            None => create_session_async(&connection, project_id, "New session").await?,
        };
        let models = list_models_async(&connection).await?;
        let model = options
            .model()
            .cloned()
            .filter(|model| models.contains(model))
            .or_else(|| models.first().cloned())
            .unwrap_or_else(|| ModelId::new("default"));

        Ok(Self {
            backend: BackendWorker::spawn(connection.clone()),
            connection,
            project_id,
            project,
            workspace_root,
            projects,
            sessions: vec![session.clone()],
            active_session: session.clone(),
            active_run: None,
            active_run_id: None,
            default_model: model.clone(),
            session_models: BTreeMap::new(),
            agent_mode: AgentMode::Agent,
            agent_mode_picker_open: false,
            session_task_cache: BTreeMap::new(),
            optimistic_messages: Vec::new(),
            sending_message: false,
            model,
            models,
            model_picker_open: false,
            settings_open: false,
            providers_open: false,
            providers: Vec::new(),
            theme_choice: ThemeChoice::System,
            appearance_subscription: None,
            after_sequence: None,
            timeline: Vec::new(),
            timeline_view: None,
            activity_records_seen: false,
            expanded_activities: BTreeSet::new(),
            approval_request_in_flight: false,
            archive_request_in_flight: false,
            pending_approval: None,
            pending_input: None,
            composer: TextBufferState::new(""),
            composer_focus_handle: focus_handle,
            input_field: InputField::Composer,
            session_state: session.state,
            run_state: None,
            summary: None,
            review: ReviewState::default(),
            tasks: Vec::new(),
            rename_dialog: None,
            rename_focus_handle,
            demo_workspace: false,
            login_enabled: false,
            github_connected: false,
            github_login: None,
            run_poll_scheduled: false,
        })
    }

    /// Submits a backend request without blocking the UI thread and applies the
    /// answer on the UI thread once it arrives.
    pub(crate) fn dispatch(
        &self,
        cx: &mut Context<Self>,
        request: ClientRequest,
        apply: impl FnOnce(&mut Self, ResponseEnvelope, &mut Context<Self>) + 'static,
    ) {
        let pending = self.backend.submit(RequestEnvelope::new(request));
        cx.spawn(async move |view, cx| {
            let response = cx
                .background_spawn(async move { pending.wait().await })
                .await;
            view.update(cx, |view, cx| {
                apply(view, response, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn record_status(&mut self, status: impl Into<String>) {
        self.timeline.push(TimelineItem::Status(status.into()));
    }

    pub(crate) fn record_backend_error(&mut self, operation: &str, error: LoomError) {
        self.timeline.push(TimelineItem::Error {
            operation: operation.to_owned(),
            error,
        });
    }

    /// Refreshes the model list. The synchronous variant is only used during
    /// the startup bootstrap, before the window exists.
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn refresh_models(&mut self) {
        let provider_ids = match list_provider_ids(&self.connection) {
            Ok(provider_ids) => provider_ids,
            Err(error) => {
                self.record_status(format!(
                    "Could not list providers for model refresh: {error}"
                ));
                return;
            }
        };
        for provider_id in provider_ids {
            let response = self.connection.request(RequestEnvelope::new(
                ClientRequest::DiscoverProviderModels {
                    provider_id: provider_id.clone(),
                },
            ));
            if let Err(error) = response.result {
                self.record_status(format!(
                    "Model refresh unavailable for {}: {}",
                    provider_id.as_str(),
                    error.message
                ));
            }
        }
        match list_models(&self.connection) {
            Ok(models) => self.apply_models(models),
            Err(error) => self.record_status(format!("Could not refresh models: {error}")),
        }
    }

    /// Refreshes the model list from a UI handler, one request at a time, on the
    /// connection worker.
    pub(crate) fn refresh_models_async(&mut self, cx: &mut Context<Self>) {
        self.dispatch(
            cx,
            ClientRequest::ListProviders,
            |view, response, cx| match response.result {
                Ok(ServerResponse::Providers { providers }) => {
                    for provider in providers {
                        let provider_id = provider.id.clone();
                        view.dispatch(
                            cx,
                            ClientRequest::DiscoverProviderModels {
                                provider_id: provider_id.clone(),
                            },
                            move |view, response, cx| {
                                if let Err(error) = response.result {
                                    view.record_status(format!(
                                        "Model refresh unavailable for {}: {}",
                                        provider_id.as_str(),
                                        error.message
                                    ));
                                }
                                view.dispatch(
                                    cx,
                                    ClientRequest::ListModels,
                                    |view, response, _| match response.result {
                                        Ok(ServerResponse::Models { models }) => view.apply_models(
                                            models.into_iter().map(|model| model.id).collect(),
                                        ),
                                        Err(error) => view.record_status(format!(
                                            "Could not refresh models: {error}"
                                        )),
                                        Ok(response) => view.record_backend_error(
                                            "model refresh",
                                            unexpected_response("model list", response),
                                        ),
                                    },
                                );
                            },
                        );
                    }
                }
                Err(error) => view.record_status(format!(
                    "Could not list providers for model refresh: {error}"
                )),
                Ok(response) => view.record_backend_error(
                    "model refresh",
                    unexpected_response("provider list", response),
                ),
            },
        );
    }

    fn apply_models(&mut self, models: Vec<ModelId>) {
        if !models.contains(&self.model)
            && let Some(model) = models
                .iter()
                .find(|model| model.as_str() == self.default_model.as_str())
                .cloned()
                .or_else(|| models.first().cloned())
        {
            self.model = model;
        }
        self.models = models;
        self.record_status(format!(
            "Model list refreshed ({} available)",
            self.models.len()
        ));
    }

    /// Loads the session and project lists synchronously for the startup
    /// bootstrap. Interactive refreshes use [`Self::reload_sessions`].
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn refresh_sessions(&mut self) -> Result<(), LoomError> {
        let response =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::ListAgentSessions {
                    project_id: Some(self.project_id),
                    include_archived: false,
                }));
        match response.result? {
            ServerResponse::AgentSessions { sessions } => {
                self.sessions = sessions;
                if let Some(active) = self
                    .sessions
                    .iter()
                    .find(|session| session.id == self.active_session.id)
                {
                    self.active_session = active.clone();
                    self.session_state = active.state;
                }
            }
            response => return Err(unexpected_response("session list", response)),
        }

        let response = self
            .connection
            .request(RequestEnvelope::new(ClientRequest::ListProjects));
        match response.result? {
            ServerResponse::Projects { projects } => {
                self.projects = projects;
                self.project = self
                    .projects
                    .iter()
                    .find(|project| project.id == self.project_id)
                    .cloned();
            }
            response => return Err(unexpected_response("project list", response)),
        }
        Ok(())
    }

    /// Reloads the session and project lists through the connection worker.
    pub(crate) fn reload_sessions(&mut self, cx: &mut Context<Self>) {
        self.dispatch(
            cx,
            ClientRequest::ListAgentSessions {
                project_id: Some(self.project_id),
                include_archived: false,
            },
            |view, response, cx| {
                match response.result {
                    Ok(ServerResponse::AgentSessions { sessions }) => {
                        view.sessions = sessions;
                        if let Some(active) = view
                            .sessions
                            .iter()
                            .find(|session| session.id == view.active_session.id)
                        {
                            view.active_session = active.clone();
                            view.session_state = active.state;
                        }
                    }
                    Err(error) => view.record_backend_error("session list refresh", error),
                    Ok(response) => view.record_backend_error(
                        "session list refresh",
                        unexpected_response("session list", response),
                    ),
                }
                view.dispatch(
                    cx,
                    ClientRequest::ListProjects,
                    |view, response, _| match response.result {
                        Ok(ServerResponse::Projects { projects }) => {
                            view.projects = projects;
                            view.project = view
                                .projects
                                .iter()
                                .find(|project| project.id == view.project_id)
                                .cloned();
                        }
                        Err(error) => view.record_backend_error("project list refresh", error),
                        Ok(response) => view.record_backend_error(
                            "project list refresh",
                            unexpected_response("project list", response),
                        ),
                    },
                );
            },
        );
    }

    pub(crate) fn reset_projection(&mut self) {
        self.timeline.clear();
        self.activity_records_seen = false;
        self.expanded_activities.clear();
        self.approval_request_in_flight = false;
        self.pending_approval = None;
        self.pending_input = None;
        self.active_run = None;
        self.active_run_id = None;
        self.run_state = None;
        self.summary = None;
        self.after_sequence = None;
    }

    fn enable_activity_projection(&mut self) {
        if self.activity_records_seen {
            return;
        }
        self.activity_records_seen = true;
        self.timeline.retain(|item| {
            !matches!(
                item,
                TimelineItem::ToolRequested { .. }
                    | TimelineItem::ToolStarted(_)
                    | TimelineItem::ToolOutput(_)
                    | TimelineItem::ToolCompleted { .. }
                    | TimelineItem::Approval { .. }
            )
        });
    }

    pub(crate) fn activate_session(&mut self, session: AgentSessionSnapshot) {
        self.active_session = session;
        self.session_state = self.active_session.state;
        self.model = self
            .session_models
            .get(&self.active_session.id)
            .cloned()
            .unwrap_or_else(|| self.default_model.clone());
        self.reset_projection();
        self.review.selected_file = None;
        self.review.changes.clear();
        self.review.diff = None;
        self.review.diff_path = None;
        self.review.evidence.clear();
        self.tasks.clear();
    }

    /// Loads a session synchronously.
    ///
    /// Only used by the startup bootstrap, before a window exists; every
    /// interactive path uses [`Self::select_session`], which goes through the
    /// connection worker.
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn load_session(&mut self, session: AgentSessionSnapshot) {
        self.activate_session(session);
        let fallback_projection = match self
            .connection
            .request(RequestEnvelope::new(
                ClientRequest::GetAgentSessionSnapshot {
                    session_id: self.active_session.id,
                },
            ))
            .result
        {
            Ok(ServerResponse::AgentSessionSnapshot(projection)) => {
                self.active_session = projection.session.clone();
                self.session_state = self.active_session.state;
                if let Some(run) = &projection.active_run {
                    self.model = run.run.model.clone();
                    self.session_task_cache
                        .insert(self.active_session.id, run.run.task.clone());
                }
                Some(projection)
            }
            Err(error) => {
                self.record_backend_error("load session snapshot", error);
                None
            }
            Ok(response) => {
                self.record_backend_error(
                    "load session snapshot",
                    unexpected_response("session snapshot", response),
                );
                None
            }
        };
        if let Err(error) = self.collect_events_since(
            None,
            fallback_projection.and_then(|projection| projection.active_run),
        ) {
            self.record_backend_error("load session events", error);
        }
        self.ensure_session_task_message(self.active_session.id);
        if let Some(run) = &self.active_run {
            self.session_task_cache
                .insert(self.active_session.id, run.task.clone());
            self.ensure_session_task_message(self.active_session.id);
        }
        // The session projection and event stream are sufficient for the
        // central view. Review/VCS data is loaded when the review pane opens.
    }

    #[cfg(not(target_family = "wasm"))]
    fn collect_events_since(
        &mut self,
        after_sequence: Option<EventSequence>,
        fallback: Option<AgentRunSnapshotProjection>,
    ) -> Result<(), LoomError> {
        let response =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
                    session_id: Some(self.active_session.id),
                    after_sequence,
                }));
        match response.result? {
            ServerResponse::SessionEvents { events } => {
                self.reset_projection();
                for event in events {
                    self.after_sequence = Some(event.sequence);
                    self.consume_event(&event.event);
                }
                if self.timeline.is_empty()
                    && let Some(projection) = fallback
                {
                    self.apply_run_projection(projection);
                }
            }
            ServerResponse::SessionEventsSnapshot {
                session,
                events,
                latest_sequence,
                ..
            } => {
                self.active_session = session;
                self.reset_projection();
                self.after_sequence = Some(latest_sequence);
                for event in events {
                    self.after_sequence = Some(event.sequence);
                    self.consume_event(&event.event);
                }
                if self.timeline.is_empty()
                    && let Some(projection) = fallback
                {
                    self.apply_run_projection(projection);
                }
            }
            response => return Err(unexpected_response("session event stream", response)),
        }
        Ok(())
    }

    /// Applies newly journaled session events through the connection worker.
    fn poll_run_once(&mut self, cx: &mut Context<Self>) {
        self.dispatch(
            cx,
            ClientRequest::GetSessionEvents {
                session_id: Some(self.active_session.id),
                after_sequence: self.after_sequence,
            },
            |view, response, cx| {
                match response.result {
                    Ok(ServerResponse::SessionEvents { events }) => {
                        for event in events {
                            view.after_sequence = Some(event.sequence);
                            view.consume_event(&event.event);
                        }
                    }
                    Ok(ServerResponse::SessionEventsSnapshot {
                        session,
                        events,
                        latest_sequence,
                        ..
                    }) => {
                        view.active_session = session;
                        view.reset_projection();
                        view.after_sequence = Some(latest_sequence);
                        for event in events {
                            view.after_sequence = Some(event.sequence);
                            view.consume_event(&event.event);
                        }
                    }
                    Err(error) => view.record_backend_error("session event stream", error),
                    Ok(response) => view.record_backend_error(
                        "session event stream",
                        unexpected_response("session event stream", response),
                    ),
                }
                view.run_poll_scheduled = false;
                view.schedule_run_poll(cx);
            },
        );
    }

    fn run_is_active(&self) -> bool {
        matches!(
            self.run_state,
            Some(AgentRunState::Planning | AgentRunState::Executing | AgentRunState::Evaluating)
        )
    }

    /// Polls the journal while a run is active. The protocol keeps the event
    /// stream resumable, so polling is sufficient for both local and remote
    /// connections without making the UI depend on a transport-specific push
    /// implementation.
    pub(crate) fn schedule_run_poll(&mut self, cx: &mut Context<Self>) {
        if self.run_poll_scheduled || !self.run_is_active() {
            return;
        }
        self.run_poll_scheduled = true;
        cx.spawn(async move |view, cx| {
            cx.background_spawn(async {
                std::thread::sleep(Duration::from_millis(250));
            })
            .await;
            view.update(cx, |view, cx| {
                if view.run_is_active() {
                    view.poll_run_once(cx);
                } else {
                    view.run_poll_scheduled = false;
                }
            })
            .ok();
        })
        .detach();
    }

    fn start_run_polling(&mut self, cx: &mut Context<Self>) {
        if self.run_poll_scheduled || self.active_run_id.is_none() {
            return;
        }
        self.run_poll_scheduled = true;
        self.poll_run_once(cx);
    }

    fn update_session_list(&mut self) {
        if let Some(session) = self
            .sessions
            .iter_mut()
            .find(|session| session.id == self.active_session.id)
        {
            *session = self.active_session.clone();
        }
    }

    pub(crate) fn consume_event(&mut self, event: &ServerEvent) {
        match event {
            ServerEvent::AgentSessionCreated { snapshot } => {
                self.active_session = snapshot.clone();
                self.session_state = snapshot.state;
                self.update_session_list();
            }
            ServerEvent::AgentSessionStateChanged { current, .. } => {
                self.session_state = *current;
                self.active_session.state = *current;
                self.update_session_list();
            }
            ServerEvent::AgentSessionForked { .. } => {}
            ServerEvent::AgentSessionRenamed { name, .. } => {
                self.active_session.name = name.clone();
                self.update_session_list();
            }
            ServerEvent::AgentSessionArchived { .. } => {
                self.session_state = AgentSessionState::Archived;
                self.active_session.state = AgentSessionState::Archived;
                self.update_session_list();
            }
            ServerEvent::Agent { event } => self.consume_agent_event(event),
            ServerEvent::WorkspaceChanged { change } => {
                self.record_status(format!("Workspace {:?}: {}", change.kind, change.path));
            }
            ServerEvent::Terminal { .. } | ServerEvent::Task { .. } => {}
            ServerEvent::ProviderHealthChanged {
                provider_id,
                health,
            } => self.record_status(format!(
                "Provider {} health: {:?}",
                provider_id.as_str(),
                health.state
            )),
        }
    }

    pub(crate) fn consume_agent_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::RunStarted { snapshot } => {
                self.active_run = Some(snapshot.clone());
                self.active_run_id = Some(snapshot.id);
                self.run_state = Some(snapshot.state);
            }
            AgentEvent::PlanProposed { plan, .. } => self.timeline.push(TimelineItem::Plan {
                steps: plan
                    .steps
                    .iter()
                    .map(|step| step.description.clone())
                    .collect(),
                completed: BTreeSet::new(),
                active: None,
            }),
            AgentEvent::UserMessage { text, .. } => {
                if self
                    .optimistic_messages
                    .first()
                    .is_some_and(|pending| pending == text)
                {
                    self.optimistic_messages.remove(0);
                } else {
                    self.timeline.push(TimelineItem::User(text.clone()));
                }
            }
            AgentEvent::AssistantMessageDelta { text, .. } => {
                if text.is_empty() {
                    return;
                }
                if let Some(TimelineItem::Assistant(message)) = self.timeline.last_mut() {
                    message.push_str(text);
                } else {
                    self.timeline.push(TimelineItem::Assistant(text.clone()));
                }
            }
            AgentEvent::StepStarted { index, .. } => {
                if let Some(TimelineItem::Plan { active, .. }) = self
                    .timeline
                    .iter_mut()
                    .rev()
                    .find(|item| matches!(item, TimelineItem::Plan { .. }))
                {
                    *active = Some(*index);
                }
            }
            AgentEvent::StepCompleted { index, .. } => {
                if let Some(TimelineItem::Plan {
                    completed, active, ..
                }) = self
                    .timeline
                    .iter_mut()
                    .rev()
                    .find(|item| matches!(item, TimelineItem::Plan { .. }))
                {
                    completed.insert(*index);
                    if active == &Some(*index) {
                        *active = None;
                    }
                }
            }
            AgentEvent::ContextInspected { .. } => {}
            AgentEvent::ProviderError { error, .. } | AgentEvent::ContextError { error, .. } => {
                self.timeline.push(TimelineItem::Error {
                    operation: "agent".to_owned(),
                    error: error.clone(),
                });
            }
            AgentEvent::ToolCallRequested { call, .. } => {
                if !self.activity_records_seen {
                    self.timeline.push(TimelineItem::ToolRequested {
                        name: call.name.clone(),
                        arguments: bounded(
                            &serde_json::to_string(&call.arguments).unwrap_or_default(),
                        ),
                    });
                }
            }
            AgentEvent::ToolApprovalRequired { call, .. } => {
                for item in &mut self.timeline {
                    if let TimelineItem::Approval { active, .. } = item {
                        *active = false;
                    }
                }
                self.pending_approval = Some(call.clone());
                if !self.activity_records_seen {
                    self.timeline.push(TimelineItem::Approval {
                        name: call.name.clone(),
                        active: true,
                    });
                }
            }
            AgentEvent::ToolPolicyEvaluated { .. } => {}
            AgentEvent::ToolApprovalDecided { .. } => {
                self.pending_approval = None;
                self.approval_request_in_flight = false;
                for item in &mut self.timeline {
                    if let TimelineItem::Approval { active, .. } = item {
                        *active = false;
                    }
                }
            }
            AgentEvent::ToolCallStarted { call, .. } => {
                if !self.activity_records_seen {
                    self.timeline
                        .push(TimelineItem::ToolStarted(call.name.clone()));
                }
            }
            AgentEvent::ToolOutputChunk { chunk, .. } => {
                if !self.activity_records_seen {
                    if let Some(TimelineItem::ToolOutput(output)) = self.timeline.last_mut() {
                        output.push_str(chunk);
                        *output = bounded(output);
                    } else {
                        self.timeline.push(TimelineItem::ToolOutput(bounded(chunk)));
                    }
                }
            }
            AgentEvent::ToolCallCompleted { result, .. } => {
                if !self.activity_records_seen {
                    self.timeline.push(TimelineItem::ToolCompleted {
                        name: result.name.clone(),
                        success: result.success,
                    });
                }
            }
            AgentEvent::ActivityRecorded { activity, .. } => {
                self.enable_activity_projection();
                upsert_activity(&mut self.timeline, activity.clone());
            }
            AgentEvent::NeedsInput { prompt, .. } => {
                self.pending_input = Some(prompt.clone());
                self.timeline.push(TimelineItem::NeedsInput(prompt.clone()));
            }
            AgentEvent::RunUsage { .. } | AgentEvent::RunUsageUpdated { .. } => {}
            AgentEvent::RunLimitReached { status, .. } => {
                self.record_status(format!("Limit reached: {:?}", status.exceeded));
            }
            AgentEvent::RecoveryRequired { reason, .. } => {
                self.record_status(format!("Recovery required: {reason}"));
            }
            AgentEvent::RunStateChanged { state, .. } => {
                self.run_state = Some(*state);
                self.session_state = session_state_for_run(*state);
                self.active_session.state = self.session_state;
                self.update_session_list();
            }
            AgentEvent::RunCompleted { snapshot } => {
                self.active_run = Some(snapshot.clone());
                self.active_run_id = Some(snapshot.id);
                self.run_state = Some(snapshot.state);
                self.session_state = session_state_for_run(snapshot.state);
                self.active_session.state = self.session_state;
                self.update_session_list();
                self.summary = snapshot.summary.clone();
                if let Some(summary) = &snapshot.summary
                    && !is_redundant_completion_summary(summary)
                {
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
        }
    }

    pub(crate) fn apply_run_projection(&mut self, projection: AgentRunSnapshotProjection) {
        self.active_run_id = Some(projection.run.id);
        self.active_run = Some(projection.run.clone());
        self.run_state = Some(projection.run.state);
        self.summary = projection.run.summary.clone();
        self.pending_approval = projection.pending_approval;
        self.pending_input = projection.pending_input;
        let activity_records = projection.activities;
        if !activity_records.is_empty() {
            self.enable_activity_projection();
        }
        if self.timeline.is_empty() {
            let mut timeline = Vec::new();
            for message in projection.messages {
                match message.role {
                    MessageRole::User => timeline.push(TimelineItem::User(message.content)),
                    MessageRole::Assistant => {
                        if message.content.is_empty() {
                            continue;
                        }
                        if let Some(TimelineItem::Assistant(previous)) = timeline.last_mut() {
                            if !previous.is_empty() && !message.content.is_empty() {
                                previous.push_str("\n\n");
                            }
                            previous.push_str(&message.content);
                        } else {
                            timeline.push(TimelineItem::Assistant(message.content));
                        }
                    }
                    MessageRole::Tool if activity_records.is_empty() => {
                        timeline.push(TimelineItem::ToolOutput(bounded(&message.content)))
                    }
                    MessageRole::Tool => {}
                    MessageRole::System => {}
                }
            }
            if !projection.plan.is_empty() {
                timeline.insert(
                    0,
                    TimelineItem::Plan {
                        steps: projection
                            .plan
                            .into_iter()
                            .map(|step| step.description)
                            .collect(),
                        completed: BTreeSet::new(),
                        active: None,
                    },
                );
            }
            self.timeline = timeline;
        }
        for activity in activity_records {
            upsert_activity(&mut self.timeline, activity);
        }
        if let Some(summary) = &projection.run.summary
            && !is_redundant_completion_summary(summary)
            && !self
                .timeline
                .iter()
                .any(|item| matches!(item, TimelineItem::Summary { .. }))
        {
            self.timeline.push(TimelineItem::Summary {
                text: summary.clone(),
                evidence: projection
                    .run
                    .evidence
                    .iter()
                    .map(|link| format!("{} ({})", link.label, link.uri))
                    .collect(),
            });
        }
    }

    /// Loads the review projections through the connection worker.
    pub(crate) fn refresh_review(&mut self, cx: &mut Context<Self>) {
        self.dispatch(
            cx,
            ClientRequest::GetWorkspaceChanges {
                project_id: self.project_id,
                after_sequence: None,
            },
            |view, response, _| match response.result {
                Ok(ServerResponse::WorkspaceChanges { changes, truncated }) => {
                    let mut seen = BTreeSet::new();
                    view.review.changes = changes
                        .into_iter()
                        .rev()
                        .filter(|change| seen.insert(change.path.clone()))
                        .take(MAX_REVIEW_CHANGES)
                        .collect();
                    if truncated {
                        view.record_status(
                            "Workspace review is showing the most recent changes".to_owned(),
                        );
                    }
                }
                Err(error) => view.record_backend_error("workspace review refresh", error),
                Ok(response) => view.record_backend_error(
                    "workspace review refresh",
                    unexpected_response("workspace changes", response),
                ),
            },
        );
        self.dispatch(
            cx,
            ClientRequest::GetVcsStatus {
                project_id: self.project_id,
            },
            |view, response, _| match response.result {
                Ok(ServerResponse::VcsStatus(status)) => view.review.vcs = Some(status),
                Err(error) => {
                    view.review.vcs = None;
                    view.record_status(format!("VCS review unavailable: {error}"));
                }
                Ok(response) => view.record_backend_error(
                    "VCS review refresh",
                    unexpected_response("VCS status", response),
                ),
            },
        );
    }

    pub(crate) fn confirm_rename(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.rename_dialog.take() else {
            return;
        };
        let name = dialog.input.text.trim().to_owned();
        if name.is_empty() {
            self.record_status("Session name cannot be empty");
            self.rename_dialog = Some(dialog);
            return;
        }
        self.dispatch(
            cx,
            ClientRequest::RenameAgentSession {
                session_id: dialog.session.id,
                name,
            },
            move |view, response, cx| match response.result {
                Ok(ServerResponse::AgentSessionRenamed(snapshot)) => {
                    view.active_session = snapshot;
                    view.reload_sessions(cx);
                }
                Err(error) => {
                    view.rename_dialog = Some(dialog);
                    view.record_backend_error("rename session", error);
                }
                Ok(response) => view.record_backend_error(
                    "rename session",
                    unexpected_response("session rename", response),
                ),
            },
        );
    }

    pub(crate) fn archive_active(&mut self, cx: &mut Context<Self>) {
        if self.archive_request_in_flight {
            return;
        }
        self.archive_request_in_flight = true;
        self.record_status("Archiving session...");
        let session_id = self.active_session.id;
        self.dispatch(
            cx,
            ClientRequest::ArchiveAgentSession { session_id },
            move |view, response, cx| {
                view.archive_request_in_flight = false;
                match response.result {
                    Ok(ServerResponse::AgentSessionArchived(snapshot)) => {
                        view.sessions.retain(|session| session.id != snapshot.id);
                        if let Some(session) = view.sessions.first().cloned() {
                            view.select_session(session, cx);
                        } else {
                            view.reset_projection();
                            view.create_session_async("New session".to_owned(), cx);
                        }
                    }
                    Err(error) => view.record_backend_error("archive session", error),
                    Ok(response) => view.record_backend_error(
                        "archive session",
                        unexpected_response("session archive", response),
                    ),
                }
            },
        );
    }

    pub(crate) fn send_message(&mut self, message: String, cx: &mut Context<Self>) {
        self.sending_message = true;
        let session_title = if self.active_run_id.is_none() {
            self.session_task_cache
                .insert(self.active_session.id, message.clone());
            let title = session_title_from_task(&message);
            self.active_session.name = title.clone();
            if let Some(session) = self
                .sessions
                .iter_mut()
                .find(|session| session.id == self.active_session.id)
            {
                session.name = title;
            }
            Some(self.active_session.name.clone())
        } else {
            None
        };
        let request = if let Some(run_id) = self.active_run_id {
            self.optimistic_messages.push(message.clone());
            ClientRequest::SendAgentMessage {
                run_id,
                message: message.clone(),
            }
        } else {
            if !self.demo_workspace && self.model.as_str() == "deterministic/demo" {
                self.sending_message = false;
                self.record_backend_error(
                    "start run",
                    LoomError::invalid_state(
                        "click Log in in the title bar to connect GitHub Copilot, or configure LOOM_OPENAI_ENDPOINT and LOOM_MODEL before starting a real run",
                    ),
                );
                return;
            }
            ClientRequest::StartAgentRun {
                session_id: self.active_session.id,
                task: message.clone(),
                model: self.model.clone(),
                workspace_root: self.workspace_root.display().to_string(),
                system_instructions: Some(
                    "Work methodically, use the available tools, and report validation.".to_owned(),
                ),
                repository_instructions: Some(
                    "Keep the change focused and provide reviewable evidence.".to_owned(),
                ),
            }
        };
        self.timeline.push(TimelineItem::User(message));
        let session_id = self.active_session.id;
        let rename_request = session_title.map(|title| {
            self.backend
                .submit(RequestEnvelope::new(ClientRequest::RenameAgentSession {
                    session_id,
                    name: title,
                }))
        });
        let run_request = self.backend.submit(RequestEnvelope::new(request));
        cx.spawn(async move |view, cx| {
            let response = cx
                .background_spawn(async move {
                    // The worker executes both requests in order; the rename is
                    // cosmetic, so its outcome does not gate the run.
                    if let Some(rename_request) = rename_request {
                        let _ = rename_request.wait().await;
                    }
                    run_request.wait().await
                })
                .await;
            view.update(cx, |view, cx| view.finish_send_response(response, cx))
                .ok();
        })
        .detach();
    }

    pub(crate) fn finish_send_response(
        &mut self,
        response: ResponseEnvelope,
        cx: &mut Context<Self>,
    ) {
        self.sending_message = false;
        match response.result {
            Ok(ServerResponse::AgentRunStarted(run)) | Ok(ServerResponse::AgentRun(run)) => {
                self.session_task_cache
                    .insert(self.active_session.id, run.task.clone());
                self.active_run = Some(run);
                self.active_run_id = self.active_run.as_ref().map(|run| run.id);
                self.run_state = self.active_run.as_ref().map(|run| run.state);
                self.session_state = AgentSessionState::Executing;
                if let Some(run) = &self.active_run
                    && !self
                        .timeline
                        .iter()
                        .any(|item| matches!(item, TimelineItem::User(text) if text == &run.task))
                {
                    self.timeline
                        .insert(0, TimelineItem::User(run.task.clone()));
                }
                self.pending_input = None;
                self.start_run_polling(cx);
                if let Some(run) = &self.active_run
                    && !self
                        .timeline
                        .iter()
                        .any(|item| matches!(item, TimelineItem::User(text) if text == &run.task))
                {
                    self.timeline
                        .insert(0, TimelineItem::User(run.task.clone()));
                }
                self.refresh_review(cx);
            }
            Err(error) => self.record_backend_error("send message", error),
            Ok(response) => self.record_backend_error(
                "send message",
                unexpected_response("send message", response),
            ),
        }
        cx.notify();
    }

    fn approve_pending_action(&mut self, cx: &mut Context<Self>) {
        if self.approval_request_in_flight {
            return;
        }
        let (Some(run_id), Some(call)) = (self.active_run_id, self.pending_approval.clone()) else {
            return;
        };
        self.approval_request_in_flight = true;
        self.dispatch(
            cx,
            ClientRequest::ApproveAgentAction {
                run_id,
                tool_call_id: call.id,
            },
            |view, response, cx| view.finish_approval_response(response, cx),
        );
    }

    fn reject_pending_action(&mut self, cx: &mut Context<Self>) {
        if self.approval_request_in_flight {
            return;
        }
        let (Some(run_id), Some(call)) = (self.active_run_id, self.pending_approval.clone()) else {
            return;
        };
        self.approval_request_in_flight = true;
        self.dispatch(
            cx,
            ClientRequest::RejectAgentAction {
                run_id,
                tool_call_id: call.id,
                reason: None,
            },
            |view, response, cx| view.finish_approval_response(response, cx),
        );
    }

    fn finish_approval_response(&mut self, response: ResponseEnvelope, cx: &mut Context<Self>) {
        match response.result {
            Ok(ServerResponse::AgentRun(run)) | Ok(ServerResponse::AgentRunStarted(run)) => {
                self.active_run = Some(run.clone());
                self.active_run_id = Some(run.id);
                self.run_state = Some(run.state);
                self.session_state = session_state_for_run(run.state);
                self.active_session.state = self.session_state;
                self.start_run_polling(cx);
            }
            Err(error) => {
                self.approval_request_in_flight = false;
                self.record_backend_error("approval", error);
            }
            Ok(response) => {
                self.approval_request_in_flight = false;
                self.record_backend_error("approval", unexpected_response("approval", response))
            }
        }
        cx.notify();
    }

    pub(crate) fn submit_composer(&mut self, cx: &mut Context<Self>) {
        let text = self.composer.text.trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.composer.set_text("");
        self.send_message(text, cx);
    }

    pub(crate) fn focus_composer(
        &mut self,
        _: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.composer_focus_handle.focus(window, cx);
        cx.notify();
    }

    pub(crate) fn focus_rename(
        &mut self,
        _: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.rename_focus_handle.focus(window, cx);
        cx.notify();
    }

    pub(crate) fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        if self.edit_input().selected_range.is_empty() {
            let cursor = self.edit_input().cursor_offset();
            if cursor == 0 {
                return;
            }
            let input = self.edit_input_mut();
            input.selected_range = input.previous_boundary(cursor)..cursor;
        }
        self.edit_input_mut().replace_utf16(None, "");
        cx.notify();
    }

    pub(crate) fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        if self.edit_input().selected_range.is_empty() {
            let cursor = self.edit_input().cursor_offset();
            if cursor >= self.edit_input().text.len() {
                return;
            }
            let input = self.edit_input_mut();
            input.selected_range = cursor..input.next_boundary(cursor);
        }
        self.edit_input_mut().replace_utf16(None, "");
        cx.notify();
    }

    pub(crate) fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        let offset = if self.edit_input().selected_range.is_empty() {
            self.edit_input()
                .previous_boundary(self.edit_input().cursor_offset())
        } else {
            self.edit_input().selected_range.start
        };
        self.edit_input_mut().move_to(offset, false);
        cx.notify();
    }

    pub(crate) fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        let offset = if self.edit_input().selected_range.is_empty() {
            self.edit_input()
                .next_boundary(self.edit_input().cursor_offset())
        } else {
            self.edit_input().selected_range.end
        };
        self.edit_input_mut().move_to(offset, false);
        cx.notify();
    }

    pub(crate) fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.edit_input_mut().select_all();
        cx.notify();
    }

    pub(crate) fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        let (line, _) = self
            .edit_input()
            .line_and_column(self.edit_input().cursor_offset());
        let offset = self.edit_input().line_ranges()[line].start;
        self.edit_input_mut().move_to(offset, false);
        cx.notify();
    }

    pub(crate) fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        let (line, _) = self
            .edit_input()
            .line_and_column(self.edit_input().cursor_offset());
        let offset = self.edit_input().line_ranges()[line].end;
        self.edit_input_mut().move_to(offset, false);
        cx.notify();
    }

    pub(crate) fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.edit_input_mut().replace_utf16(None, &text);
            cx.notify();
        }
    }

    pub(crate) fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_input().selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.edit_input().text[self.edit_input().selected_range.clone()].to_owned(),
            ));
        }
    }

    pub(crate) fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        if self.rename_dialog.is_some() {
            self.confirm_rename(cx);
        } else {
            self.submit_composer(cx);
        }
        cx.notify();
    }

    pub(crate) fn select_session(&mut self, session: AgentSessionSnapshot, cx: &mut Context<Self>) {
        self.github_login = None;
        self.settings_open = false;
        self.providers_open = false;
        self.review.open = false;
        self.activate_session(session.clone());
        self.ensure_session_task_message(session.id);
        let session_id = session.id;
        let backend = self.backend.clone();
        let snapshot_request = self.backend.submit(RequestEnvelope::new(
            ClientRequest::GetAgentSessionSnapshot { session_id },
        ));
        cx.spawn(async move |view, cx| {
            let snapshot = cx
                .background_spawn(async move { snapshot_request.wait().await })
                .await;
            let events_request =
                backend.submit(RequestEnvelope::new(ClientRequest::GetSessionEvents {
                    session_id: Some(session_id),
                    after_sequence: None,
                }));
            let events = cx
                .background_spawn(async move { events_request.wait().await })
                .await;
            view.update(cx, |view, cx| {
                view.finish_async_session_load(session_id, snapshot, events, cx);
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    pub(crate) fn finish_async_session_load(
        &mut self,
        session_id: AgentSessionId,
        snapshot_response: ResponseEnvelope,
        events_response: ResponseEnvelope,
        cx: &mut Context<Self>,
    ) {
        if self.active_session.id != session_id {
            return;
        }
        let fallback_projection = match snapshot_response.result {
            Ok(ServerResponse::AgentSessionSnapshot(projection)) => {
                self.active_session = projection.session.clone();
                if let Some(run) = &projection.active_run {
                    self.session_task_cache
                        .insert(session_id, run.run.task.clone());
                    self.model = run.run.model.clone();
                }
                Some(projection)
            }
            Err(error) => {
                self.record_backend_error("load session snapshot", error);
                self.reset_projection();
                None
            }
            Ok(response) => {
                self.record_backend_error(
                    "load session snapshot",
                    unexpected_response("session snapshot", response),
                );
                self.reset_projection();
                None
            }
        };
        self.reset_projection();
        match events_response.result {
            Ok(ServerResponse::SessionEvents { events }) => {
                for event in events {
                    self.after_sequence = Some(event.sequence);
                    self.consume_event(&event.event);
                }
                if self.timeline.is_empty()
                    && let Some(projection) = fallback_projection
                        .as_ref()
                        .and_then(|projection| projection.active_run.clone())
                {
                    self.apply_run_projection(projection);
                }
            }
            Ok(ServerResponse::SessionEventsSnapshot {
                session,
                events,
                latest_sequence,
                ..
            }) => {
                self.active_session = session;
                self.reset_projection();
                self.after_sequence = Some(latest_sequence);
                for event in events {
                    self.after_sequence = Some(event.sequence);
                    self.consume_event(&event.event);
                }
                if self.timeline.is_empty()
                    && let Some(projection) = fallback_projection
                        .as_ref()
                        .and_then(|projection| projection.active_run.clone())
                {
                    self.apply_run_projection(projection);
                }
            }
            Err(error) => {
                if let Some(projection) = fallback_projection
                    .as_ref()
                    .and_then(|projection| projection.active_run.clone())
                {
                    self.apply_run_projection(projection);
                }
                self.record_backend_error("load session events", error);
            }
            Ok(response) => self.record_backend_error(
                "load session events",
                unexpected_response("session event stream", response),
            ),
        }
        self.ensure_session_task_message(session_id);
        cx.notify();
    }

    pub(crate) fn ensure_session_task_message(&mut self, session_id: AgentSessionId) {
        let Some(task) = self.session_task_cache.get(&session_id).cloned() else {
            return;
        };
        if !self
            .timeline
            .iter()
            .any(|item| matches!(item, TimelineItem::User(text) if text == &task))
        {
            self.timeline.insert(0, TimelineItem::User(task));
        }
    }

    pub(crate) fn toggle_github_login(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = &self.github_login {
            if matches!(
                state,
                GitHubLoginState::Success | GitHubLoginState::Error(_)
            ) {
                self.github_login = None;
                cx.notify();
            }
            return;
        }
        self.settings_open = false;
        self.review.open = false;
        self.start_github_login(cx);
    }

    pub(crate) fn copy_github_login_value(
        &mut self,
        value: String,
        label: &'static str,
        cx: &mut Context<Self>,
    ) {
        cx.write_to_clipboard(ClipboardItem::new_string(value));
        self.record_status(format!("Copied GitHub {label} to clipboard"));
        cx.notify();
    }

    pub(crate) fn start_github_login(&mut self, cx: &mut Context<Self>) {
        self.github_login = Some(GitHubLoginState::Starting);
        cx.notify();
        self.start_github_login_flow(cx);
    }

    #[cfg(not(target_family = "wasm"))]
    fn start_github_login_flow(&mut self, cx: &mut Context<Self>) {
        let task = cx.background_spawn(async { GitHubCopilotAuthenticator::default().begin() });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            view.update(cx, |view, cx| view.handle_github_device_code(result, cx))
                .ok();
        })
        .detach();
    }

    /// GitHub Copilot device-code sign-in relies on `loom-providers`'
    /// credential store and OAuth flow, which have no browser equivalent (no
    /// OS keychain, no writable local file). The browser client cannot offer
    /// this login path.
    #[cfg(target_family = "wasm")]
    fn start_github_login_flow(&mut self, cx: &mut Context<Self>) {
        self.github_login = Some(GitHubLoginState::Error(
            "GitHub sign-in isn't available in the browser client yet.".to_owned(),
        ));
        cx.notify();
    }

    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn handle_github_device_code(
        &mut self,
        result: Result<GitHubDeviceCode, LoomError>,
        cx: &mut Context<Self>,
    ) {
        let device = match result {
            Ok(device) => device,
            Err(error) => {
                self.github_login = Some(GitHubLoginState::Error(error.message));
                cx.notify();
                return;
            }
        };
        self.github_login = Some(GitHubLoginState::Awaiting {
            verification_uri: device.verification_uri.clone(),
            user_code: device.user_code.clone(),
            expires_in: device.expires_in,
        });
        cx.notify();
        let task =
            cx.background_spawn(async move { GitHubCopilotAuthenticator::default().poll(&device) });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            view.update(cx, |view, cx| view.finish_github_login(result, cx))
                .ok();
        })
        .detach();
    }

    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn finish_github_login(
        &mut self,
        result: Result<String, LoomError>,
        cx: &mut Context<Self>,
    ) {
        self.github_login = Some(GitHubLoginState::Completing);
        let token = match result {
            Ok(token) => token,
            Err(error) => {
                self.github_login = Some(GitHubLoginState::Error(error.message));
                cx.notify();
                return;
            }
        };
        match FileCredentialStore::open(FileCredentialStore::default_path())
            .and_then(|credentials| credentials.insert(GITHUB_COPILOT_CREDENTIAL_REF, token))
        {
            Ok(()) => {}
            Err(error) => {
                self.github_login = Some(GitHubLoginState::Error(error.message));
                cx.notify();
                return;
            }
        };
        if self.model.as_str() == "deterministic/demo" {
            self.model = ModelId::new(GITHUB_COPILOT_DEFAULT_MODEL);
        }
        self.refresh_models_async(cx);
        self.github_login = Some(GitHubLoginState::Success);
        self.github_connected = true;
        self.record_status("GitHub Copilot login succeeded");
        cx.notify();
    }

    pub(crate) fn toggle_model_picker(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.model_picker_open = !self.model_picker_open;
        cx.notify();
    }

    pub(crate) fn refresh_models_button(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.refresh_models_async(cx);
        cx.notify();
    }

    pub(crate) fn select_model(&mut self, model: ModelId, cx: &mut Context<Self>) {
        self.session_models
            .insert(self.active_session.id, model.clone());
        self.model = model;
        self.model_picker_open = false;
        cx.notify();
    }

    pub(crate) fn toggle_agent_mode_picker(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.agent_mode_picker_open = !self.agent_mode_picker_open;
        cx.notify();
    }

    pub(crate) fn select_agent_mode(&mut self, mode: AgentMode, cx: &mut Context<Self>) {
        self.agent_mode_picker_open = false;
        let policy = mode.approval_policy();
        self.dispatch(
            cx,
            ClientRequest::SetApprovalPolicy {
                project_id: self.project_id,
                policy,
            },
            move |view, response, _| match response.result {
                Ok(ServerResponse::ApprovalPolicy(_)) => {
                    view.agent_mode = mode;
                    view.record_status(format!("{} mode enabled", mode.label()));
                }
                Err(error) => view.record_backend_error("set approval mode", error),
                Ok(response) => view.record_backend_error(
                    "set approval mode",
                    unexpected_response("approval policy", response),
                ),
            },
        );
        cx.notify();
    }

    pub(crate) fn select_default_model(&mut self, model: ModelId, cx: &mut Context<Self>) {
        self.default_model = model;
        self.settings_open = false;
        cx.notify();
    }

    pub(crate) fn open_settings_from_menu(&mut self, cx: &mut Context<Self>) {
        self.github_login = None;
        self.review.open = false;
        self.providers_open = false;
        self.settings_open = true;
        cx.notify();
    }

    pub(crate) fn open_providers_from_menu(&mut self, cx: &mut Context<Self>) {
        self.github_login = None;
        self.review.open = false;
        self.settings_open = false;
        self.providers_open = true;
        self.dispatch(
            cx,
            ClientRequest::ListProviders,
            |view, response, _| match response.result {
                Ok(ServerResponse::Providers { providers }) => {
                    view.providers = providers;
                }
                Err(error) => view.record_backend_error("list providers", error),
                Ok(response) => view.record_backend_error(
                    "list providers",
                    unexpected_response("provider list", response),
                ),
            },
        );
        cx.notify();
    }

    pub(crate) fn close_providers(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.providers_open = false;
        cx.notify();
    }

    pub(crate) fn close_settings(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings_open = false;
        cx.notify();
    }

    pub(crate) fn select_theme(
        &mut self,
        theme: ThemeChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let ThemeChoice::Preset(name, _) = theme {
            let appearance = window.appearance();
            cx.set_window_appearance(Some(appearance));
            if let Err(error) = crate::theme::apply_preset_theme(name, appearance, cx) {
                self.record_status(format!("could not load theme: {error}"));
                return;
            }
            self.theme_choice = theme;
            cx.notify();
            return;
        }

        self.theme_choice = theme;
        let appearance = match theme {
            ThemeChoice::System => {
                cx.set_window_appearance(None);
                window.appearance()
            }
            ThemeChoice::Light => {
                cx.set_window_appearance(Some(WindowAppearance::Light));
                WindowAppearance::Light
            }
            ThemeChoice::Dark => {
                cx.set_window_appearance(Some(WindowAppearance::Dark));
                WindowAppearance::Dark
            }
            ThemeChoice::Preset(_, _) => unreachable!("preset themes return early"),
        };
        self.apply_appearance(appearance, window, cx);
    }

    pub(crate) fn observe_system_appearance(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.appearance_subscription.is_none() {
            self.appearance_subscription =
                Some(cx.observe_window_appearance(window, |view, window, cx| {
                    if view.theme_choice == ThemeChoice::System {
                        view.apply_appearance(window.appearance(), window, cx);
                    }
                }));
        }
    }

    fn apply_appearance(
        &mut self,
        appearance: WindowAppearance,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let preset = match appearance {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => "catppuccin-mocha",
            WindowAppearance::Light | WindowAppearance::VibrantLight => "catppuccin-latte",
        };
        if let Err(error) = crate::theme::apply_preset_theme(preset, appearance, cx) {
            log::warn!("could not apply the default theme: {error}");
            gpui_component::Theme::change(appearance, None, cx);
            crate::theme::sync_palette(cx);
        }
        cx.notify();
    }

    pub(crate) fn new_session(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let name = format!("Session {}", self.sessions.len().saturating_add(1));
        self.create_session_async(name, cx);
    }

    /// Creates a session through the connection worker and selects it.
    pub(crate) fn create_session_async(&mut self, name: String, cx: &mut Context<Self>) {
        self.dispatch(
            cx,
            ClientRequest::CreateAgentSession {
                project_id: self.project_id,
                name,
            },
            |view, response, cx| match response.result {
                Ok(ServerResponse::AgentSessionCreated(snapshot)) => {
                    view.sessions.push(snapshot.clone());
                    view.select_session(snapshot, cx);
                }
                Err(error) => view.record_backend_error("create session", error),
                Ok(response) => view.record_backend_error(
                    "create session",
                    unexpected_response("session creation", response),
                ),
            },
        );
    }

    pub(crate) fn begin_session_rename(
        &mut self,
        session: AgentSessionSnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_session(session, cx);
        self.rename_dialog = Some(RenameDialogState {
            session: self.active_session.clone(),
            input: TextBufferState::new(self.active_session.name.clone()),
        });
        self.rename_focus_handle.focus(window, cx);
    }

    pub(crate) fn toggle_changes_sidebar(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.review.panel = ReviewPanel::Changes;
        self.review.open = !self.review.open;
        self.refresh_review(cx);
        cx.notify();
    }

    pub(crate) fn show_review(
        &mut self,
        _panel: ReviewPanel,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings_open = false;
        self.providers_open = false;
        self.github_login = None;
        self.toggle_changes_sidebar(event, window, cx);
    }

    pub(crate) fn close_review(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.review.open = false;
        cx.notify();
    }

    pub(crate) fn open_review_file(&mut self, path: String, cx: &mut Context<Self>) {
        self.dispatch(
            cx,
            ClientRequest::ReadWorkspaceFile {
                project_id: self.project_id,
                path,
            },
            |view, response, _| match response.result {
                Ok(ServerResponse::WorkspaceFile(mut file)) => {
                    file.content = bounded_to(&file.content, MAX_REVIEW_DIFF);
                    view.review.diff_path = Some(file.path.clone());
                    view.review.selected_file = Some(file);
                    view.review.open = true;
                    view.review.panel = ReviewPanel::Changes;
                }
                Err(error) => view.record_backend_error("read review file", error),
                Ok(response) => view.record_backend_error(
                    "read review file",
                    unexpected_response("workspace file", response),
                ),
            },
        );
    }

    pub(crate) fn render_session_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = div().flex().flex_col().gap_1();
        let view = cx.entity();
        for (index, session) in self.sessions.iter().enumerate() {
            let active = session.id == self.active_session.id;
            let session = session.clone();
            let selected_session = session.clone();
            let card = div()
                .id(("session", index))
                .relative()
                .w_full()
                .px_2()
                .py_3()
                .rounded_lg()
                .border_1()
                .border_color(if active { rgb(0x3b5d85) } else { rgb(0x242833) })
                .bg(if active { rgb(0x25334a) } else { rgb(0x1b1d24) })
                .text_color(if active { rgb(0xf3f4f6) } else { rgb(0xb7c0d0) })
                .cursor_pointer()
                .hover(|style| style.bg(if active { rgb(0x293b56) } else { rgb(0x20242c) }))
                .child(
                    div()
                        .text_sm()
                        .child(session.name.clone())
                        .when(session.state == AgentSessionState::Archived, |element| {
                            element.text_color(rgb(0x64748b))
                        }),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(state_color(session.state))
                        .child(session_state_label(session.state)),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.select_session(selected_session.clone(), cx);
                }));
            let rename_session = session.clone();
            let archive_session = session.clone();
            let context_view = view.clone();
            list = list.child(card.context_menu(move |menu, _, _| {
                let rename_view = context_view.clone();
                let archive_view = context_view.clone();
                let rename_session = rename_session.clone();
                let archive_session = archive_session.clone();
                menu.item(PopupMenuItem::new("Rename").on_click(move |_, window, cx| {
                    let rename_session = rename_session.clone();
                    rename_view.update(cx, |view, cx| {
                        view.begin_session_rename(rename_session, window, cx);
                    });
                }))
                .item(PopupMenuItem::new("Archive").on_click(move |_, _, _cx| {
                    let archive_session = archive_session.clone();
                    archive_view.update(_cx, |view, cx| {
                        view.select_session(archive_session, cx);
                        view.archive_active(cx);
                    });
                }))
            }));
        }
        if self.sessions.is_empty() {
            list = list.child(
                div()
                    .p_2()
                    .text_sm()
                    .text_color(rgb(0x8f98a6))
                    .child("No sessions"),
            );
        }
        list
    }

    pub(crate) fn render_model_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut picker = div()
            .w_full()
            .p_2()
            .flex()
            .flex_col()
            .gap_1()
            .bg(rgb(0x20242c))
            .border_1()
            .border_color(rgb(0x3b4555));
        for (index, model) in self.models.iter().enumerate() {
            let model = model.clone();
            let active = model == self.model;
            picker = picker.child(
                div()
                    .id(("model-option", index))
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(if active { rgb(0x293244) } else { rgb(0x1b1d24) })
                    .text_sm()
                    .text_color(if active { rgb(0xe5e7eb) } else { rgb(0xb7c0d0) })
                    .cursor_pointer()
                    .child(model.as_str().to_owned())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_model(model.clone(), cx);
                    })),
            );
        }
        if self.models.is_empty() {
            picker = picker.child(
                div()
                    .text_sm()
                    .text_color(rgb(0xfca5a5))
                    .child("No model is configured"),
            );
        }
        picker
    }

    pub(crate) fn render_agent_mode_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut picker = div()
            .absolute()
            .bottom(px(34.))
            .left(px(8.))
            .w(px(140.))
            .p_1()
            .flex()
            .flex_col()
            .gap_1()
            .rounded_sm()
            .bg(rgb(0x20242c))
            .border_1()
            .border_color(rgb(0x3b4555))
            .shadow_lg();
        for (index, mode) in AgentMode::ALL.into_iter().enumerate() {
            let active = mode == self.agent_mode;
            picker = picker.child(
                div()
                    .id(("agent-mode", index))
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(if active { rgb(0x293244) } else { rgb(0x20242c) })
                    .text_xs()
                    .text_color(if active { rgb(0xf3f4f6) } else { rgb(0xb7c0d0) })
                    .cursor_pointer()
                    .child(mode.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_agent_mode(mode, cx);
                    })),
            );
        }
        picker.into_any()
    }

    fn render_activity_section(
        &self,
        activities: &[AgentActivityRecord],
        index: usize,
        parent: &Entity<LoomView>,
    ) -> gpui::AnyElement {
        let model = activities.iter().find_map(|activity| match &activity.data {
            AgentActivityData::ModelTurn { model } => Some(model.as_str().to_owned()),
            _ => None,
        });
        let turn = activities
            .iter()
            .find(|activity| matches!(activity.data, AgentActivityData::ModelTurn { .. }));
        let title = activity_turn_title(activities);
        let turn_status = turn.map(|activity| {
            let duration = activity
                .elapsed_ms
                .map_or_else(String::new, format_duration);
            if duration.is_empty() {
                activity_status_label(activity.status).to_owned()
            } else {
                format!("{} · {duration}", activity_status_label(activity.status))
            }
        });
        let mut section = div()
            .id(("activity-section", index))
            .mx_2()
            .my_1()
            .pl_3()
            .border_l_1()
            .border_color(rgb(0x3b4555))
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .text_xs()
                    .text_color(rgb(0x93c5fd))
                    .child("●")
                    .child(title)
                    .when_some(model, |element, model| {
                        element
                            .child("·")
                            .child(div().text_color(rgb(0x64748b)).child(model))
                    })
                    .when_some(turn_status, |element, status| {
                        element
                            .child("·")
                            .child(div().text_color(rgb(0x94a3b8)).child(status))
                    }),
            );
        for (activity_index, activity) in activities.iter().enumerate() {
            if matches!(activity.data, AgentActivityData::ModelTurn { .. }) {
                continue;
            }
            let (label, detail) = activity_label(activity);
            let duration = activity.elapsed_ms.map(format_duration);
            let output = activity_output(activity);
            let expanded = self.expanded_activities.contains(&activity.id);
            let status_color = match activity.status {
                AgentActivityStatus::Failed => rgb(0xfca5a5),
                AgentActivityStatus::Completed => rgb(0x9ad7bd),
                AgentActivityStatus::AwaitingApproval | AgentActivityStatus::AwaitingInput => {
                    rgb(0xfef3c7)
                }
                AgentActivityStatus::Started => rgb(0x93c5fd),
                AgentActivityStatus::Cancelled => rgb(0x94a3b8),
            };
            let activity_id = activity.id;
            let parent_for_toggle = parent.clone();
            let mut row = div()
                .id(("activity", ((index as u64) << 32) | activity_index as u64))
                .flex()
                .flex_col()
                .cursor_pointer()
                .text_xs()
                .text_color(rgb(0xcbd5e1))
                .on_click(move |_, _, cx| {
                    parent_for_toggle.update(cx, |this, cx| this.toggle_activity(activity_id, cx));
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .w(px(12.))
                                .text_color(status_color)
                                .child(activity_marker(activity.status)),
                        )
                        .child(if expanded { "⌄" } else { "›" })
                        .child(label)
                        .flex_1()
                        .child(
                            div()
                                .text_color(status_color)
                                .child(activity_status_label(activity.status)),
                        )
                        .when_some(duration, |element, duration| {
                            element
                                .text_color(rgb(0x64748b))
                                .child(format!(" · {duration}"))
                        }),
                );
            if expanded {
                if let Some(detail) = detail {
                    row = row.child(
                        div()
                            .ml(px(26.))
                            .max_w(px(560.))
                            .text_color(rgb(0x94a3b8))
                            .child(detail),
                    );
                }
                if let Some(output) = output {
                    row = row.child(
                        div()
                            .ml(px(26.))
                            .max_w(px(560.))
                            .border_l_1()
                            .border_color(rgb(0x30343f))
                            .pl_2()
                            .text_color(rgb(0x8f98a6))
                            .child(bounded_to(output, 420)),
                    );
                }
            }
            if activity.status == AgentActivityStatus::AwaitingApproval
                && self.pending_approval.is_some()
                && !self.approval_request_in_flight
            {
                let parent_for_approve = parent.clone();
                let parent_for_reject = parent.clone();
                row = row.child(
                    div()
                        .ml(px(26.))
                        .mt_1()
                        .flex()
                        .gap_2()
                        .child(
                            div()
                                .id((
                                    "approve-activity",
                                    ((index as u64) << 32) | activity_index as u64,
                                ))
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .bg(rgb(0x24543d))
                                .text_color(rgb(0xbbf7d0))
                                .cursor_pointer()
                                .child("Approve")
                                .on_click(move |_, _, cx| {
                                    parent_for_approve
                                        .update(cx, |this, cx| this.approve_pending_action(cx));
                                }),
                        )
                        .child(
                            div()
                                .id((
                                    "reject-activity",
                                    ((index as u64) << 32) | activity_index as u64,
                                ))
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .bg(rgb(0x542936))
                                .text_color(rgb(0xfecdd3))
                                .cursor_pointer()
                                .child("Reject")
                                .on_click(move |_, _, cx| {
                                    parent_for_reject
                                        .update(cx, |this, cx| this.reject_pending_action(cx));
                                }),
                        ),
                );
            } else if activity.status == AgentActivityStatus::AwaitingApproval
                && self.approval_request_in_flight
            {
                row = row.child(
                    div()
                        .ml(px(26.))
                        .mt_1()
                        .text_color(rgb(0x94a3b8))
                        .child("Submitting approval..."),
                );
            }
            section = section.child(row);
        }
        section.into_any()
    }

    fn toggle_activity(&mut self, activity_id: ActivityId, cx: &mut Context<Self>) {
        if !self.expanded_activities.remove(&activity_id) {
            self.expanded_activities.insert(activity_id);
        }
        cx.notify();
    }

    pub(crate) fn render_timeline_item(
        &self,
        item: &TimelineItem,
        index: usize,
        parent: &Entity<LoomView>,
    ) -> gpui::AnyElement {
        let user_background = rgb(0x20242c);
        let user_foreground = rgb(0xdbeafe);
        match item {
            TimelineItem::User(text) => div()
                .w_full()
                .flex()
                .justify_end()
                .child(
                    div()
                        .w_full()
                        .max_w(px(520.))
                        .p_3()
                        .rounded_lg()
                        .bg(user_background)
                        .text_color(user_foreground)
                        .child(div().text_xs().text_color(user_foreground).child("You"))
                        .child(render_timeline_text(
                            format!("transcript-user-{index}"),
                            text.clone(),
                            0xdbeafe,
                        )),
                )
                .into_any(),
            TimelineItem::Assistant(text) => div()
                .px_3()
                .py_2()
                .text_color(rgb(0xf3f4f6))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x8f98a6))
                        .child("GitHub Copilot"),
                )
                .child(render_timeline_text(
                    format!("transcript-assistant-{index}"),
                    text.clone(),
                    0xf3f4f6,
                ))
                .into_any(),
            TimelineItem::ActivitySection { activities } => {
                self.render_activity_section(activities, index, parent)
            }
            TimelineItem::Plan {
                steps,
                completed,
                active,
            } => {
                let mut card = div()
                    .px_3()
                    .py_2()
                    .text_color(rgb(0xb7c0d0))
                    .child(div().text_xs().text_color(rgb(0x8f98a6)).child("Plan"));
                for (index, step) in steps.iter().enumerate() {
                    let index = index as u32;
                    let marker = if completed.contains(&index) {
                        "✓"
                    } else if active == &Some(index) {
                        ">"
                    } else {
                        "○"
                    };
                    card = card.child(
                        div()
                            .text_xs()
                            .text_color(if completed.contains(&index) {
                                rgb(0x9ad7bd)
                            } else {
                                rgb(0xb7c0d0)
                            })
                            .child(format!("{marker} {}", step)),
                    );
                }
                card.into_any()
            }
            TimelineItem::ToolRequested { name, arguments } => div()
                .px_3()
                .py_1()
                .text_color(rgb(0xcbd5e1))
                .child(format!(">_  {name}"))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x8f98a6))
                        .child(arguments.clone()),
                )
                .into_any(),
            TimelineItem::Approval { name, active } => {
                let mut row = div()
                    .px_3()
                    .py_1()
                    .text_color(if *active {
                        rgb(0xfef3c7)
                    } else {
                        rgb(0x94a3b8)
                    })
                    .child(if *active {
                        format!("Approval required  >_ {name}")
                    } else {
                        format!("Approval resolved  >_ {name}")
                    });
                if *active && self.pending_approval.is_some() && !self.approval_request_in_flight {
                    let parent_for_approve = parent.clone();
                    let parent_for_reject = parent.clone();
                    row = row.child(
                        div()
                            .mt_1()
                            .flex()
                            .gap_2()
                            .child(
                                div()
                                    .id(("approve-legacy", index))
                                    .px_2()
                                    .py_1()
                                    .rounded_sm()
                                    .bg(rgb(0x24543d))
                                    .text_color(rgb(0xbbf7d0))
                                    .cursor_pointer()
                                    .child("Approve")
                                    .on_click(move |_, _, cx| {
                                        parent_for_approve
                                            .update(cx, |this, cx| this.approve_pending_action(cx));
                                    }),
                            )
                            .child(
                                div()
                                    .id(("reject-legacy", index))
                                    .px_2()
                                    .py_1()
                                    .rounded_sm()
                                    .bg(rgb(0x542936))
                                    .text_color(rgb(0xfecdd3))
                                    .cursor_pointer()
                                    .child("Reject")
                                    .on_click(move |_, _, cx| {
                                        parent_for_reject
                                            .update(cx, |this, cx| this.reject_pending_action(cx));
                                    }),
                            ),
                    );
                } else if *active && self.approval_request_in_flight {
                    row = row.child(
                        div()
                            .mt_1()
                            .text_color(rgb(0x94a3b8))
                            .child("Submitting approval..."),
                    );
                }
                row.into_any()
            }
            TimelineItem::ToolStarted(name) => div()
                .px_3()
                .py_1()
                .text_xs()
                .text_color(rgb(0xcbd5e1))
                .child(format!(">_  {name}"))
                .into_any(),
            TimelineItem::ToolOutput(output) => div()
                .mx_3()
                .px_2()
                .py_1()
                .border_l_1()
                .border_color(rgb(0x30343f))
                .text_xs()
                .text_color(rgb(0x94a3b8))
                .child(output.clone())
                .into_any(),
            TimelineItem::ToolCompleted { name, success } => div()
                .px_3()
                .py_1()
                .text_xs()
                .text_color(if *success {
                    rgb(0x9ad7bd)
                } else {
                    rgb(0xfca5a5)
                })
                .child(format!(
                    ">_  {name} [{}]",
                    if *success { "ok" } else { "failed" }
                ))
                .into_any(),
            TimelineItem::Status(status) => div()
                .px_3()
                .py_1()
                .text_xs()
                .text_color(rgb(0x94a3b8))
                .child(status.clone())
                .into_any(),
            TimelineItem::Error { operation, error } => div()
                .p_2()
                .rounded_lg()
                .bg(rgb(0x3a1f24))
                .text_sm()
                .text_color(rgb(0xfca5a5))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0xfda4af))
                        .child(format!("{} · {}", operation, error.code)),
                )
                .child(div().mt_1().child(render_timeline_text(
                    format!("timeline-error-{index}"),
                    error.message.clone(),
                    0xfca5a5,
                )))
                .when(error.retryable, |element| {
                    element.child(
                        div()
                            .mt_1()
                            .text_xs()
                            .text_color(rgb(0xfda4af))
                            .child("This operation can be retried."),
                    )
                })
                .into_any(),
            TimelineItem::NeedsInput(prompt) => div()
                .p_2()
                .rounded_sm()
                .bg(rgb(0x3b2f66))
                .text_color(rgb(0xe9d5ff))
                .child(div().text_xs().child("Agent needs input"))
                .child(render_timeline_text(
                    format!("timeline-input-{index}"),
                    prompt.clone(),
                    0xe9d5ff,
                ))
                .into_any(),
            TimelineItem::Summary { text, evidence } => {
                let mut card = div()
                    .px_3()
                    .py_2()
                    .text_color(rgb(0xf3f4f6))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(0x8f98a6))
                            .child("GitHub Copilot"),
                    )
                    .child(text.clone());
                for link in evidence {
                    card = card.child(
                        div()
                            .text_xs()
                            .text_color(rgb(0x9ad7bd))
                            .child(format!("Evidence: {link}")),
                    );
                }
                card.into_any()
            }
        }
    }

    fn timeline_entity(&mut self, cx: &mut Context<Self>) -> Entity<TimelineView> {
        if let Some(timeline_view) = &self.timeline_view {
            return timeline_view.clone();
        }

        let parent = cx.entity();
        let timeline_view = cx.new(|_| TimelineView::new(parent));
        self.timeline_view = Some(timeline_view.clone());
        timeline_view
    }

    pub(crate) fn render_review(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let title = match self.review.panel {
            ReviewPanel::Changes => "Changed files",
            ReviewPanel::Diff => "Diff",
            ReviewPanel::Evidence => "Evidence",
        };
        let mut body = div()
            .flex_1()
            .id("changes-sidebar-scroll")
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_1()
            .p_2();
        match self.review.panel {
            ReviewPanel::Changes => {
                if self.review.changes.is_empty() {
                    body = body.child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x8f98a6))
                            .child("No workspace changes recorded"),
                    );
                }
                for (index, change) in self.review.changes.iter().take(24).enumerate() {
                    let path = change.path.clone();
                    body = body.child(
                        div()
                            .id(("review-file", index))
                            .text_sm()
                            .text_color(change_color(change.kind))
                            .cursor_pointer()
                            .child(format!(
                                "{}  {}",
                                change_kind_label(change.kind),
                                change.path
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_review_file(path.clone(), cx);
                                cx.notify();
                            })),
                    );
                }
                if let Some(status) = &self.review.vcs {
                    for (index, file) in status.files.iter().take(24).enumerate() {
                        let path = file.path.clone();
                        body = body.child(
                            div()
                                .id(("git-file", index))
                                .text_sm()
                                .text_color(rgb(0xfef3c7))
                                .cursor_pointer()
                                .child(format!("Git · {:?}  {}", file.worktree, file.path))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_review_file(path.clone(), cx);
                                    cx.notify();
                                })),
                        );
                    }
                }
                if let Some(file) = &self.review.selected_file {
                    body = body.child(
                        div()
                            .mt_1()
                            .text_xs()
                            .text_color(rgb(0x93c5fd))
                            .child(format!("READ-ONLY FILE  {}", file.path)),
                    );
                    body = body.child(
                        div()
                            .p_2()
                            .bg(rgb(0x0f1115))
                            .text_xs()
                            .text_color(rgb(0xcbd5e1))
                            .child(file.content.clone()),
                    );
                }
            }
            ReviewPanel::Diff => {
                body = body.child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x8f98a6))
                        .child("A read-only view of the changes in this session."),
                );
                body = body.child(
                    div()
                        .mt_1()
                        .p_2()
                        .bg(rgb(0x0f1115))
                        .text_xs()
                        .text_color(rgb(0xcbd5e1))
                        .child(
                            self.review
                                .diff
                                .as_ref()
                                .map(|diff| diff.patch.clone())
                                .unwrap_or_else(|| "No diff available".to_owned()),
                        ),
                );
            }
            ReviewPanel::Evidence => {
                body = body.child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x8f98a6))
                        .child("What was completed and how it was checked."),
                );
                if let Some(summary) = &self.summary {
                    body = body.child(
                        div()
                            .p_2()
                            .rounded_sm()
                            .bg(rgb(0x064e3b))
                            .text_sm()
                            .text_color(rgb(0xd1fae5))
                            .child(summary.clone()),
                    );
                }
                if self.tasks.is_empty() {
                    body = body.child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x8f98a6))
                            .child("No validation tasks have been run"),
                    );
                }
                for task in self.tasks.iter().take(6) {
                    let status_color = match task.status {
                        TaskStatus::Completed => rgb(0x9ad7bd),
                        TaskStatus::Failed | TaskStatus::Cancelled => rgb(0xfca5a5),
                        TaskStatus::Queued | TaskStatus::Running => rgb(0xfef3c7),
                    };
                    body = body.child(
                        div()
                            .p_2()
                            .rounded_sm()
                            .bg(rgb(0x20242c))
                            .text_sm()
                            .text_color(status_color)
                            .child(format!("{}  {:?}", task.label, task.status))
                            .child(
                                div()
                                    .mt_1()
                                    .text_xs()
                                    .text_color(rgb(0x94a3b8))
                                    .child(bounded(&task.output)),
                            ),
                    );
                    for artifact in task.artifacts.iter().take(3) {
                        body =
                            body.child(div().text_xs().text_color(rgb(0x9ad7bd)).child(format!(
                                "Artifact  {}{}",
                                artifact.path,
                                if artifact.exists { "" } else { " (missing)" }
                            )));
                    }
                }
                if self.review.evidence.is_empty() {
                    body = body.child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x8f98a6))
                            .child("No evidence links attached"),
                    );
                }
                for evidence in &self.review.evidence {
                    body = body.child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x9ad7bd))
                            .child(evidence.clone()),
                    );
                }
            }
        }
        div()
            .w(px(340.))
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(0x17191f))
            .border_l_1()
            .border_color(rgb(0x30343f))
            .child(
                div()
                    .w_full()
                    .px_2()
                    .py_2()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(rgb(0x293244))
                    .child(div().text_xs().text_color(rgb(0x93c5fd)).child(title))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div().text_xs().text_color(rgb(0x8f98a6)).child(
                                    self.review
                                        .vcs
                                        .as_ref()
                                        .map(|status| {
                                            format!(
                                                "{}  {}",
                                                status.branch.as_deref().unwrap_or("detached"),
                                                if status.clean { "clean" } else { "modified" }
                                            )
                                        })
                                        .unwrap_or_else(|| "VCS unavailable".to_owned()),
                                ),
                            )
                            .child(
                                Button::new("close-review")
                                    .icon(Icon::new(IconName::FileText))
                                    .ghost()
                                    .xsmall()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.review.panel = ReviewPanel::Changes;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("toggle-review-sidebar-close")
                                    .icon(Icon::new(IconName::PanelRight))
                                    .ghost()
                                    .xsmall()
                                    .on_click(cx.listener(Self::close_review)),
                            ),
                    ),
            )
            .child(body)
    }

    pub(crate) fn render_composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let placeholder = if self.pending_input.is_some() {
            "Answer the agent question..."
        } else if self.sending_message {
            "Sending direction..."
        } else if self.active_run_id.is_some() {
            "Send follow-up direction..."
        } else {
            "Describe a task..."
        };
        div()
            .w_full()
            .p_3()
            .bg(rgb(0x17191f))
            .border_t_1()
            .border_color(rgb(0x30343f))
            .child(
                div()
                    .w_full()
                    .min_h(px(46.))
                    .p_3()
                    .rounded_lg()
                    .bg(rgb(0x10141b))
                    .border_1()
                    .border_color(rgb(0x3b4555))
                    .text_color(rgb(0xe5e7eb))
                    .key_context("Composer")
                    .track_focus(&self.composer_focus_handle)
                    .cursor(CursorStyle::IBeam)
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::focus_composer))
                    .on_action(cx.listener(Self::backspace))
                    .on_action(cx.listener(Self::delete))
                    .on_action(cx.listener(Self::left))
                    .on_action(cx.listener(Self::right))
                    .on_action(cx.listener(Self::select_all))
                    .on_action(cx.listener(Self::home))
                    .on_action(cx.listener(Self::end))
                    .on_action(cx.listener(Self::paste))
                    .on_action(cx.listener(Self::copy))
                    .on_action(cx.listener(Self::submit))
                    .child(TextInputElement {
                        view: cx.entity(),
                        field: InputField::Composer,
                    })
                    .when(self.composer.text.is_empty(), |element| {
                        element.child(div().text_sm().text_color(rgb(0x64748b)).child(placeholder))
                    }),
            )
            .child(
                div().mt_2().flex().items_center().justify_between().child(
                    div()
                        .flex()
                        .relative()
                        .items_center()
                        .gap_1()
                        .child(
                            div()
                                .id("composer-agent-mode")
                                .px_2()
                                .py_1()
                                .rounded_lg()
                                .bg(rgb(0x20242c))
                                .hover(|style| style.bg(rgb(0x293244)))
                                .text_xs()
                                .text_color(rgb(0xb7c0d0))
                                .cursor_pointer()
                                .child(self.agent_mode.label())
                                .on_click(cx.listener(Self::toggle_agent_mode_picker)),
                        )
                        .when(self.agent_mode_picker_open, |element| {
                            element.child(self.render_agent_mode_picker(cx))
                        })
                        .child(
                            div()
                                .id("composer-model-picker")
                                .px_2()
                                .py_1()
                                .rounded_lg()
                                .bg(rgb(0x20242c))
                                .hover(|style| style.bg(rgb(0x293244)))
                                .text_xs()
                                .text_color(rgb(0xb7c0d0))
                                .cursor_pointer()
                                .child(format!("Model  {}", self.model.as_str()))
                                .on_click(cx.listener(Self::toggle_model_picker)),
                        )
                        .when(self.model_picker_open, |element| {
                            element.child(self.render_model_picker(cx))
                        })
                        .when(self.sending_message, |element| {
                            element
                                .child(div().text_xs().text_color(rgb(0x64748b)).child("Working…"))
                        }),
                ),
            )
    }

    pub(crate) fn render_rename_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(dialog) = &self.rename_dialog else {
            return div().into_any();
        };
        div()
            .id("rename-dialog")
            .absolute()
            .top(px(120.))
            .left(px(280.))
            .w(px(420.))
            .p_3()
            .rounded_lg()
            .bg(rgb(0x1b1d24))
            .border_1()
            .border_color(rgb(0x3b4555))
            .shadow_lg()
            .text_color(rgb(0xe5e7eb))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0xf3f4f6))
                    .child("Rename session"),
            )
            .child(
                div()
                    .mt_1()
                    .text_xs()
                    .text_color(rgb(0x8f98a6))
                    .child(format!("Current name: {}", dialog.session.name)),
            )
            .child(
                div()
                    .mt_3()
                    .w_full()
                    .min_h(px(34.))
                    .p_2()
                    .rounded_sm()
                    .bg(rgb(0x0f1115))
                    .border_1()
                    .border_color(rgb(0x3b4555))
                    .text_color(rgb(0xe5e7eb))
                    .key_context("RenameDialog")
                    .track_focus(&self.rename_focus_handle)
                    .cursor(CursorStyle::IBeam)
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::focus_rename))
                    .on_action(cx.listener(Self::backspace))
                    .on_action(cx.listener(Self::delete))
                    .on_action(cx.listener(Self::left))
                    .on_action(cx.listener(Self::right))
                    .on_action(cx.listener(Self::select_all))
                    .on_action(cx.listener(Self::home))
                    .on_action(cx.listener(Self::end))
                    .on_action(cx.listener(Self::paste))
                    .on_action(cx.listener(Self::copy))
                    .on_action(cx.listener(Self::submit))
                    .child(TextInputElement {
                        view: cx.entity(),
                        field: InputField::Rename,
                    }),
            )
            .child(
                div()
                    .mt_3()
                    .flex()
                    .justify_end()
                    .gap_1()
                    .child(
                        div()
                            .id("cancel-rename")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .bg(rgb(0x242833))
                            .hover(|style| style.bg(rgb(0x293244)))
                            .text_sm()
                            .cursor_pointer()
                            .child("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.rename_dialog = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("confirm-rename")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .bg(rgb(0x2563eb))
                            .text_sm()
                            .text_color(rgb(0xffffff))
                            .cursor_pointer()
                            .child("Rename")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.confirm_rename(cx);
                                cx.notify();
                            })),
                    ),
            )
            .into_any()
    }

    pub(crate) fn render_github_login_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(state) = &self.github_login else {
            return div().into_any();
        };
        let body = match state {
            GitHubLoginState::Starting => div()
                .text_sm()
                .text_color(rgb(0x8f98a6))
                .child("Requesting a GitHub device code..."),
            GitHubLoginState::Awaiting {
                verification_uri,
                user_code,
                expires_in,
            } => div()
                .text_sm()
                .text_color(rgb(0xe5e7eb))
                .child("Open this URL in a browser:")
                .child(
                    div()
                        .mt_2()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .text_color(rgb(0x93c5fd))
                                .child(verification_uri.clone()),
                        )
                        .child(
                            div()
                                .id("open-github-verification-url")
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .bg(rgb(0x2563eb))
                                .hover(|style| style.bg(rgb(0x1d4ed8)))
                                .text_xs()
                                .text_color(rgb(0xffffff))
                                .cursor_pointer()
                                .child("Open")
                                .on_click({
                                    let verification_uri = verification_uri.clone();
                                    cx.listener(move |this, _, _, cx| {
                                        if let Err(error) = open_external_url(&verification_uri) {
                                            this.record_status(format!(
                                                "Could not open GitHub URL: {error}"
                                            ));
                                        }
                                        cx.notify();
                                    })
                                }),
                        )
                        .child(
                            div()
                                .id("copy-github-verification-url")
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .bg(rgb(0x20242c))
                                .hover(|style| style.bg(rgb(0x293244)))
                                .text_xs()
                                .cursor_pointer()
                                .child("Copy")
                                .on_click({
                                    let verification_uri = verification_uri.clone();
                                    cx.listener(move |this, _, _, cx| {
                                        this.copy_github_login_value(
                                            verification_uri.clone(),
                                            "verification URL",
                                            cx,
                                        );
                                    })
                                }),
                        ),
                )
                .child(
                    div()
                        .mt_3()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .text_color(rgb(0xfef3c7))
                                .child(format!("Enter code: {user_code}")),
                        )
                        .child(
                            div()
                                .id("copy-github-user-code")
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .bg(rgb(0x20242c))
                                .hover(|style| style.bg(rgb(0x293244)))
                                .text_xs()
                                .cursor_pointer()
                                .child("Copy code")
                                .on_click({
                                    let user_code = user_code.clone();
                                    cx.listener(move |this, _, _, cx| {
                                        this.copy_github_login_value(
                                            user_code.clone(),
                                            "device code",
                                            cx,
                                        );
                                    })
                                }),
                        ),
                )
                .child(
                    div()
                        .mt_1()
                        .text_xs()
                        .text_color(rgb(0x8f98a6))
                        .child(format!(
                            "Waiting for authorization (expires in {expires_in}s)"
                        )),
                ),
            GitHubLoginState::Completing => div()
                .text_sm()
                .text_color(rgb(0x8f98a6))
                .child("Authorization received. Saving credentials..."),
            GitHubLoginState::Success => div()
                .text_sm()
                .text_color(rgb(0x9ad7bd))
                .child("GitHub Copilot is connected. You can now use its models."),
            GitHubLoginState::Error(error) => div()
                .text_sm()
                .text_color(rgb(0xfca5a5))
                .child(error.clone()),
        };
        div()
            .id("github-login-dialog")
            .size_full()
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .p_6()
            .flex()
            .flex_col()
            .bg(rgb(0x111318))
            .text_color(rgb(0xe5e7eb))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0xf3f4f6))
                            .child("Connect GitHub Copilot"),
                    )
                    .child(
                        div()
                            .id("close-github-login")
                            .w(px(28.))
                            .h(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_lg()
                            .bg(rgb(0x20242c))
                            .hover(|style| style.bg(rgb(0x293244)))
                            .text_color(rgb(0xb7c0d0))
                            .cursor_pointer()
                            .tooltip(|_, cx| {
                                cx.new(|_| LoomTooltip {
                                    text: "Close GitHub connection".into(),
                                })
                                .into()
                            })
                            .child(Icon::new(IconName::Close).size_4())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.github_login = None;
                                cx.notify();
                            })),
                    ),
            )
            .child(div().mt_4().child(body))
            .into_any()
    }

    pub(crate) fn render_settings_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut body = div().flex().flex_col().gap_1();
        if self.models.is_empty() {
            body = body.child(
                div()
                    .text_sm()
                    .text_color(rgb(0x8f98a6))
                    .child("No configured models are available."),
            );
        } else {
            for (index, model) in self.models.iter().enumerate() {
                let model = model.clone();
                let selected = model == self.default_model;
                body = body.child(
                    div()
                        .id(("default-model", index))
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .bg(if selected {
                            rgb(0x293244)
                        } else {
                            rgb(0x20242c)
                        })
                        .text_sm()
                        .text_color(if selected {
                            rgb(0xf3f4f6)
                        } else {
                            rgb(0xb7c0d0)
                        })
                        .cursor_pointer()
                        .child(model.as_str().to_owned())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.select_default_model(model.clone(), cx);
                        })),
                );
            }
        }
        div()
            .id("settings-dialog")
            .size_full()
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .p_6()
            .flex()
            .flex_col()
            .bg(rgb(0x111318))
            .text_color(rgb(0xe5e7eb))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_sm().text_color(rgb(0xf3f4f6)).child("Settings"))
                    .child(
                        div()
                            .id("close-settings")
                            .w(px(28.))
                            .h(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_lg()
                            .bg(rgb(0x20242c))
                            .hover(|style| style.bg(rgb(0x293244)))
                            .text_color(rgb(0xb7c0d0))
                            .cursor_pointer()
                            .tooltip(|_, cx| {
                                cx.new(|_| LoomTooltip {
                                    text: "Close settings".into(),
                                })
                                .into()
                            })
                            .child(Icon::new(IconName::Close).size_4())
                            .on_click(cx.listener(Self::close_settings)),
                    ),
            )
            .child(
                div()
                    .mt_3()
                    .text_xs()
                    .text_color(rgb(0x93c5fd))
                    .child("DEFAULT MODEL FOR NEW SESSIONS"),
            )
            .child(
                div()
                    .id("settings-refresh-models")
                    .mt_3()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(rgb(0x20242c))
                    .hover(|style| style.bg(rgb(0x293244)))
                    .text_xs()
                    .text_color(rgb(0xb7c0d0))
                    .cursor_pointer()
                    .child("Refresh available models")
                    .on_click(cx.listener(Self::refresh_models_button)),
            )
            .child(
                div()
                    .mt_5()
                    .text_xs()
                    .text_color(rgb(0x93c5fd))
                    .child("THEME"),
            )
            .child(
                div().mt_2().flex().flex_wrap().gap_1().children(
                    ThemeChoice::ALL
                        .into_iter()
                        .enumerate()
                        .map(|(index, choice)| {
                            let selected = choice == self.theme_choice;
                            div()
                                .id(("theme-choice", index))
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .bg(if selected {
                                    rgb(0x293244)
                                } else {
                                    rgb(0x20242c)
                                })
                                .text_xs()
                                .text_color(if selected {
                                    rgb(0xf3f4f6)
                                } else {
                                    rgb(0xb7c0d0)
                                })
                                .cursor_pointer()
                                .child(choice.label())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.select_theme(choice, window, cx);
                                }))
                        }),
                ),
            )
            .child(div().mt_2().child(body))
            .into_any()
    }

    pub(crate) fn render_providers_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let github_provider = self
            .providers
            .iter()
            .find(|provider| provider.kind == ProviderKind::GitHubCopilot);
        let local_providers = self
            .providers
            .iter()
            .filter(|provider| provider.kind != ProviderKind::GitHubCopilot)
            .collect::<Vec<_>>();
        let github_status = if self.github_connected {
            "Connected"
        } else {
            "Not connected"
        };
        let github_models = github_provider.map_or(0, |provider| provider.models.len());

        let mut local_body = div().flex().flex_col().gap_1();
        if local_providers.is_empty() {
            local_body = local_body.child(
                div()
                    .p_3()
                    .rounded_lg()
                    .bg(rgb(0x171c25))
                    .border_1()
                    .border_color(rgb(0x293244))
                    .text_sm()
                    .text_color(rgb(0x8f98a6))
                    .child("No local providers are configured."),
            );
        } else {
            for provider in local_providers {
                local_body =
                    local_body.child(
                        div()
                            .p_3()
                            .rounded_lg()
                            .bg(rgb(0x171c25))
                            .border_1()
                            .border_color(rgb(0x293244))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(0xf3f4f6))
                                    .child(provider.display_name.clone()),
                            )
                            .child(div().mt_1().text_xs().text_color(rgb(0x8f98a6)).child(
                                format!(
                                    "{} model{} available",
                                    provider.models.len(),
                                    if provider.models.len() == 1 { "" } else { "s" }
                                ),
                            )),
                    );
            }
        }

        let mut github_card =
            div()
                .p_3()
                .rounded_lg()
                .bg(rgb(0x171c25))
                .border_1()
                .border_color(rgb(0x293244))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(0xf3f4f6))
                                .child("GitHub Copilot"),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(if self.github_connected {
                                    rgb(0x9ad7bd)
                                } else {
                                    rgb(0xfef3c7)
                                })
                                .child(github_status),
                        ),
                )
                .child(div().mt_1().text_xs().text_color(rgb(0x8f98a6)).child(
                    if github_models == 0 {
                        "Connect your GitHub account to use Copilot models.".to_owned()
                    } else {
                        format!(
                            "{} model{} available",
                            github_models,
                            if github_models == 1 { "" } else { "s" }
                        )
                    },
                ));
        if self.login_enabled && !self.github_connected {
            github_card = github_card.child(
                div()
                    .id("connect-github-provider")
                    .mt_3()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(rgb(0x2563eb))
                    .hover(|style| style.bg(rgb(0x1d4ed8)))
                    .text_xs()
                    .text_color(rgb(0xffffff))
                    .cursor_pointer()
                    .child("Connect GitHub Copilot")
                    .on_click(cx.listener(Self::toggle_github_login)),
            );
        }

        div()
            .id("providers-dialog")
            .size_full()
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .p_6()
            .flex()
            .flex_col()
            .bg(rgb(0x111318))
            .text_color(rgb(0xe5e7eb))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_sm().text_color(rgb(0xf3f4f6)).child("Providers"))
                    .child(
                        div()
                            .id("close-providers")
                            .w(px(28.))
                            .h(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_lg()
                            .bg(rgb(0x20242c))
                            .hover(|style| style.bg(rgb(0x293244)))
                            .text_color(rgb(0xb7c0d0))
                            .cursor_pointer()
                            .tooltip(|_, cx| {
                                cx.new(|_| LoomTooltip {
                                    text: "Close providers".into(),
                                })
                                .into()
                            })
                            .child(Icon::new(IconName::Close).size_4())
                            .on_click(cx.listener(Self::close_providers)),
                    ),
            )
            .child(
                div()
                    .mt_5()
                    .text_xs()
                    .text_color(rgb(0x93c5fd))
                    .child("GITHUB COPILOT"),
            )
            .child(div().mt_2().child(github_card))
            .child(
                div()
                    .mt_5()
                    .text_xs()
                    .text_color(rgb(0x93c5fd))
                    .child("LOCAL PROVIDER"),
            )
            .child(div().mt_2().child(local_body))
            .into_any()
    }
}

impl Render for LoomView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.schedule_run_poll(cx);
        let view = cx.entity();
        let project_name = self
            .project
            .as_ref()
            .map(|project| project.name.as_str())
            .unwrap_or("Project");
        let decorations = window.window_decorations();
        let client_decorated = matches!(decorations, Decorations::Client { .. });
        let shadow_size = CLIENT_DECORATION_SHADOW;
        let mut tiling = match decorations {
            Decorations::Client { tiling } => tiling,
            Decorations::Server => Tiling::default(),
        };
        if window.is_maximized() || window.is_fullscreen() {
            tiling = Tiling::tiled();
        }
        let decoration_inset = if tiling.is_tiled() {
            px(0.)
        } else {
            shadow_size
        };
        if client_decorated {
            window.set_client_inset(shadow_size);
        }
        let content = div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(rgb(0x111318))
            .text_color(rgb(0xe5e7eb))
            .text_size(px(13.))
            .child(TextSelectionLayer)
            .child(
                div()
                    .h(px(30.))
                    .w_full()
                    .px_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .bg(rgb(0x1b1d24))
                    .border_b_1()
                    .border_color(rgb(0x30343f))
                    .rounded_client_top(client_decorated, tiling)
                    .child(
                        div()
                            .id("window-titlebar-drag")
                            .flex()
                            .flex_1()
                            .items_center()
                            .gap_2()
                            .cursor_pointer()
                            .window_control_area(WindowControlArea::Drag)
                            .on_mouse_down(MouseButton::Left, |event, window, _| {
                                if event.click_count == 2 {
                                    window.zoom_window();
                                } else {
                                    window.start_window_move();
                                }
                            })
                            .child(div().text_xs().text_color(rgb(0x8f98a6)).child(format!(
                                "{}  ·  {}",
                                project_name, self.active_session.name
                            ))),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new("account-menu")
                                    .icon(Icon::new(IconName::User))
                                    .ghost()
                                    .xsmall()
                                    .dropdown_menu({
                                        let view = view.clone();
                                        move |menu, _, _| {
                                            let providers_view = view.clone();
                                            let settings_view = view.clone();
                                            menu.item(PopupMenuItem::new("Providers").on_click(
                                                move |_, _, cx| {
                                                    providers_view.update(cx, |view, cx| {
                                                        view.open_providers_from_menu(cx);
                                                    });
                                                },
                                            ))
                                            .item(
                                                PopupMenuItem::new("Settings").on_click(
                                                    move |_, _, cx| {
                                                        settings_view.update(cx, |view, cx| {
                                                            view.open_settings_from_menu(cx);
                                                        });
                                                    },
                                                ),
                                            )
                                        }
                                    }),
                            )
                            .child(
                                div().flex().items_center().gap_1().ml_2().child(
                                    div()
                                        .id("window-close")
                                        .w(px(22.))
                                        .h(px(22.))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded_sm()
                                        .text_sm()
                                        .text_color(rgb(0xfca5a5))
                                        .hover(|style| style.bg(rgb(0x7f1d1d)))
                                        .cursor_pointer()
                                        .tooltip(|_, cx| {
                                            cx.new(|_| LoomTooltip {
                                                text: "Close window".into(),
                                            })
                                            .into()
                                        })
                                        .window_control_area(WindowControlArea::Close)
                                        .child(Icon::new(IconName::Close).size_4())
                                        .on_mouse_down(MouseButton::Left, |_, window, cx| {
                                            window.prevent_default();
                                            cx.stop_propagation();
                                        })
                                        .on_click(|_, window, cx| {
                                            cx.stop_propagation();
                                            window.remove_window();
                                        }),
                                ),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .overflow_hidden()
                    .child(
                        div()
                            .w(px(250.))
                            .h_full()
                            .relative()
                            .p_2()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .bg(rgb(0x17191f))
                            .border_r_1()
                            .border_color(rgb(0x30343f))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(rgb(0xf3f4f6))
                                            .child("Workspace"),
                                    )
                                    .child(
                                        Button::new("top-menu-button")
                                            .icon(Icon::new(IconName::Ellipsis))
                                            .ghost()
                                            .dropdown_menu({
                                                let view = view.clone();
                                                move |menu, _, _| {
                                                    menu.item(
                                                        PopupMenuItem::new("Settings").on_click({
                                                            let view = view.clone();
                                                            move |_, _, cx| {
                                                                view.update(cx, |view, cx| {
                                                                    view.open_settings_from_menu(
                                                                        cx,
                                                                    );
                                                                });
                                                            }
                                                        }),
                                                    )
                                                }
                                            }),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div().text_xs().text_color(rgb(0x8f98a6)).child("Sessions"),
                                    )
                                    .child(
                                        div()
                                            .id("new-session")
                                            .w(px(28.))
                                            .h(px(28.))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded_lg()
                                            .bg(rgb(0x202b3b))
                                            .hover(|style| style.bg(rgb(0x293b56)))
                                            .text_sm()
                                            .text_color(rgb(0x93c5fd))
                                            .cursor_pointer()
                                            .tooltip(|_, cx| {
                                                cx.new(|_| LoomTooltip {
                                                    text: "Create a new session".into(),
                                                })
                                                .into()
                                            })
                                            .child(Icon::new(IconName::Plus).size_4())
                                            .on_click(cx.listener(Self::new_session)),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .id("session-list")
                                    .overflow_y_scroll()
                                    .child(self.render_session_list(cx)),
                            )
                            .child(
                                div()
                                    .pt_2()
                                    .border_t_1()
                                    .border_color(rgb(0x30343f))
                                    .text_xs()
                                    .text_color(rgb(0x64748b))
                                    .child(format!(
                                        "{} sessions  ·  {}",
                                        self.sessions.len(),
                                        self.workspace_root.display()
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .h_full()
                            .relative()
                            .flex()
                            .flex_col()
                            .overflow_hidden()
                            .child(
                                div()
                                    .w_full()
                                    .px_3()
                                    .py_2()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .bg(rgb(0x14161a))
                                    .border_b_1()
                                    .border_color(rgb(0x30343f))
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .child(self.active_session.name.clone())
                                            .child(
                                                div().text_xs().text_color(rgb(0x8f98a6)).child(
                                                    format!(
                                                        "{}  ·  {}  ·  {} model{}",
                                                        run_state_label(self.run_state),
                                                        self.model.as_str(),
                                                        self.models.len(),
                                                        if self.models.len() == 1 {
                                                            ""
                                                        } else {
                                                            "s"
                                                        }
                                                    ),
                                                ),
                                            ),
                                    )
                                    .when(false, |element| {
                                        element.child(
                                            div()
                                                .flex()
                                                .gap_1()
                                                .child(
                                                    div()
                                                        .id("review-changes")
                                                        .w(px(28.))
                                                        .h(px(26.))
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .rounded_sm()
                                                        .bg(rgb(0x20242c))
                                                        .hover(|style| style.bg(rgb(0x293244)))
                                                        .text_sm()
                                                        .text_color(rgb(0x94a3b8))
                                                        .cursor_pointer()
                                                        .tooltip(|_, cx| {
                                                            cx.new(|_| LoomTooltip {
                                                                text: "Changed files".into(),
                                                            })
                                                            .into()
                                                        })
                                                        .child(
                                                            Icon::new(IconName::FileText).size_4(),
                                                        )
                                                        .on_click(cx.listener(
                                                            |this, event, window, cx| {
                                                                this.show_review(
                                                                    ReviewPanel::Changes,
                                                                    event,
                                                                    window,
                                                                    cx,
                                                                )
                                                            },
                                                        )),
                                                )
                                                .child(
                                                    div()
                                                        .id("review-diff")
                                                        .w(px(28.))
                                                        .h(px(26.))
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .rounded_sm()
                                                        .bg(rgb(0x20242c))
                                                        .hover(|style| style.bg(rgb(0x293244)))
                                                        .text_sm()
                                                        .text_color(rgb(0x94a3b8))
                                                        .cursor_pointer()
                                                        .tooltip(|_, cx| {
                                                            cx.new(|_| LoomTooltip {
                                                                text: "Read-only diff".into(),
                                                            })
                                                            .into()
                                                        })
                                                        .child(
                                                            Icon::new(IconName::FileText).size_4(),
                                                        )
                                                        .on_click(cx.listener(
                                                            |this, event, window, cx| {
                                                                this.show_review(
                                                                    ReviewPanel::Diff,
                                                                    event,
                                                                    window,
                                                                    cx,
                                                                )
                                                            },
                                                        )),
                                                )
                                                .child(
                                                    div()
                                                        .id("review-evidence")
                                                        .w(px(28.))
                                                        .h(px(26.))
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .rounded_sm()
                                                        .bg(rgb(0x20242c))
                                                        .hover(|style| style.bg(rgb(0x293244)))
                                                        .text_sm()
                                                        .text_color(rgb(0x94a3b8))
                                                        .cursor_pointer()
                                                        .tooltip(|_, cx| {
                                                            cx.new(|_| LoomTooltip {
                                                                text: "Task evidence".into(),
                                                            })
                                                            .into()
                                                        })
                                                        .child(Icon::new(IconName::Check).size_4())
                                                        .on_click(cx.listener(
                                                            |this, event, window, cx| {
                                                                this.show_review(
                                                                    ReviewPanel::Evidence,
                                                                    event,
                                                                    window,
                                                                    cx,
                                                                )
                                                            },
                                                        )),
                                                )
                                                .when(
                                                    self.workspace_root
                                                        .join("Cargo.toml")
                                                        .is_file(),
                                                    |element| {
                                                        element
                                                            .child(
                                                                div()
                                                                    .id("run-build")
                                                                    .w(px(28.))
                                                                    .h(px(26.))
                                                                    .flex()
                                                                    .items_center()
                                                                    .justify_center()
                                                                    .rounded_sm()
                                                                    .bg(rgb(0x20242c))
                                                                    .hover(|style| {
                                                                        style.bg(rgb(0x293244))
                                                                    })
                                                                    .text_sm()
                                                                    .text_color(rgb(0x94a3b8))
                                                                    .cursor_pointer()
                                                                    .tooltip(|_, cx| {
                                                                        cx.new(|_| LoomTooltip {
                                                                            text: "Run cargo check"
                                                                                .into(),
                                                                        })
                                                                        .into()
                                                                    })
                                                                    .child(
                                                                        Icon::new(IconName::Check)
                                                                            .size_4(),
                                                                    )
                                                                    .on_click(
                                                                        cx.listener(
                                                                            |_, _, _, _| {},
                                                                        ),
                                                                    ),
                                                            )
                                                            .child(
                                                                div()
                                                                    .id("run-tests")
                                                                    .w(px(28.))
                                                                    .h(px(26.))
                                                                    .flex()
                                                                    .items_center()
                                                                    .justify_center()
                                                                    .rounded_sm()
                                                                    .bg(rgb(0x20242c))
                                                                    .hover(|style| {
                                                                        style.bg(rgb(0x293244))
                                                                    })
                                                                    .text_sm()
                                                                    .text_color(rgb(0x94a3b8))
                                                                    .cursor_pointer()
                                                                    .tooltip(|_, cx| {
                                                                        cx.new(|_| LoomTooltip {
                                                                            text: "Run cargo test"
                                                                                .into(),
                                                                        })
                                                                        .into()
                                                                    })
                                                                    .child("T")
                                                                    .on_click(
                                                                        cx.listener(
                                                                            |_, _, _, _| {},
                                                                        ),
                                                                    ),
                                                            )
                                                            .child(
                                                                div()
                                                                    .id("run-lint")
                                                                    .w(px(28.))
                                                                    .h(px(26.))
                                                                    .flex()
                                                                    .items_center()
                                                                    .justify_center()
                                                                    .rounded_sm()
                                                                    .bg(rgb(0x20242c))
                                                                    .hover(|style| {
                                                                        style.bg(rgb(0x293244))
                                                                    })
                                                                    .text_sm()
                                                                    .text_color(rgb(0x94a3b8))
                                                                    .cursor_pointer()
                                                                    .tooltip(|_, cx| {
                                                                        cx.new(|_| LoomTooltip {
                                                                            text:
                                                                                "Run cargo clippy"
                                                                                    .into(),
                                                                        })
                                                                        .into()
                                                                    })
                                                                    .child(
                                                                        Icon::new(
                                                                            IconName::FileText,
                                                                        )
                                                                        .size_4(),
                                                                    )
                                                                    .on_click(
                                                                        cx.listener(
                                                                            |_, _, _, _| {},
                                                                        ),
                                                                    ),
                                                            )
                                                    },
                                                )
                                                .when(false, |element| element),
                                        )
                                    })
                                    .when(!self.review.open, |element| {
                                        element.child(
                                            Button::new("toggle-review-sidebar")
                                                .icon(Icon::new(IconName::PanelRight))
                                                .ghost()
                                                .xsmall()
                                                .on_click(cx.listener(
                                                    |this, event, window, cx| {
                                                        this.show_review(
                                                            ReviewPanel::Changes,
                                                            event,
                                                            window,
                                                            cx,
                                                        );
                                                    },
                                                )),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .id("timeline-scroll")
                                    .overflow_hidden()
                                    .child(self.timeline_entity(cx)),
                            )
                            .child(self.render_composer(cx))
                            .when(self.rename_dialog.is_some(), |element| {
                                element.child(self.render_rename_dialog(cx))
                            })
                            .when(self.settings_open, |element| {
                                element.child(self.render_settings_dialog(cx))
                            })
                            .when(
                                self.providers_open && self.github_login.is_none(),
                                |element| element.child(self.render_providers_dialog(cx)),
                            )
                            .when(self.github_login.is_some(), |element| {
                                element.child(self.render_github_login_dialog(cx))
                            }),
                    )
                    .when(
                        self.review.open
                            && !self.settings_open
                            && !self.providers_open
                            && self.github_login.is_none(),
                        |element| element.child(self.render_review(cx)),
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
                    .rounded_client_bottom(client_decorated, tiling)
                    .text_xs()
                    .text_color(rgb(0x8f98a6))
                    .child(format!(
                        "{}  ·  {} updates  ·  {}",
                        session_state_label(self.session_state),
                        self.timeline.len(),
                        if self.demo_workspace {
                            "Demo workspace"
                        } else {
                            "Workspace"
                        }
                    )),
            );
        let content = content.rounded_client_corners(client_decorated, tiling);
        match decorations {
            Decorations::Server => div().size_full().child(content),
            Decorations::Client { .. } => div()
                .size_full()
                .bg(transparent_black())
                .when(!tiling.top, |element| element.pt(shadow_size))
                .when(!tiling.bottom, |element| element.pb(shadow_size))
                .when(!tiling.left, |element| element.pl(shadow_size))
                .when(!tiling.right, |element| element.pr(shadow_size))
                .child(
                    div()
                        .size_full()
                        .rounded_client_corners(true, tiling)
                        .when(!tiling.is_tiled(), |element| {
                            element.border_1().border_color(rgb(0x30343f)).shadow(vec![
                                gpui::BoxShadow {
                                    color: gpui::hsla(0., 0., 0., 0.4),
                                    blur_radius: shadow_size / 2.,
                                    spread_radius: px(0.),
                                    offset: point(px(0.), px(0.)),
                                    inset: false,
                                },
                            ])
                        })
                        .child(content),
                )
                .child(
                    canvas(
                        |_bounds, window, _cx| {
                            window.insert_hitbox(
                                Bounds::new(
                                    point(px(0.), px(0.)),
                                    window.window_bounds().get_bounds().size,
                                ),
                                HitboxBehavior::Normal,
                            )
                        },
                        move |_bounds, hitbox, window, _cx| {
                            let size = window.window_bounds().get_bounds().size;
                            let Some(edge) =
                                resize_edge(window.mouse_position(), decoration_inset, size)
                            else {
                                return;
                            };
                            window.set_cursor_style(
                                match edge {
                                    ResizeEdge::Top | ResizeEdge::Bottom => {
                                        CursorStyle::ResizeUpDown
                                    }
                                    ResizeEdge::Left | ResizeEdge::Right => {
                                        CursorStyle::ResizeLeftRight
                                    }
                                    ResizeEdge::TopLeft | ResizeEdge::BottomRight => {
                                        CursorStyle::ResizeUpLeftDownRight
                                    }
                                    ResizeEdge::TopRight | ResizeEdge::BottomLeft => {
                                        CursorStyle::ResizeUpRightDownLeft
                                    }
                                },
                                &hitbox,
                            );
                        },
                    )
                    .size_full()
                    .absolute(),
                )
                .on_mouse_move(|_, window, _| window.refresh())
                .on_mouse_down(MouseButton::Left, move |event, window, _| {
                    let size = window.window_bounds().get_bounds().size;
                    if let Some(edge) = resize_edge(event.position, decoration_inset, size) {
                        window.start_window_resize(edge);
                    }
                }),
        }
    }
}
