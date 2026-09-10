use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use gpui::{
    App, Bounds, ClickEvent, ClipboardItem, Context, CursorStyle, Decorations, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Focusable, GlobalElementId,
    HitboxBehavior, KeyBinding, LayoutId, MouseButton, MouseDownEvent, PaintQuad, Pixels, Point,
    Render, ResizeEdge, ShapedLine, SharedString, Style, TextAlign, TextRun, Tiling,
    TitlebarOptions, UTF16Selection, Window, WindowAppearance, WindowBackgroundAppearance,
    WindowBounds, WindowControlArea, WindowDecorations, WindowOptions, actions, canvas, div, fill,
    point, prelude::*, px, relative, rgba, size, transparent_black,
};
use gpui_base::TextSelectionLayer;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{
    Icon, IconName, Sizable,
    menu::{ContextMenuExt, DropdownMenu, PopupMenuItem},
    text::TextView,
};
use loom_agent::{AgentEvent, AgentRunSnapshot, AgentRunState};
use loom_core::{
    AgentSessionId, AgentSessionSnapshot, AgentSessionState, Capability, CapabilitySet, ErrorCode,
    EventSequence, LoomError, ProjectId, RunId,
};
use loom_model::{MessageRole, ModelId, ProviderId, ToolCall};
use loom_process::{TaskSnapshot, TaskStatus};
use loom_protocol::{
    AgentRunSnapshotProjection, CURRENT_PROTOCOL_VERSION, ClientRequest, ProjectSnapshot,
    RequestEnvelope, ResponseEnvelope, ServerEvent, ServerResponse,
};
use loom_providers::{
    CredentialRef, CredentialStore, FileCredentialStore, GITHUB_COPILOT_CREDENTIAL_REF,
    GITHUB_COPILOT_DEFAULT_MODEL, GITHUB_COPILOT_PROVIDER_ID, GitHubCopilotAuthenticator,
    GitHubDeviceCode,
};
use loom_server::{InProcessBackend, InProcessConnection, WebSocketConnection, WebSocketTransport};
use loom_vcs::{GitDiff, GitRepositoryStatus, GitService};
use loom_workspace::{WorkspaceChange, WorkspaceFile};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const MAX_TIMELINE_OUTPUT: usize = 32 * 1024;
const MAX_REVIEW_CHANGES: usize = 80;
const MAX_REVIEW_DIFF: usize = 48 * 1024;

const CLIENT_DECORATION_ROUNDING: Pixels = px(10.);
const CLIENT_DECORATION_SHADOW: Pixels = px(10.);

/// GPUI content masks are axis-aligned rectangles, so a rounded parent cannot clip a
/// square child. Every element that paints a background into a window corner therefore
/// has to carry the corner radius itself.
trait ClientCorners: Styled + Sized {
    fn rounded_client_top(mut self, decorated: bool, tiling: Tiling) -> Self {
        if decorated && !tiling.top && !tiling.left {
            self = self.rounded_tl(CLIENT_DECORATION_ROUNDING);
        }
        if decorated && !tiling.top && !tiling.right {
            self = self.rounded_tr(CLIENT_DECORATION_ROUNDING);
        }
        self
    }

    fn rounded_client_bottom(mut self, decorated: bool, tiling: Tiling) -> Self {
        if decorated && !tiling.bottom && !tiling.left {
            self = self.rounded_bl(CLIENT_DECORATION_ROUNDING);
        }
        if decorated && !tiling.bottom && !tiling.right {
            self = self.rounded_br(CLIENT_DECORATION_ROUNDING);
        }
        self
    }

    fn rounded_client_corners(self, decorated: bool, tiling: Tiling) -> Self {
        self.rounded_client_top(decorated, tiling)
            .rounded_client_bottom(decorated, tiling)
    }
}

impl<T: Styled + Sized> ClientCorners for T {}

actions!(
    loom_composer,
    [
        Backspace, Delete, Left, Right, SelectAll, Home, End, Paste, Copy, Submit
    ]
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReviewPanel {
    Changes,
    Diff,
    Evidence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentMode {
    Ask,
    Edit,
    Agent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ThemeChoice {
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];

    const fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }
}

impl AgentMode {
    const ALL: [Self; 3] = [Self::Ask, Self::Edit, Self::Agent];

    const fn label(self) -> &'static str {
        match self {
            Self::Ask => "Ask",
            Self::Edit => "Edit",
            Self::Agent => "Agent",
        }
    }
}

#[derive(Clone, Debug)]
enum BackendStatus {
    Connected,
    Error(LoomError),
}

#[derive(Clone, Debug)]
struct ReviewState {
    open: bool,
    panel: ReviewPanel,
    changes: Vec<WorkspaceChange>,
    diff: Option<GitDiff>,
    diff_path: Option<String>,
    vcs: Option<GitRepositoryStatus>,
    evidence: Vec<String>,
    selected_file: Option<WorkspaceFile>,
}

#[derive(Clone, Debug)]
struct RenameDialogState {
    session: AgentSessionSnapshot,
    input: TextBufferState,
}

#[derive(Clone, Debug)]
enum GitHubLoginState {
    Starting,
    Awaiting {
        verification_uri: String,
        user_code: String,
        expires_in: u64,
    },
    Completing,
    Success,
    Error(String),
}

impl Default for ReviewState {
    fn default() -> Self {
        Self {
            open: false,
            panel: ReviewPanel::Changes,
            changes: Vec::new(),
            diff: None,
            diff_path: None,
            vcs: None,
            evidence: Vec::new(),
            selected_file: None,
        }
    }
}

#[derive(Clone, Debug)]
enum TimelineItem {
    User(String),
    Assistant(String),
    Plan(Vec<String>),
    StepStarted { index: u32 },
    StepCompleted { index: u32 },
    ToolRequested { name: String, arguments: String },
    Approval { name: String, active: bool },
    ToolStarted(String),
    ToolOutput(String),
    ToolCompleted { name: String, success: bool },
    Status(String),
    Error { operation: String, error: LoomError },
    NeedsInput(String),
    Summary { text: String, evidence: Vec<String> },
}

#[derive(Clone, Debug)]
struct TextBufferState {
    text: String,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
}

impl TextBufferState {
    fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let end = text.len();
        Self {
            text,
            selected_range: end..end,
            selection_reversed: false,
            marked_range: None,
        }
    }

    fn set_text(&mut self, text: impl Into<String>) {
        *self = Self::new(text);
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        self.text
            .get(..offset.min(self.text.len()))
            .unwrap_or_default()
            .chars()
            .map(char::len_utf16)
            .sum()
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_offset = 0;
        for character in self.text.chars() {
            if utf16_offset >= offset {
                break;
            }
            utf16_offset += character.len_utf16();
            utf8_offset += character.len_utf8();
        }
        utf8_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.text
            .char_indices()
            .rev()
            .find_map(|(index, _)| (index < offset).then_some(index))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.text
            .char_indices()
            .find_map(|(index, _)| (index > offset).then_some(index))
            .unwrap_or(self.text.len())
    }

    fn line_ranges(&self) -> Vec<Range<usize>> {
        let mut ranges = Vec::new();
        let mut start = 0;
        for (index, character) in self.text.char_indices() {
            if character == '\n' {
                ranges.push(start..index);
                start = index + character.len_utf8();
            }
        }
        ranges.push(start..self.text.len());
        ranges
    }

    fn line_and_column(&self, offset: usize) -> (usize, usize) {
        let offset = offset.min(self.text.len());
        let ranges = self.line_ranges();
        let line = ranges
            .iter()
            .position(|range| offset <= range.end)
            .unwrap_or_else(|| ranges.len().saturating_sub(1));
        (line, offset.saturating_sub(ranges[line].start))
    }

    fn move_to(&mut self, offset: usize, extend: bool) {
        let offset = offset.min(self.text.len());
        if extend {
            self.select_to(offset);
        } else {
            self.selected_range = offset..offset;
            self.selection_reversed = false;
        }
    }

    fn select_to(&mut self, offset: usize) {
        let offset = offset.min(self.text.len());
        let cursor = self.cursor_offset();
        let anchor = if self.selected_range.is_empty() {
            cursor
        } else if self.selection_reversed {
            self.selected_range.end
        } else {
            self.selected_range.start
        };
        self.selected_range = anchor.min(offset)..anchor.max(offset);
        self.selection_reversed = offset < anchor;
    }

    fn replace_utf16(
        &mut self,
        range_utf16: Option<Range<usize>>,
        replacement: &str,
    ) -> Range<usize> {
        let range = range_utf16
            .as_ref()
            .map(|range| self.range_from_utf16(range))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        let replacement = replacement.replace("\r\n", "\n").replace('\r', "\n");
        self.text.replace_range(range.clone(), &replacement);
        let cursor = range.start + replacement.len();
        self.selected_range = cursor..cursor;
        self.selection_reversed = false;
        self.marked_range = None;
        range.start..cursor
    }

    fn select_all(&mut self) {
        self.selected_range = 0..self.text.len();
        self.selection_reversed = false;
    }

    fn line_count(&self) -> usize {
        self.line_ranges().len().max(1)
    }
}

struct TextInputElement {
    view: Entity<LoomView>,
    field: InputField,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InputField {
    Composer,
    Rename,
}

struct TextInputPrepaint {
    bounds: Bounds<Pixels>,
    lines: Vec<(Range<usize>, ShapedLine)>,
    selection: Option<PaintQuad>,
    cursor: Option<PaintQuad>,
}

struct LoomTooltip {
    text: SharedString,
}

impl Render for LoomTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_sm()
            .bg(rgb(0x20242c))
            .border_1()
            .border_color(rgb(0x3b4555))
            .text_sm()
            .text_color(rgb(0xe5e7eb))
            .child(self.text.clone())
    }
}

#[derive(Clone)]
enum ClientConnection {
    InProcess(InProcessConnection),
    Remote {
        runtime: Arc<tokio::runtime::Runtime>,
        connection: Arc<Mutex<WebSocketConnection>>,
    },
}

impl ClientConnection {
    fn remote(url: String, token: String) -> Result<Self, LoomError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::Internal,
                    format!("could not create remote client runtime: {error}"),
                    false,
                )
            })?;
        let connection = runtime.block_on(WebSocketTransport::new(&url, &token).connect())?;
        Ok(Self::Remote {
            runtime: Arc::new(runtime),
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    fn request(&self, request: RequestEnvelope) -> ResponseEnvelope {
        match self {
            Self::InProcess(connection) => connection.request(request),
            Self::Remote {
                runtime,
                connection,
                ..
            } => {
                let request_id = request.request_id;
                let result = connection
                    .lock()
                    .map_err(|_| {
                        LoomError::new(
                            ErrorCode::Internal,
                            "remote connection lock was poisoned",
                            true,
                        )
                    })
                    .and_then(|mut connection| runtime.block_on(connection.request(request)));
                match result {
                    Ok(response) => response,
                    Err(error) => ResponseEnvelope::failure(request_id, error),
                }
            }
        }
    }

    fn description(&self) -> &'static str {
        match self {
            Self::InProcess(_) => "local",
            Self::Remote { .. } => "remote",
        }
    }
}

struct LoomView {
    connection: ClientConnection,
    project_id: ProjectId,
    project: Option<ProjectSnapshot>,
    workspace_root: PathBuf,
    projects: Vec<ProjectSnapshot>,
    sessions: Vec<AgentSessionSnapshot>,
    active_session: AgentSessionSnapshot,
    active_run: Option<AgentRunSnapshot>,
    active_run_id: Option<RunId>,
    model: ModelId,
    default_model: ModelId,
    session_models: BTreeMap<AgentSessionId, ModelId>,
    agent_mode: AgentMode,
    agent_mode_picker_open: bool,
    session_event_cache: BTreeMap<AgentSessionId, Vec<ServerEvent>>,
    session_task_cache: BTreeMap<AgentSessionId, String>,
    optimistic_messages: Vec<String>,
    sending_message: bool,
    models: Vec<ModelId>,
    model_picker_open: bool,
    settings_open: bool,
    theme_choice: ThemeChoice,
    dark_theme: bool,
    after_sequence: Option<EventSequence>,
    timeline: Vec<TimelineItem>,
    pending_approval: Option<ToolCall>,
    pending_input: Option<String>,
    composer: TextBufferState,
    composer_focus_handle: FocusHandle,
    input_field: InputField,
    session_state: AgentSessionState,
    run_state: Option<AgentRunState>,
    summary: Option<String>,
    review: ReviewState,
    tasks: Vec<TaskSnapshot>,
    rename_dialog: Option<RenameDialogState>,
    rename_focus_handle: FocusHandle,
    backend_status: BackendStatus,
    demo_workspace: bool,
    login_enabled: bool,
    github_connected: bool,
    github_login: Option<GitHubLoginState>,
}

impl LoomView {
    fn input_state(&self, field: InputField) -> Option<&TextBufferState> {
        match field {
            InputField::Composer => Some(&self.composer),
            InputField::Rename => self.rename_dialog.as_ref().map(|dialog| &dialog.input),
        }
    }

    fn input_state_mut(&mut self, field: InputField) -> Option<&mut TextBufferState> {
        match field {
            InputField::Composer => Some(&mut self.composer),
            InputField::Rename => self.rename_dialog.as_mut().map(|dialog| &mut dialog.input),
        }
    }

    fn input_focus_handle(&self, field: InputField) -> FocusHandle {
        match field {
            InputField::Composer => self.composer_focus_handle.clone(),
            InputField::Rename => self.rename_focus_handle.clone(),
        }
    }

    fn edit_input(&self) -> &TextBufferState {
        self.input_state(self.input_field)
            .expect("focused input field is present")
    }

    fn edit_input_mut(&mut self) -> &mut TextBufferState {
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

impl IntoElement for TextInputElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextInputElement {
    type RequestLayoutState = ();
    type PrepaintState = TextInputPrepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let line_count = self
            .view
            .read(cx)
            .input_state(self.field)
            .map_or(1, TextBufferState::line_count);
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = (window.line_height() * line_count).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.view.read(cx);
        let Some(input) = input.input_state(self.field) else {
            return TextInputPrepaint {
                bounds,
                lines: Vec::new(),
                selection: None,
                cursor: None,
            };
        };
        let text = input.text.clone();
        let ranges = input.line_ranges();
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let mut lines = Vec::with_capacity(ranges.len());
        for range in ranges {
            let line_text = text[range.clone()].to_owned();
            let run = TextRun {
                len: line_text.len(),
                font: style.font(),
                color: style.color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let line = window.text_system().shape_line(
                SharedString::from(line_text),
                font_size,
                &[run],
                None,
            );
            lines.push((range, line));
        }

        let line_height = window.line_height();
        let selection = if input.selected_range.is_empty() {
            None
        } else {
            let range = &input.selected_range;
            lines
                .iter()
                .enumerate()
                .find_map(|(index, (line_range, line))| {
                    let start = range.start.max(line_range.start);
                    let end = range.end.min(line_range.end);
                    (start < end).then(|| {
                        fill(
                            Bounds::from_corners(
                                point(
                                    bounds.left() + line.x_for_index(start - line_range.start),
                                    bounds.top() + line_height * index,
                                ),
                                point(
                                    bounds.left() + line.x_for_index(end - line_range.start),
                                    bounds.top() + line_height * (index + 1),
                                ),
                            ),
                            rgba(0x335b8def),
                        )
                    })
                })
        };
        let cursor = if input.selected_range.is_empty() {
            let offset = input.cursor_offset();
            lines.iter().enumerate().find_map(|(index, (range, line))| {
                (offset >= range.start && offset <= range.end).then(|| {
                    fill(
                        Bounds::new(
                            point(
                                bounds.left() + line.x_for_index(offset - range.start),
                                bounds.top() + line_height * index,
                            ),
                            size(px(2.), line_height),
                        ),
                        rgb(0x60a5fa),
                    )
                })
            })
        } else {
            None
        };
        TextInputPrepaint {
            bounds,
            lines,
            selection,
            cursor,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.view.read(cx).input_focus_handle(self.field);
        self.view.update(cx, |view, _| {
            view.input_field = self.field;
        });
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(prepaint.bounds, self.view.clone()),
            cx,
        );
        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection);
        }
        let line_height = window.line_height();
        for (index, (_, line)) in prepaint.lines.iter().enumerate() {
            let _ = line.paint(
                point(
                    prepaint.bounds.left(),
                    prepaint.bounds.top() + line_height * index,
                ),
                line_height,
                TextAlign::Left,
                None,
                window,
                cx,
            );
        }
        if focus_handle.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }
    }
}

impl LoomView {
    fn try_new(
        options: &UiOptions,
        focus_handle: FocusHandle,
        rename_focus_handle: FocusHandle,
    ) -> Result<Self, LoomError> {
        let (connection, workspace_root, project_id, demo_workspace) =
            if let Some(remote_url) = &options.remote {
                let token = options.token.as_deref().ok_or_else(|| {
                    LoomError::invalid_request("remote connections require LOOM_TOKEN to be set")
                })?;
                let connection = ClientConnection::remote(remote_url.clone(), token.to_owned())?;
                negotiate(&connection)?;
                let projects = list_projects(&connection)?;
                let project = select_remote_project(&projects, options.workspace.as_deref())?;
                let workspace_root = project.root.clone().map(PathBuf::from).ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::WorkspaceAccessDenied,
                        "selected remote project has no configured workspace root",
                        false,
                    )
                })?;
                (connection, workspace_root, project.id, false)
            } else {
                let (workspace_root, demo_workspace) = prepare_workspace(options)?;
                let project_id = if demo_workspace {
                    ProjectId::new()
                } else {
                    stable_project_id(&workspace_root)
                };
                let backend = if demo_workspace {
                    InProcessBackend::demo_with_github_copilot()?
                } else if let Some(endpoint) = &options.endpoint {
                    InProcessBackend::with_openai_compatible_persistent_with_github_copilot(
                        endpoint,
                        options.api_key.as_deref().unwrap_or_default(),
                        options.model.clone(),
                        backend_persistence_path(&workspace_root)?,
                    )?
                } else {
                    InProcessBackend::new_persistent_with_github_copilot(backend_persistence_path(
                        &workspace_root,
                    )?)?
                };
                (
                    ClientConnection::InProcess(backend.connect()),
                    workspace_root,
                    project_id,
                    demo_workspace,
                )
            };
        if options.remote.is_none() {
            negotiate(&connection)?;
            open_workspace(&connection, project_id, &workspace_root)?;
        }
        let sessions = list_sessions(&connection, project_id)?;
        let session = sessions.into_iter().next().map_or_else(
            || create_session(&connection, project_id, "New session"),
            Ok,
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
        let run = if demo_workspace {
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
            session_event_cache: BTreeMap::new(),
            session_task_cache: BTreeMap::new(),
            optimistic_messages: Vec::new(),
            sending_message: false,
            model,
            models,
            model_picker_open: false,
            settings_open: false,
            theme_choice: ThemeChoice::System,
            dark_theme: true,
            after_sequence: None,
            timeline: Vec::new(),
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
            backend_status: BackendStatus::Connected,
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
        };
        view.refresh_models();
        view.refresh_sessions()?;
        let active_session = view.active_session.clone();
        view.load_session(active_session);
        Ok(view)
    }

    fn record_status(&mut self, status: impl Into<String>) {
        self.timeline.push(TimelineItem::Status(status.into()));
    }

    fn record_backend_error(&mut self, operation: &str, error: LoomError) {
        self.backend_status = BackendStatus::Error(error.clone());
        self.timeline.push(TimelineItem::Error {
            operation: operation.to_owned(),
            error,
        });
    }

    fn refresh_models(&mut self) {
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
            Ok(models) => {
                if !models.contains(&self.model) {
                    if let Some(model) = models
                        .iter()
                        .find(|model| model.as_str() == self.default_model.as_str())
                        .cloned()
                        .or_else(|| models.first().cloned())
                    {
                        self.model = model;
                    }
                }
                self.models = models;
                self.record_status(format!(
                    "Model list refreshed ({} available)",
                    self.models.len()
                ));
            }
            Err(error) => self.record_status(format!("Could not refresh models: {error}")),
        }
    }

    fn unexpected_response(operation: &str, response: ServerResponse) -> LoomError {
        LoomError::new(
            ErrorCode::Internal,
            format!("backend returned unexpected {operation} response: {response:?}"),
            false,
        )
    }

    fn refresh_sessions(&mut self) -> Result<(), LoomError> {
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
            response => return Err(Self::unexpected_response("session list", response)),
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
            response => return Err(Self::unexpected_response("project list", response)),
        }
        self.backend_status = BackendStatus::Connected;
        Ok(())
    }

    fn reset_projection(&mut self) {
        self.timeline.clear();
        self.pending_approval = None;
        self.pending_input = None;
        self.active_run = None;
        self.active_run_id = None;
        self.run_state = None;
        self.summary = None;
        self.after_sequence = None;
    }

    fn activate_session(&mut self, session: AgentSessionSnapshot) {
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

    fn load_session(&mut self, session: AgentSessionSnapshot) {
        self.activate_session(session);
        let projection_run_id = match self
            .connection
            .request(RequestEnvelope::new(
                ClientRequest::GetAgentSessionSnapshot {
                    session_id: self.active_session.id,
                },
            ))
            .result
        {
            Ok(ServerResponse::AgentSessionSnapshot(projection)) => {
                self.active_session = projection.session;
                self.session_state = self.active_session.state;
                self.active_run = projection.active_run.as_ref().map(|run| run.run.clone());
                self.active_run_id = projection.active_run.as_ref().map(|run| run.run.id);
                self.run_state = self.active_run.as_ref().map(|run| run.state);
                if let Some(run) = &self.active_run {
                    self.model = run.model.clone();
                    self.session_task_cache
                        .insert(self.active_session.id, run.task.clone());
                }
                self.active_run_id
            }
            Err(error) => {
                self.record_backend_error("load session snapshot", error);
                None
            }
            Ok(response) => {
                self.record_backend_error(
                    "load session snapshot",
                    Self::unexpected_response("session snapshot", response),
                );
                None
            }
        };
        if let Err(error) = self.collect_recent_events() {
            self.record_backend_error("load session events", error);
        }
        if self.active_run_id.is_none() {
            self.active_run_id = projection_run_id;
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

    fn collect_events(&mut self) -> Result<(), LoomError> {
        let response =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
                    session_id: Some(self.active_session.id),
                    after_sequence: self.after_sequence,
                }));
        match response.result? {
            ServerResponse::SessionEvents { events } => {
                for event in events {
                    self.after_sequence = Some(event.sequence);
                    self.consume_event(&event.event);
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
            }
            response => return Err(Self::unexpected_response("session event stream", response)),
        }
        self.backend_status = BackendStatus::Connected;
        Ok(())
    }

    fn collect_recent_events(&mut self) -> Result<(), LoomError> {
        let response = self.connection.request(RequestEnvelope::new(
            ClientRequest::GetRecentSessionEvents {
                session_id: self.active_session.id,
                limit: 32,
            },
        ));
        match response.result? {
            ServerResponse::SessionEvents { events } => {
                for event in events {
                    self.after_sequence = Some(event.sequence);
                    self.consume_event(&event.event);
                }
            }
            response => return Err(Self::unexpected_response("recent session events", response)),
        }
        self.backend_status = BackendStatus::Connected;
        Ok(())
    }

    fn consume_event(&mut self, event: &ServerEvent) {
        match event {
            ServerEvent::AgentSessionCreated { snapshot } => {
                self.active_session = snapshot.clone();
                self.session_state = snapshot.state;
            }
            ServerEvent::AgentSessionStateChanged { current, .. } => {
                self.session_state = *current;
                self.active_session.state = *current;
            }
            ServerEvent::AgentSessionForked { .. } => {}
            ServerEvent::AgentSessionRenamed { name, .. } => {
                self.active_session.name = name.clone();
            }
            ServerEvent::AgentSessionArchived { .. } => {
                self.session_state = AgentSessionState::Archived;
                self.active_session.state = AgentSessionState::Archived;
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

    fn consume_agent_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::RunStarted { snapshot } => {
                self.active_run = Some(snapshot.clone());
                self.active_run_id = Some(snapshot.id);
                self.run_state = Some(snapshot.state);
                self.timeline
                    .push(TimelineItem::Status("Agent run started".to_owned()));
            }
            AgentEvent::PlanProposed { plan, .. } => self.timeline.push(TimelineItem::Plan(
                plan.steps
                    .iter()
                    .map(|step| step.description.clone())
                    .collect(),
            )),
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
                if let Some(TimelineItem::Assistant(message)) = self.timeline.last_mut() {
                    message.push_str(text);
                } else {
                    self.timeline.push(TimelineItem::Assistant(text.clone()));
                }
            }
            AgentEvent::StepStarted { index, .. } => {
                self.timeline
                    .push(TimelineItem::StepStarted { index: *index });
            }
            AgentEvent::StepCompleted { index, .. } => {
                self.timeline
                    .push(TimelineItem::StepCompleted { index: *index });
            }
            AgentEvent::ContextInspected { inspection, .. } => self.record_status(format!(
                "Context: {} input tokens, {} omitted",
                inspection.included_tokens, inspection.omitted_tokens
            )),
            AgentEvent::ProviderError { error, .. } | AgentEvent::ContextError { error, .. } => {
                self.timeline.push(TimelineItem::Error {
                    operation: "agent".to_owned(),
                    error: error.clone(),
                });
            }
            AgentEvent::ToolCallRequested { call, .. } => {
                self.timeline.push(TimelineItem::ToolRequested {
                    name: call.name.clone(),
                    arguments: bounded(&serde_json::to_string(&call.arguments).unwrap_or_default()),
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
            AgentEvent::ToolPolicyEvaluated { evaluation, .. } => self.record_status(format!(
                "Policy {:?}: {}",
                evaluation.decision, evaluation.reason
            )),
            AgentEvent::ToolApprovalDecided { decision, .. } => {
                self.pending_approval = None;
                for item in &mut self.timeline {
                    if let TimelineItem::Approval { active, .. } = item {
                        *active = false;
                    }
                }
                self.record_status(format!("Approval: {decision:?}"));
            }
            AgentEvent::ToolCallStarted { call, .. } => {
                self.timeline
                    .push(TimelineItem::ToolStarted(call.name.clone()));
            }
            AgentEvent::ToolOutputChunk { chunk, .. } => {
                if let Some(TimelineItem::ToolOutput(output)) = self.timeline.last_mut() {
                    output.push_str(chunk);
                    *output = bounded(output);
                } else {
                    self.timeline.push(TimelineItem::ToolOutput(bounded(chunk)));
                }
            }
            AgentEvent::ToolCallCompleted { result, .. } => {
                self.timeline.push(TimelineItem::ToolCompleted {
                    name: result.name.clone(),
                    success: result.success,
                });
            }
            AgentEvent::NeedsInput { prompt, .. } => {
                self.pending_input = Some(prompt.clone());
                self.timeline.push(TimelineItem::NeedsInput(prompt.clone()));
            }
            AgentEvent::RunUsage { usage, .. } => self.record_status(format!(
                "Usage: {} input / {} output tokens",
                usage.input_tokens, usage.output_tokens
            )),
            AgentEvent::RunUsageUpdated { usage, .. } => self.record_status(format!(
                "Total usage: {} input / {} output / {} tool calls",
                usage.input_tokens, usage.output_tokens, usage.tool_calls
            )),
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
            }
            AgentEvent::RunCompleted { snapshot } => {
                self.active_run = Some(snapshot.clone());
                self.active_run_id = Some(snapshot.id);
                self.run_state = Some(snapshot.state);
                self.session_state = session_state_for_run(snapshot.state);
                self.active_session.state = self.session_state;
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
        }
    }

    fn refresh_run_snapshot(&mut self) -> Result<(), LoomError> {
        let Some(run_id) = self.active_run_id else {
            return Ok(());
        };
        let response =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::GetAgentRunSnapshot {
                    run_id,
                }));
        match response.result? {
            ServerResponse::AgentRunSnapshot(projection) => {
                self.apply_run_projection(projection);
                Ok(())
            }
            response => Err(Self::unexpected_response("run snapshot", response)),
        }
    }

    fn apply_run_projection(&mut self, projection: AgentRunSnapshotProjection) {
        self.active_run_id = Some(projection.run.id);
        self.active_run = Some(projection.run.clone());
        self.run_state = Some(projection.run.state);
        self.summary = projection.run.summary.clone();
        self.pending_approval = projection.pending_approval;
        self.pending_input = projection.pending_input;
        if self.timeline.is_empty() {
            let mut timeline = Vec::new();
            for message in projection.messages {
                match message.role {
                    MessageRole::User => timeline.push(TimelineItem::User(message.content)),
                    MessageRole::Assistant => {
                        timeline.push(TimelineItem::Assistant(message.content))
                    }
                    MessageRole::Tool => {
                        timeline.push(TimelineItem::ToolOutput(bounded(&message.content)))
                    }
                    MessageRole::System => {}
                }
            }
            if !projection.plan.is_empty() {
                self.timeline.insert(
                    0,
                    TimelineItem::Plan(
                        projection
                            .plan
                            .into_iter()
                            .map(|step| step.description)
                            .collect(),
                    ),
                );
            }
            self.timeline = timeline;
        }
        if let Some(summary) = &projection.run.summary
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

    fn refresh_review(&mut self) {
        let changes =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::GetWorkspaceChanges {
                    project_id: self.project_id,
                    after_sequence: None,
                }));
        match changes.result {
            Ok(ServerResponse::WorkspaceChanges { changes, truncated }) => {
                let mut seen = BTreeSet::new();
                self.review.changes = changes
                    .into_iter()
                    .rev()
                    .filter(|change| seen.insert(change.path.clone()))
                    .take(MAX_REVIEW_CHANGES)
                    .collect();
                if truncated {
                    self.record_status(
                        "Workspace review is showing the most recent changes".to_owned(),
                    );
                }
            }
            Err(error) => self.record_backend_error("workspace review refresh", error),
            Ok(response) => self.record_backend_error(
                "workspace review refresh",
                Self::unexpected_response("workspace changes", response),
            ),
        }

        let status = self
            .connection
            .request(RequestEnvelope::new(ClientRequest::GetVcsStatus {
                project_id: self.project_id,
            }));
        match status.result {
            Ok(ServerResponse::VcsStatus(status)) => self.review.vcs = Some(status),
            Err(error) => {
                self.review.vcs = None;
                self.record_status(format!("VCS review unavailable: {error}"));
            }
            Ok(response) => self.record_backend_error(
                "VCS review refresh",
                Self::unexpected_response("VCS status", response),
            ),
        }
    }

    fn create_session_and_select(&mut self, name: String) -> Result<(), LoomError> {
        let snapshot = create_session(&self.connection, self.project_id, &name)?;
        self.sessions.push(snapshot.clone());
        self.activate_session(snapshot);
        self.backend_status = BackendStatus::Connected;
        Ok(())
    }

    fn confirm_rename(&mut self) {
        let Some(dialog) = self.rename_dialog.take() else {
            return;
        };
        let name = dialog.input.text.trim().to_owned();
        if name.is_empty() {
            self.record_status("Session name cannot be empty");
            self.rename_dialog = Some(dialog);
            return;
        }
        let response =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::RenameAgentSession {
                    session_id: dialog.session.id,
                    name,
                }));
        match response.result {
            Ok(ServerResponse::AgentSessionRenamed(snapshot)) => {
                self.active_session = snapshot;
                if let Err(error) = self.refresh_sessions() {
                    self.record_backend_error("session list refresh", error);
                }
            }
            Err(error) => {
                self.rename_dialog = Some(dialog);
                self.record_backend_error("rename session", error);
            }
            Ok(response) => self.record_backend_error(
                "rename session",
                Self::unexpected_response("session rename", response),
            ),
        }
    }

    fn archive_active(&mut self) {
        let response =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::ArchiveAgentSession {
                    session_id: self.active_session.id,
                }));
        match response.result {
            Ok(ServerResponse::AgentSessionArchived(_)) => {
                if let Err(error) = self.refresh_sessions() {
                    self.record_backend_error("session list refresh", error);
                    return;
                }
                if let Some(session) = self.sessions.first().cloned() {
                    self.load_session(session);
                } else if let Err(error) = self.create_session_and_select("New session".to_owned())
                {
                    self.record_backend_error("create replacement session", error);
                }
            }
            Err(error) => self.record_backend_error("archive session", error),
            Ok(response) => self.record_backend_error(
                "archive session",
                Self::unexpected_response("session archive", response),
            ),
        }
    }

    fn send_message(&mut self, message: String, cx: &mut Context<Self>) {
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
        let connection = self.connection.clone();
        let session_id = self.active_session.id;
        let request = RequestEnvelope::new(request);
        cx.spawn(async move |view, cx| {
            let response = cx
                .background_spawn(async move {
                    if let Some(title) = session_title {
                        let _ = connection.request(RequestEnvelope::new(
                            ClientRequest::RenameAgentSession {
                                session_id,
                                name: title,
                            },
                        ));
                    }
                    connection.request(request)
                })
                .await;
            view.update(cx, |view, cx| view.finish_send_response(response, cx))
                .ok();
        })
        .detach();
    }

    fn finish_send_response(&mut self, response: ResponseEnvelope, cx: &mut Context<Self>) {
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
                if let Err(error) = self.collect_events() {
                    self.record_backend_error("message event stream", error);
                }
                if let Err(error) = self.refresh_run_snapshot() {
                    self.record_backend_error("message run snapshot", error);
                }
                if let Some(run) = &self.active_run
                    && !self
                        .timeline
                        .iter()
                        .any(|item| matches!(item, TimelineItem::User(text) if text == &run.task))
                {
                    self.timeline
                        .insert(0, TimelineItem::User(run.task.clone()));
                }
                self.refresh_review();
            }
            Err(error) => self.record_backend_error("send message", error),
            Ok(response) => self.record_backend_error(
                "send message",
                Self::unexpected_response("send message", response),
            ),
        }
        cx.notify();
    }

    fn submit_composer(&mut self, cx: &mut Context<Self>) {
        let text = self.composer.text.trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.composer.set_text("");
        self.send_message(text, cx);
    }

    fn focus_composer(&mut self, _: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.composer_focus_handle.focus(window, cx);
        cx.notify();
    }

    fn focus_rename(&mut self, _: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.rename_focus_handle.focus(window, cx);
        cx.notify();
    }

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
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

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
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

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        let offset = if self.edit_input().selected_range.is_empty() {
            self.edit_input()
                .previous_boundary(self.edit_input().cursor_offset())
        } else {
            self.edit_input().selected_range.start
        };
        self.edit_input_mut().move_to(offset, false);
        cx.notify();
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        let offset = if self.edit_input().selected_range.is_empty() {
            self.edit_input()
                .next_boundary(self.edit_input().cursor_offset())
        } else {
            self.edit_input().selected_range.end
        };
        self.edit_input_mut().move_to(offset, false);
        cx.notify();
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.edit_input_mut().select_all();
        cx.notify();
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        let (line, _) = self
            .edit_input()
            .line_and_column(self.edit_input().cursor_offset());
        let offset = self.edit_input().line_ranges()[line].start;
        self.edit_input_mut().move_to(offset, false);
        cx.notify();
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        let (line, _) = self
            .edit_input()
            .line_and_column(self.edit_input().cursor_offset());
        let offset = self.edit_input().line_ranges()[line].end;
        self.edit_input_mut().move_to(offset, false);
        cx.notify();
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.edit_input_mut().replace_utf16(None, &text);
            cx.notify();
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_input().selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.edit_input().text[self.edit_input().selected_range.clone()].to_owned(),
            ));
        }
    }

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        if self.rename_dialog.is_some() {
            self.confirm_rename();
        } else {
            self.submit_composer(cx);
        }
        cx.notify();
    }

    fn select_session(&mut self, session: AgentSessionSnapshot, cx: &mut Context<Self>) {
        self.github_login = None;
        self.settings_open = false;
        self.review.open = false;
        self.activate_session(session.clone());
        if let Some(events) = self.session_event_cache.get(&session.id).cloned() {
            for event in events {
                self.consume_event(&event);
            }
        }
        self.ensure_session_task_message(session.id);
        let connection = self.connection.clone();
        let session_id = session.id;
        let needs_snapshot = !self.session_task_cache.contains_key(&session_id);
        cx.spawn(async move |view, cx| {
            let snapshot = if needs_snapshot {
                let connection = connection.clone();
                Some(
                    cx.background_spawn(async move {
                        connection.request(RequestEnvelope::new(
                            ClientRequest::GetAgentSessionSnapshot { session_id },
                        ))
                    })
                    .await,
                )
            } else {
                None
            };
            let events_task = cx.background_spawn(async move {
                connection.request(RequestEnvelope::new(
                    ClientRequest::GetRecentSessionEvents {
                        session_id,
                        limit: 32,
                    },
                ))
            });
            let events = events_task.await;
            view.update(cx, |view, cx| {
                view.finish_async_session_load(session_id, snapshot, events, cx);
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn finish_async_session_load(
        &mut self,
        session_id: AgentSessionId,
        snapshot_response: Option<ResponseEnvelope>,
        events_response: ResponseEnvelope,
        cx: &mut Context<Self>,
    ) {
        if self.active_session.id != session_id {
            return;
        }
        if let Some(snapshot_response) = snapshot_response {
            if let Ok(ServerResponse::AgentSessionSnapshot(projection)) = snapshot_response.result {
                self.active_session = projection.session;
                self.active_run = projection.active_run.as_ref().map(|run| run.run.clone());
                self.active_run_id = projection.active_run.as_ref().map(|run| run.run.id);
                self.run_state = self.active_run.as_ref().map(|run| run.state);
                if let Some(run) = &self.active_run {
                    self.session_task_cache.insert(session_id, run.task.clone());
                }
            }
        }
        self.reset_projection();
        match events_response.result {
            Ok(ServerResponse::SessionEvents { events }) => {
                self.session_event_cache.insert(
                    session_id,
                    events.iter().map(|event| event.event.clone()).collect(),
                );
                for event in events {
                    self.after_sequence = Some(event.sequence);
                    self.consume_event(&event.event);
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
            }
            Err(error) => self.record_backend_error("load session events", error),
            Ok(response) => self.record_backend_error(
                "load session events",
                Self::unexpected_response("session event stream", response),
            ),
        }
        self.ensure_session_task_message(session_id);
        cx.notify();
    }

    fn ensure_session_task_message(&mut self, session_id: AgentSessionId) {
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

    fn toggle_github_login(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
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

    fn copy_github_login_value(
        &mut self,
        value: String,
        label: &'static str,
        cx: &mut Context<Self>,
    ) {
        cx.write_to_clipboard(ClipboardItem::new_string(value));
        self.record_status(format!("Copied GitHub {label} to clipboard"));
        cx.notify();
    }

    fn start_github_login(&mut self, cx: &mut Context<Self>) {
        self.github_login = Some(GitHubLoginState::Starting);
        cx.notify();
        let task = cx.background_spawn(async { GitHubCopilotAuthenticator::default().begin() });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            view.update(cx, |view, cx| view.handle_github_device_code(result, cx))
                .ok();
        })
        .detach();
    }

    fn handle_github_device_code(
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

    fn finish_github_login(&mut self, result: Result<String, LoomError>, cx: &mut Context<Self>) {
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
        match discover_provider_models(&self.connection).map(|_| ()) {
            Ok(()) => self.refresh_models(),
            Err(error) => self.record_backend_error("refresh models after login", error),
        }
        self.github_login = Some(GitHubLoginState::Success);
        self.github_connected = true;
        self.record_status("GitHub Copilot login succeeded");
        cx.notify();
    }

    fn toggle_model_picker(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.model_picker_open = !self.model_picker_open;
        cx.notify();
    }

    fn refresh_models_button(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.refresh_models();
        cx.notify();
    }

    fn select_model(&mut self, model: ModelId, cx: &mut Context<Self>) {
        self.session_models
            .insert(self.active_session.id, model.clone());
        self.model = model;
        self.model_picker_open = false;
        cx.notify();
    }

    fn toggle_agent_mode_picker(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.agent_mode_picker_open = !self.agent_mode_picker_open;
        cx.notify();
    }

    fn select_agent_mode(&mut self, mode: AgentMode, cx: &mut Context<Self>) {
        self.agent_mode = mode;
        self.agent_mode_picker_open = false;
        cx.notify();
    }

    fn select_default_model(&mut self, model: ModelId, cx: &mut Context<Self>) {
        self.default_model = model;
        self.settings_open = false;
        cx.notify();
    }

    fn open_settings_from_menu(&mut self, cx: &mut Context<Self>) {
        self.github_login = None;
        self.review.open = false;
        self.settings_open = true;
        cx.notify();
    }

    fn close_settings(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.settings_open = false;
        cx.notify();
    }

    fn select_theme(&mut self, theme: ThemeChoice, window: &mut Window, cx: &mut Context<Self>) {
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
        };
        self.dark_theme = matches!(
            appearance,
            WindowAppearance::Dark | WindowAppearance::VibrantDark
        );
        DARK_THEME_ACTIVE.store(self.dark_theme, Ordering::Relaxed);
        gpui_component::Theme::change(appearance, Some(window), cx);
        cx.notify();
    }

    fn new_session(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let name = format!("Session {}", self.sessions.len().saturating_add(1));
        if let Err(error) = self.create_session_and_select(name) {
            self.record_backend_error("create session", error);
        }
        cx.notify();
    }

    fn begin_session_rename(
        &mut self,
        session: AgentSessionSnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.load_session(session);
        self.rename_dialog = Some(RenameDialogState {
            session: self.active_session.clone(),
            input: TextBufferState::new(self.active_session.name.clone()),
        });
        self.rename_focus_handle.focus(window, cx);
    }

    fn toggle_changes_sidebar(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.review.panel = ReviewPanel::Changes;
        self.review.open = !self.review.open;
        self.refresh_review();
        cx.notify();
    }

    fn show_review(
        &mut self,
        _panel: ReviewPanel,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings_open = false;
        self.github_login = None;
        self.toggle_changes_sidebar(event, window, cx);
    }

    fn close_review(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.review.open = false;
        cx.notify();
    }

    fn open_review_file(&mut self, path: String) {
        let response =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::ReadWorkspaceFile {
                    project_id: self.project_id,
                    path,
                }));
        match response.result {
            Ok(ServerResponse::WorkspaceFile(mut file)) => {
                file.content = bounded_to(&file.content, MAX_REVIEW_DIFF);
                self.review.diff_path = Some(file.path.clone());
                self.review.selected_file = Some(file);
                self.review.open = true;
                self.review.panel = ReviewPanel::Changes;
            }
            Err(error) => self.record_backend_error("read review file", error),
            Ok(response) => self.record_backend_error(
                "read review file",
                Self::unexpected_response("workspace file", response),
            ),
        }
    }

    fn render_session_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                .py_2()
                .rounded_sm()
                .bg(if active { rgb(0x293244) } else { rgb(0x1b1d24) })
                .text_color(if active { rgb(0xf3f4f6) } else { rgb(0xb7c0d0) })
                .cursor_pointer()
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
                        .child(session_state_name(session.state)),
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
                    archive_view.update(_cx, |view, _cx| {
                        view.load_session(archive_session);
                        view.archive_active();
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

    fn render_model_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
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

    fn render_agent_mode_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
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

    fn render_timeline_item(
        &self,
        item: &TimelineItem,
        index: usize,
        _cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let user_background = if self.dark_theme {
            gpui::rgb(0x1f4f78)
        } else {
            gpui::rgb(0xdbeafe)
        };
        let user_foreground = if self.dark_theme {
            gpui::rgb(0xdbeafe)
        } else {
            gpui::rgb(0x1e3a8a)
        };
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
                        .child(
                            TextView::markdown(format!("transcript-user-{index}"), text.clone())
                                .selectable(true)
                                .w_full()
                                .text_color(user_foreground),
                        ),
                )
                .into_any(),
            TimelineItem::Assistant(text) => div()
                .p_3()
                .rounded_lg()
                .bg(rgb(0x17191f))
                .text_color(rgb(0xf3f4f6))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x9ad7bd))
                        .child("GitHub Copilot"),
                )
                .child(
                    TextView::markdown(format!("transcript-assistant-{index}"), text.clone())
                        .selectable(true)
                        .w_full()
                        .text_color(rgb(0xf3f4f6)),
                )
                .into_any(),
            TimelineItem::Plan(steps) => {
                let mut card = div()
                    .p_2()
                    .rounded_sm()
                    .bg(rgb(0x1e293b))
                    .text_color(rgb(0xdbeafe))
                    .child(div().text_xs().text_color(rgb(0x93c5fd)).child("PLAN"));
                for (index, step) in steps.iter().enumerate() {
                    card = card.child(div().text_sm().child(format!("{}. {}", index + 1, step)));
                }
                card.into_any()
            }
            TimelineItem::StepStarted { index } => div()
                .px_2()
                .py_1()
                .text_sm()
                .text_color(rgb(0xfef3c7))
                .child(format!("Step {} started", index + 1))
                .into_any(),
            TimelineItem::StepCompleted { index } => div()
                .px_2()
                .py_1()
                .text_sm()
                .text_color(rgb(0x9ad7bd))
                .child(format!("Step {} completed", index + 1))
                .into_any(),
            TimelineItem::ToolRequested { name, arguments } => div()
                .p_2()
                .rounded_sm()
                .bg(rgb(0x242833))
                .text_color(rgb(0xcbd5e1))
                .child(format!("Tool requested  {name}"))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x8f98a6))
                        .child(arguments.clone()),
                )
                .into_any(),
            TimelineItem::Approval { name, active } => div()
                .p_2()
                .rounded_sm()
                .bg(if *active {
                    rgb(0x493b1a)
                } else {
                    rgb(0x242833)
                })
                .text_color(if *active {
                    rgb(0xfef3c7)
                } else {
                    rgb(0x94a3b8)
                })
                .child(if *active {
                    format!("Approval required  {name}")
                } else {
                    format!("Approval resolved  {name}")
                })
                .into_any(),
            TimelineItem::ToolStarted(name) => div()
                .px_2()
                .py_1()
                .text_sm()
                .text_color(rgb(0xcbd5e1))
                .child(format!("Tool started  {name}"))
                .into_any(),
            TimelineItem::ToolOutput(output) => div()
                .mx_2()
                .p_2()
                .rounded_sm()
                .bg(rgb(0x0f1115))
                .text_xs()
                .text_color(rgb(0x94a3b8))
                .child(output.clone())
                .into_any(),
            TimelineItem::ToolCompleted { name, success } => div()
                .px_2()
                .py_1()
                .text_sm()
                .text_color(if *success {
                    rgb(0x9ad7bd)
                } else {
                    rgb(0xfca5a5)
                })
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
                .child(
                    div().mt_1().child(
                        TextView::markdown(
                            format!("timeline-error-{index}"),
                            error.message.clone(),
                        )
                        .selectable(true),
                    ),
                )
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
                .child(
                    TextView::markdown(format!("timeline-input-{index}"), prompt.clone())
                        .selectable(true),
                )
                .into_any(),
            TimelineItem::Summary { text, evidence } => {
                let mut card = div()
                    .p_2()
                    .rounded_sm()
                    .bg(rgb(0x064e3b))
                    .text_color(rgb(0xd1fae5))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(0x9ad7bd))
                            .child("FINAL SUMMARY"),
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

    fn render_timeline(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut timeline = div().flex().flex_col().gap_3().p_4();
        if self.timeline.is_empty() {
            timeline = timeline.child(
                div()
                    .p_3()
                    .text_sm()
                    .text_color(rgb(0x8f98a6))
                    .child("Start a task to see the agent run here."),
            );
        }
        for (index, item) in self.timeline.iter().enumerate() {
            if matches!(
                item,
                TimelineItem::User(_)
                    | TimelineItem::Assistant(_)
                    | TimelineItem::Approval { .. }
                    | TimelineItem::Error { .. }
                    | TimelineItem::NeedsInput(_)
            ) {
                timeline = timeline.child(self.render_timeline_item(item, index, cx));
            }
        }
        timeline
    }

    fn render_review(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let title = match self.review.panel {
            ReviewPanel::Changes => "CHANGED FILES",
            ReviewPanel::Diff => "READ-ONLY DIFF",
            ReviewPanel::Evidence => "TASK EVIDENCE",
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
                            .child(format!("{:?}  {}", change.kind, change.path))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_review_file(path.clone());
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
                                .child(format!("Git  {:?}  {}", file.worktree, file.path))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_review_file(path.clone());
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
                        .child("Projection only; editing and staging are not available in M5."),
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
                        .child("TASK RESULT AND EVIDENCE"),
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
                    .py_1()
                    .flex()
                    .items_center()
                    .justify_between()
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

    fn render_composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
            .p_2()
            .bg(rgb(0x17191f))
            .border_t_1()
            .border_color(rgb(0x30343f))
            .child(
                div()
                    .w_full()
                    .min_h(px(42.))
                    .p_2()
                    .rounded_sm()
                    .bg(rgb(0x0f1115))
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
                div().mt_1().flex().items_center().justify_between().child(
                    div()
                        .flex()
                        .relative()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .id("composer-agent-mode")
                                .px_2()
                                .py_1()
                                .rounded_sm()
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
                                .rounded_sm()
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

    fn render_rename_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                                this.confirm_rename();
                                cx.notify();
                            })),
                    ),
            )
            .into_any()
    }

    fn render_github_login_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                                        if let Err(error) = open::that(&verification_uri) {
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
            .bg(transparent_black())
            .text_color(rgb(0xe5e7eb))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0xf3f4f6))
                    .child("Log in to GitHub Copilot"),
            )
            .child(
                div()
                    .absolute()
                    .top(px(24.))
                    .right(px(24.))
                    .id("close-github-login")
                    .px_3()
                    .py_1()
                    .rounded_sm()
                    .bg(rgb(0x20242c))
                    .hover(|style| style.bg(rgb(0x293244)))
                    .text_xs()
                    .text_color(rgb(0xb7c0d0))
                    .cursor_pointer()
                    .child("Back to session")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.github_login = None;
                        cx.notify();
                    })),
            )
            .child(div().mt_4().child(body))
            .into_any()
    }

    fn render_settings_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                            .px_3()
                            .py_1()
                            .rounded_sm()
                            .bg(rgb(0x20242c))
                            .hover(|style| style.bg(rgb(0x293244)))
                            .text_xs()
                            .text_color(rgb(0xb7c0d0))
                            .cursor_pointer()
                            .child("Back to session")
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
                div()
                    .mt_2()
                    .flex()
                    .gap_1()
                    .children(
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
}

impl Render for LoomView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        let (backend_label, backend_color) = match &self.backend_status {
            BackendStatus::Connected => (
                format!("{} / connected", self.connection.description()),
                rgb(0x9ad7bd),
            ),
            BackendStatus::Error(error) => (
                if error.retryable {
                    format!("{} / retryable error", self.connection.description())
                } else {
                    format!("{} / backend error", self.connection.description())
                },
                rgb(0xfca5a5),
            ),
        };
        let project_name = self
            .project
            .as_ref()
            .map(|project| project.name.as_str())
            .unwrap_or("Project");
        let login_label = match (&self.github_login, self.github_connected) {
            (_, true) => "GitHub connected",
            (None, false) => "Log in",
            (Some(GitHubLoginState::Success), false) => "GitHub connected",
            (Some(_), false) => "GitHub login...",
        };
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
                    .h(px(40.))
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
                            .child("Loom")
                            .child(div().text_sm().text_color(rgb(0x8f98a6)).child(format!(
                                "{}  /  {}",
                                project_name, self.active_session.name
                            ))),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_sm()
                            .text_color(backend_color)
                            .child(format!(
                                "{}  {}",
                                session_state_name(self.session_state),
                                backend_label
                            ))
                            .when(self.login_enabled, |element| {
                                element.child(
                                    div()
                                        .id("github-login")
                                        .px_2()
                                        .py_1()
                                        .rounded_sm()
                                        .bg(rgb(0x20242c))
                                        .hover(|style| style.bg(rgb(0x293244)))
                                        .text_xs()
                                        .cursor_pointer()
                                        .child(login_label)
                                        .on_click(cx.listener(Self::toggle_github_login)),
                                )
                            })
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
                                        .child(Icon::new(IconName::Close).size_4())
                                        .on_click(|_, window, _| {
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
                                        div().text_xs().text_color(rgb(0x93c5fd)).child("SESSIONS"),
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
                                        div().text_xs().text_color(rgb(0x93c5fd)).child("SESSIONS"),
                                    )
                                    .child(
                                        div()
                                            .id("new-session")
                                            .w(px(26.))
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
                                                    text: "Create session".into(),
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
                                        "{} sessions  |  {}",
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
                                                        "{}  |  {}  |  {} model{}",
                                                        self.run_state
                                                            .map(run_state_name)
                                                            .unwrap_or("idle"),
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
                                    .overflow_y_scroll()
                                    .child(self.render_timeline(cx)),
                            )
                            .child(self.render_composer(cx))
                            .when(self.rename_dialog.is_some(), |element| {
                                element.child(self.render_rename_dialog(cx))
                            })
                            .when(self.settings_open, |element| {
                                element.child(self.render_settings_dialog(cx))
                            })
                            .when(self.github_login.is_some(), |element| {
                                element.child(self.render_github_login_dialog(cx))
                            }),
                    )
                    .when(
                        self.review.open && !self.settings_open && self.github_login.is_none(),
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
                        "{}  |  {} timeline items  |  {}",
                        session_state_name(self.session_state),
                        self.timeline.len(),
                        if self.demo_workspace {
                            "demo workspace"
                        } else {
                            "workspace"
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
                            element.shadow(vec![gpui::BoxShadow {
                                color: gpui::hsla(0., 0., 0., 0.4),
                                blur_radius: shadow_size / 2.,
                                spread_radius: px(0.),
                                offset: point(px(0.), px(0.)),
                                inset: false,
                            }])
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

fn resize_edge(
    position: Point<Pixels>,
    inset: Pixels,
    size: gpui::Size<Pixels>,
) -> Option<ResizeEdge> {
    let edge = if position.y < inset && position.x < inset {
        ResizeEdge::TopLeft
    } else if position.y < inset && position.x > size.width - inset {
        ResizeEdge::TopRight
    } else if position.y < inset {
        ResizeEdge::Top
    } else if position.y > size.height - inset && position.x < inset {
        ResizeEdge::BottomLeft
    } else if position.y > size.height - inset && position.x > size.width - inset {
        ResizeEdge::BottomRight
    } else if position.y > size.height - inset {
        ResizeEdge::Bottom
    } else if position.x < inset {
        ResizeEdge::Left
    } else if position.x > size.width - inset {
        ResizeEdge::Right
    } else {
        return None;
    };
    Some(edge)
}

fn bounded(value: &str) -> String {
    bounded_to(value, MAX_TIMELINE_OUTPUT)
}

fn session_title_from_task(task: &str) -> String {
    let normalized = task.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut title = normalized.chars().take(56).collect::<String>();
    if normalized.chars().count() > 56 {
        title.push('…');
    }
    if title.is_empty() {
        "New session".to_owned()
    } else {
        title
    }
}

fn bounded_to(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let end = value
        .char_indices()
        .map(|(index, character)| (index, index + character.len_utf8()))
        .take_while(|(_, end)| *end <= limit)
        .map(|(_, end)| end)
        .last()
        .unwrap_or_default();
    let mut result = value[..end].to_owned();
    result.push_str("\n...[output truncated]");
    result
}

fn state_color(state: AgentSessionState) -> gpui::Rgba {
    match state {
        AgentSessionState::Completed => rgb(0x9ad7bd),
        AgentSessionState::Failed | AgentSessionState::Cancelled => rgb(0xfca5a5),
        AgentSessionState::AwaitingApproval | AgentSessionState::NeedsInput => rgb(0xfef3c7),
        AgentSessionState::Archived => rgb(0x64748b),
        _ => rgb(0x93c5fd),
    }
}

static DARK_THEME_ACTIVE: AtomicBool = AtomicBool::new(true);

fn rgb(value: u32) -> gpui::Rgba {
    let value = if DARK_THEME_ACTIVE.load(Ordering::Relaxed) {
        value
    } else {
        match value {
            0x111318 => 0xf8fafc,
            0x14161a | 0x17191f => 0xf1f5f9,
            0x1b1d24 | 0x20242c => 0xe2e8f0,
            0x242833 => 0xcbd5e1,
            0x293244 => 0xbfdbfe,
            0x30343f | 0x3b4555 => 0xcbd5e1,
            0x0f1115 => 0xffffff,
            0xe5e7eb | 0xf3f4f6 => 0x0f172a,
            0xb7c0d0 | 0x8f98a6 | 0x94a3b8 => 0x475569,
            0x93c5fd | 0xbfdbfe => 0x1d4ed8,
            0x1d4ed8 => 0x1e40af,
            0x1e293b | 0x1f4f78 => 0xdbeafe,
            0x3a1f24 => 0xfee2e2,
            0x3b2f66 => 0xf3e8ff,
            0x493b1a => 0xfef3c7,
            0x60a5fa => 0x2563eb,
            0x7f1d1d => 0xfecaca,
            0x9ad7bd => 0x047857,
            0xfca5a5 => 0xb91c1c,
            0xfda4af => 0x9f1239,
            0xfef3c7 => 0x92400e,
            0xcbd5e1 | 0xdbeafe => 0x1e3a8a,
            0xd1fae5 => 0x065f46,
            0xe9d5ff => 0x6b21a8,
            _ => value,
        }
    };
    gpui::rgb(value)
}

fn change_color(kind: loom_workspace::WorkspaceChangeKind) -> gpui::Rgba {
    match kind {
        loom_workspace::WorkspaceChangeKind::Created => rgb(0x9ad7bd),
        loom_workspace::WorkspaceChangeKind::Deleted => rgb(0xfca5a5),
        loom_workspace::WorkspaceChangeKind::Modified => rgb(0xfef3c7),
    }
}

fn session_state_for_run(state: AgentRunState) -> AgentSessionState {
    match state {
        AgentRunState::Planning => AgentSessionState::Planning,
        AgentRunState::Executing => AgentSessionState::Executing,
        AgentRunState::AwaitingApproval => AgentSessionState::AwaitingApproval,
        AgentRunState::Paused => AgentSessionState::Paused,
        AgentRunState::NeedsInput => AgentSessionState::NeedsInput,
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
        AgentSessionState::Archived => "archived",
    }
}

const fn run_state_name(state: AgentRunState) -> &'static str {
    match state {
        AgentRunState::Planning => "planning",
        AgentRunState::Executing => "executing",
        AgentRunState::AwaitingApproval => "awaiting approval",
        AgentRunState::Paused => "paused",
        AgentRunState::NeedsInput => "needs input",
        AgentRunState::Evaluating => "evaluating",
        AgentRunState::Completed => "completed",
        AgentRunState::Failed => "failed",
        AgentRunState::Cancelled => "cancelled",
    }
}

fn negotiate(connection: &ClientConnection) -> Result<(), LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Negotiate {
        client_version: CURRENT_PROTOCOL_VERSION,
        capabilities: CapabilitySet::new([
            Capability::CreateAgentSession,
            Capability::ReadAgentSession,
            Capability::ControlAgentSession,
            Capability::SubscribeSessionEvents,
            Capability::StartAgentRun,
            Capability::ReadAgentRun,
            Capability::ControlAgentRun,
            Capability::PauseAgentRun,
            Capability::ResumeAgentRun,
            Capability::ApproveAgentAction,
            Capability::OpenWorkspace,
            Capability::ReadWorkspace,
            Capability::ReadVcsStatus,
            Capability::ReadVcsDiff,
            Capability::ReadTask,
            Capability::StartTask,
            Capability::ControlTask,
            Capability::ReadTaskEvidence,
            Capability::JsonProtocol,
        ]),
    }));
    match response.result? {
        ServerResponse::Negotiated(_) => Ok(()),
        response => Err(LoomView::unexpected_response("negotiation", response)),
    }
}

fn open_workspace(
    connection: &ClientConnection,
    project_id: ProjectId,
    root: &Path,
) -> Result<(), LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::OpenWorkspace {
        project_id,
        root: root.display().to_string(),
    }));
    match response.result? {
        ServerResponse::WorkspaceOpened(_) => Ok(()),
        response => Err(LoomView::unexpected_response("workspace open", response)),
    }
}

fn create_session(
    connection: &ClientConnection,
    project_id: ProjectId,
    name: &str,
) -> Result<AgentSessionSnapshot, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::CreateAgentSession {
        project_id,
        name: name.to_owned(),
    }));
    match response.result? {
        ServerResponse::AgentSessionCreated(snapshot) => Ok(snapshot),
        response => Err(LoomView::unexpected_response("session creation", response)),
    }
}

fn list_sessions(
    connection: &ClientConnection,
    project_id: ProjectId,
) -> Result<Vec<AgentSessionSnapshot>, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::ListAgentSessions {
        project_id: Some(project_id),
        include_archived: false,
    }));
    match response.result? {
        ServerResponse::AgentSessions { sessions } => Ok(sessions),
        response => Err(LoomView::unexpected_response("session list", response)),
    }
}

fn list_projects(connection: &ClientConnection) -> Result<Vec<ProjectSnapshot>, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::ListProjects));
    match response.result? {
        ServerResponse::Projects { projects } => Ok(projects),
        response => Err(LoomView::unexpected_response("project list", response)),
    }
}

fn select_remote_project(
    projects: &[ProjectSnapshot],
    requested_root: Option<&Path>,
) -> Result<ProjectSnapshot, LoomError> {
    let project = requested_root
        .and_then(|root| {
            let requested = root.to_string_lossy();
            projects.iter().find(|project| {
                project
                    .root
                    .as_deref()
                    .is_some_and(|project_root| project_root == requested)
            })
        })
        .or_else(|| projects.first())
        .cloned()
        .ok_or_else(|| {
            LoomError::new(
                ErrorCode::NotFound,
                "remote backend has no open projects",
                false,
            )
        })?;
    Ok(project)
}

fn list_models(connection: &ClientConnection) -> Result<Vec<ModelId>, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::ListModels));
    match response.result? {
        ServerResponse::Models { models } => Ok(models.into_iter().map(|model| model.id).collect()),
        response => Err(LoomView::unexpected_response("model list", response)),
    }
}

fn list_provider_ids(connection: &ClientConnection) -> Result<Vec<ProviderId>, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::ListProviders));
    match response.result? {
        ServerResponse::Providers { providers } => {
            Ok(providers.into_iter().map(|provider| provider.id).collect())
        }
        response => Err(LoomView::unexpected_response("provider list", response)),
    }
}

fn discover_provider_models(connection: &ClientConnection) -> Result<Vec<ModelId>, LoomError> {
    let response = connection.request(RequestEnvelope::new(
        ClientRequest::DiscoverProviderModels {
            provider_id: ProviderId::new(GITHUB_COPILOT_PROVIDER_ID),
        },
    ));
    match response.result? {
        ServerResponse::Models { models } => Ok(models.into_iter().map(|model| model.id).collect()),
        response => Err(LoomView::unexpected_response(
            "GitHub Copilot model discovery",
            response,
        )),
    }
}

fn start_run(
    connection: &ClientConnection,
    session: &AgentSessionSnapshot,
    workspace_root: &Path,
    model: &ModelId,
    task: &str,
) -> Result<AgentRunSnapshot, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::StartAgentRun {
        session_id: session.id,
        task: task.to_owned(),
        model: model.clone(),
        workspace_root: workspace_root.display().to_string(),
        system_instructions: Some(
            "Work methodically, use the available tools, and report validation.".to_owned(),
        ),
        repository_instructions: Some(
            "Keep the change focused and provide reviewable evidence.".to_owned(),
        ),
    }));
    match response.result? {
        ServerResponse::AgentRunStarted(run) => Ok(run),
        response => Err(LoomView::unexpected_response("agent run start", response)),
    }
}

#[derive(Clone, Debug)]
struct UiOptions {
    workspace: Option<PathBuf>,
    task: String,
    demo: bool,
    model: ModelId,
    endpoint: Option<String>,
    api_key: Option<String>,
    remote: Option<String>,
    token: Option<String>,
}

impl UiOptions {
    fn parse<I>(args: I) -> Result<Self, LoomError>
    where
        I: IntoIterator<Item = String>,
    {
        let mut workspace = None;
        let mut task = "make a small repository change and validate it".to_owned();
        let mut demo = false;
        let mut model = env::var("LOOM_MODEL")
            .map(ModelId::new)
            .unwrap_or_else(|_| ModelId::new(GITHUB_COPILOT_DEFAULT_MODEL));
        let mut endpoint = env::var("LOOM_OPENAI_ENDPOINT").ok();
        let api_key = env::var("LOOM_API_KEY").ok();
        let mut remote = env::var("LOOM_REMOTE_URL").ok();
        let token = env::var("LOOM_TOKEN").ok();
        let mut args = args.into_iter().skip(1);
        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--workspace" => {
                    let value = args
                        .next()
                        .ok_or_else(|| LoomError::invalid_request("--workspace requires a path"))?;
                    workspace = Some(PathBuf::from(value));
                    demo = false;
                }
                "--task" => {
                    task = args.next().ok_or_else(|| {
                        LoomError::invalid_request("--task requires a description")
                    })?;
                    if task.trim().is_empty() {
                        return Err(LoomError::invalid_request(
                            "--task requires a non-empty description",
                        ));
                    }
                }
                "--model" => {
                    model = ModelId::new(args.next().ok_or_else(|| {
                        LoomError::invalid_request("--model requires a model id")
                    })?);
                    if model.as_str().trim().is_empty() {
                        return Err(LoomError::invalid_request(
                            "--model requires a non-empty model id",
                        ));
                    }
                }
                "--endpoint" => {
                    let value = args
                        .next()
                        .ok_or_else(|| LoomError::invalid_request("--endpoint requires a URL"))?;
                    if value.trim().is_empty() {
                        return Err(LoomError::invalid_request(
                            "--endpoint requires a non-empty URL",
                        ));
                    }
                    endpoint = Some(value);
                }
                "--remote" => {
                    let value = args
                        .next()
                        .ok_or_else(|| LoomError::invalid_request("--remote requires a URL"))?;
                    if value.trim().is_empty() {
                        return Err(LoomError::invalid_request(
                            "--remote requires a non-empty URL",
                        ));
                    }
                    remote = Some(value);
                    demo = false;
                }
                "--demo" => {
                    workspace = None;
                    demo = true;
                    model = ModelId::new("deterministic/demo");
                }
                "--help" | "-h" => {
                    return Err(LoomError::invalid_request(
                        "usage: loom-ui [--workspace PATH] [--task DESCRIPTION] [--model ID] [--endpoint URL] [--remote URL] [--demo]",
                    ));
                }
                unknown => {
                    return Err(LoomError::invalid_request(format!(
                        "unknown argument '{unknown}'; use --help for usage"
                    )));
                }
            }
        }
        Ok(Self {
            workspace,
            task,
            demo,
            model,
            endpoint,
            api_key,
            remote,
            token,
        })
    }
}

fn prepare_workspace(options: &UiOptions) -> Result<(PathBuf, bool), LoomError> {
    if let Some(workspace) = &options.workspace {
        let root = fs::canonicalize(workspace).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!(
                    "could not open workspace '{}': {error}",
                    workspace.display()
                ),
                false,
            )
        })?;
        if !root.is_dir() {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("workspace '{}' is not a directory", root.display()),
                false,
            ));
        }
        return Ok((root, false));
    }

    if !options.demo {
        let root = fs::canonicalize(env::current_dir().map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not determine current workspace: {error}"),
                false,
            )
        })?)
        .map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not open current workspace: {error}"),
                false,
            )
        })?;
        return Ok((root, false));
    }

    let root = env::temp_dir().join("loom-m5-ui-demo");
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
            "Workspace used by the Loom M5 agent workspace demo.\n",
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
    Ok((root, options.demo))
}

fn backend_persistence_path(root: &Path) -> Result<PathBuf, LoomError> {
    let state_root = env::var_os("LOOM_STATE_DIR")
        .map(PathBuf::from)
        .or_else(|| env::var_os("XDG_STATE_HOME").map(PathBuf::from))
        .or_else(|| {
            env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("state"))
        })
        .unwrap_or_else(|| env::temp_dir().join("loom-state"));
    let digest = Sha256::digest(root.to_string_lossy().as_bytes());
    let project_key = digest
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(state_root
        .join("loom")
        .join("projects")
        .join(format!("{project_key}.json")))
}

fn stable_project_id(root: &Path) -> ProjectId {
    let digest = Sha256::digest(root.to_string_lossy().as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    ProjectId::from_uuid(Uuid::from_bytes(bytes))
}

fn main() {
    let options = match UiOptions::parse(std::env::args()) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("could not parse Loom UI arguments: {error}");
            std::process::exit(1);
        }
    };
    gpui_platform::application()
        .with_assets(gpui_kit_assets::AllAssets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);
            cx.bind_keys([
                KeyBinding::new("backspace", Backspace, Some("Composer")),
                KeyBinding::new("delete", Delete, Some("Composer")),
                KeyBinding::new("left", Left, Some("Composer")),
                KeyBinding::new("right", Right, Some("Composer")),
                KeyBinding::new("cmd-a", SelectAll, Some("Composer")),
                KeyBinding::new("ctrl-a", SelectAll, Some("Composer")),
                KeyBinding::new("home", Home, Some("Composer")),
                KeyBinding::new("end", End, Some("Composer")),
                KeyBinding::new("cmd-v", Paste, Some("Composer")),
                KeyBinding::new("ctrl-v", Paste, Some("Composer")),
                KeyBinding::new("cmd-c", Copy, Some("Composer")),
                KeyBinding::new("ctrl-c", Copy, Some("Composer")),
                KeyBinding::new("enter", Submit, Some("Composer")),
            ]);
            let view = match LoomView::try_new(&options, cx.focus_handle(), cx.focus_handle()) {
                Ok(view) => view,
                Err(error) => {
                    eprintln!("could not initialize Loom UI: {error}");
                    cx.quit();
                    return;
                }
            };
            let bounds = Bounds::centered(None, size(px(1200.), px(780.)), cx);
            let window = match cx.open_window(
                WindowOptions {
                    focus: true,
                    titlebar: Some(TitlebarOptions {
                        title: None,
                        appears_transparent: true,
                        traffic_light_position: Some(point(px(9.), px(9.))),
                    }),
                    app_owns_titlebar_drag: true,
                    window_background: WindowBackgroundAppearance::Transparent,
                    window_decorations: Some(WindowDecorations::Client),
                    is_movable: true,
                    is_resizable: true,
                    is_minimizable: true,
                    window_min_size: Some(size(px(720.), px(480.))),
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |_, cx| cx.new(|_| view),
            ) {
                Ok(window) => window,
                Err(error) => {
                    eprintln!("failed to open Loom window: {error}");
                    cx.quit();
                    return;
                }
            };
            if let Err(error) = window.update(cx, |view, window, cx| {
                view.composer_focus_handle.focus(window, cx);
                view.select_theme(ThemeChoice::System, window, cx);
                cx.activate(true);
            }) {
                eprintln!("failed to focus Loom composer: {error}");
                cx.quit();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_buffer_input_transforms_utf16_ranges_and_selection() {
        let mut buffer = TextBufferState::new("a😀c\nsecond");
        let replacement = buffer.replace_utf16(Some(1..3), "x");
        assert_eq!(replacement, 1..2);
        assert_eq!(buffer.text, "axc\nsecond");

        buffer.move_to(2, false);
        buffer.select_to(4);
        assert_eq!(buffer.selected_range, 2..4);
        buffer.replace_utf16(None, "😀");
        assert_eq!(buffer.text, "ax😀second");
        assert_eq!(buffer.cursor_offset(), 6);
        assert_eq!(buffer.offset_to_utf16(6), 4);
        assert_eq!(buffer.offset_from_utf16(4), 6);
    }

    #[test]
    fn bounded_projection_is_explicit() {
        let value = bounded_to("abcdef", 3);
        assert_eq!(value, "abc\n...[output truncated]");
        assert!(bounded_to("😀😀", 4).starts_with('😀'));
    }

    #[test]
    fn ui_options_allow_explicit_workspace_and_task() {
        let options = UiOptions::parse([
            "loom-ui".to_owned(),
            "--workspace".to_owned(),
            "/tmp/project".to_owned(),
            "--task".to_owned(),
            "fix the agent flow".to_owned(),
            "--model".to_owned(),
            "gpt-4o-mini".to_owned(),
            "--endpoint".to_owned(),
            "http://127.0.0.1:8000/v1/chat/completions".to_owned(),
            "--remote".to_owned(),
            "ws://127.0.0.1:8080/ws".to_owned(),
        ])
        .unwrap();
        assert_eq!(options.workspace, Some(PathBuf::from("/tmp/project")));
        assert_eq!(options.task, "fix the agent flow");
        assert_eq!(options.model.as_str(), "gpt-4o-mini");
        assert_eq!(
            options.endpoint.as_deref(),
            Some("http://127.0.0.1:8000/v1/chat/completions")
        );
        assert_eq!(options.remote.as_deref(), Some("ws://127.0.0.1:8080/ws"));
        assert!(!options.demo);
    }

    #[test]
    fn project_identity_and_persistence_path_are_stable_per_workspace() {
        let first = Path::new("/tmp/loom-project");
        let second = Path::new("/tmp/other-project");
        assert_eq!(stable_project_id(first), stable_project_id(first));
        assert_ne!(stable_project_id(first), stable_project_id(second));
        assert_ne!(
            backend_persistence_path(first).unwrap(),
            backend_persistence_path(second).unwrap()
        );
    }
}
