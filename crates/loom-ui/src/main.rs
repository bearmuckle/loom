use std::{
    fs,
    ops::Range,
    path::{Path, PathBuf},
};

use gpui::{
    App, Application, Bounds, ClickEvent, ClipboardItem, Context, CursorStyle, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Focusable, GlobalElementId,
    KeyBinding, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad,
    Pixels, Point, Render, ShapedLine, SharedString, Style, TextRun, TitlebarOptions,
    UTF16Selection, Window, WindowBackgroundAppearance, WindowBounds, WindowDecorations,
    WindowOptions, actions, div, fill, point, prelude::*, px, relative, rgb, rgba, size,
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
    BufferEdit, BufferId, BufferSnapshot, EditorLayoutSnapshot, FileTreeEntry, SearchMatch,
    SearchQuery, TextRange,
};

actions!(
    loom_editor,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectAll,
        Home,
        End,
        SelectHome,
        SelectEnd,
        Paste,
        Copy,
        Cut,
        Save,
        Undo,
        Redo
    ]
);

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
    editor_input: TextBufferState,
    editor_focus_handle: FocusHandle,
    editor_layout_cache: Option<EditorLayoutCache>,
    backend_status: BackendStatus,
    demo_workspace: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum TimelineItem {
    Plan(Vec<String>),
    Assistant(String),
    ToolRequested { name: String, arguments: String },
    Approval { name: String, active: bool },
    ToolStarted(String),
    ToolOutput(String),
    ToolCompleted { name: String, success: bool },
    Status(String),
    Error { operation: String, error: LoomError },
    Summary { text: String, evidence: Vec<String> },
}

#[derive(Clone, Debug)]
enum BackendStatus {
    Connected,
    Error(LoomError),
}

#[derive(Clone, Debug)]
struct TextBufferState {
    text: String,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
}

#[derive(Clone, Debug)]
struct CachedEditorLine {
    range: Range<usize>,
    layout: ShapedLine,
}

#[derive(Clone, Debug)]
struct EditorLayoutCache {
    bounds: Bounds<Pixels>,
    line_height: Pixels,
    lines: Vec<CachedEditorLine>,
}

struct TextEditorElement {
    view: Entity<LoomView>,
}

struct TextEditorPrepaint {
    bounds: Bounds<Pixels>,
    lines: Vec<(Range<usize>, ShapedLine)>,
    selections: Vec<PaintQuad>,
    cursor: Option<PaintQuad>,
}

impl TextBufferState {
    fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            selected_range: text.len()..text.len(),
            text,
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
        let offset = offset.min(self.text.len());
        self.text
            .get(..offset)
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

    fn offset_for_line_and_column(&self, line: usize, column: usize) -> usize {
        let ranges = self.line_ranges();
        let range = &ranges[line.min(ranges.len().saturating_sub(1))];
        let end = range.end;
        let mut offset = range.start;
        for (index, character) in self.text[range.clone()].char_indices() {
            if index >= column {
                break;
            }
            offset = range.start + index + character.len_utf8();
        }
        offset.min(end)
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

    fn move_vertical(&mut self, direction: i32, extend: bool) {
        let (line, column) = self.line_and_column(self.cursor_offset());
        let target = if direction.is_negative() {
            line.saturating_sub(direction.unsigned_abs() as usize)
        } else {
            line.saturating_add(direction as usize)
        };
        let target = target.min(self.line_ranges().len().saturating_sub(1));
        self.move_to(self.offset_for_line_and_column(target, column), extend);
    }

    fn move_home(&mut self, extend: bool) {
        let (line, _) = self.line_and_column(self.cursor_offset());
        let offset = self.line_ranges()[line].start;
        self.move_to(offset, extend);
    }

    fn move_end(&mut self, extend: bool) {
        let (line, _) = self.line_and_column(self.cursor_offset());
        let offset = self.line_ranges()[line].end;
        self.move_to(offset, extend);
    }

    fn replace_range(&mut self, range: Range<usize>, replacement: &str) {
        let replacement = replacement.replace("\r\n", "\n").replace('\r', "\n");
        self.text.replace_range(range.clone(), &replacement);
        let cursor = range.start + replacement.len();
        self.selected_range = cursor..cursor;
        self.selection_reversed = false;
        self.marked_range = None;
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
        let replacement_range = range.start..range.start + replacement.len();
        self.replace_range(range, &replacement);
        replacement_range
    }

    fn select_all(&mut self) {
        self.selected_range = 0..self.text.len();
        self.selection_reversed = false;
    }

    fn line_count(&self) -> usize {
        self.line_ranges().len().max(1)
    }
}

impl LoomView {
    fn editor_utf8_index_for_point(&self, point: Point<Pixels>) -> Option<usize> {
        let cache = self.editor_layout_cache.as_ref()?;
        let local = cache.bounds.localize(&point)?;
        let line_index = (local.y / cache.line_height).floor().max(0.) as usize;
        let line = cache.lines.get(line_index)?;
        Some(
            line.range.start
                + line
                    .layout
                    .closest_index_for_x(local.x)
                    .min(line.range.len()),
        )
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
        let range = self.editor_input.range_from_utf16(&range_utf16);
        actual_range.replace(self.editor_input.range_to_utf16(&range));
        self.editor_input.text.get(range).map(str::to_owned)
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self
                .editor_input
                .range_to_utf16(&self.editor_input.selected_range),
            reversed: self.editor_input.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.editor_input
            .marked_range
            .as_ref()
            .map(|range| self.editor_input.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.editor_input.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_input_text(range_utf16, new_text, None, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_input_text(range_utf16, new_text, new_selected_range_utf16, cx);
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let cache = self.editor_layout_cache.as_ref()?;
        let range = self.editor_input.range_from_utf16(&range_utf16);
        let line_index = cache
            .lines
            .iter()
            .position(|line| range.start <= line.range.end && range.end >= line.range.start)?;
        let line = &cache.lines[line_index];
        let start = range.start.clamp(line.range.start, line.range.end);
        let end = range.end.clamp(line.range.start, line.range.end);
        let top = cache.bounds.top() + cache.line_height * line_index;
        Some(Bounds::from_corners(
            point(
                cache.bounds.left() + line.layout.x_for_index(start - line.range.start),
                top,
            ),
            point(
                cache.bounds.left() + line.layout.x_for_index(end - line.range.start),
                top + cache.line_height,
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        self.editor_utf8_index_for_point(point)
            .map(|offset| self.editor_input.offset_to_utf16(offset))
    }
}

impl Focusable for LoomView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.editor_focus_handle.clone()
    }
}

impl IntoElement for TextEditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextEditorElement {
    type RequestLayoutState = ();
    type PrepaintState = TextEditorPrepaint;

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
        let line_count = self.view.read(cx).editor_input.line_count();
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
        let text = input.editor_input.text.clone();
        let ranges = input.editor_input.line_ranges();
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let mut lines = Vec::with_capacity(ranges.len());
        let markers = input
            .active_buffer
            .and_then(|buffer_id| {
                input
                    .open_buffers
                    .iter()
                    .find(|buffer| buffer.id == buffer_id)
            })
            .map(|buffer| buffer.agent_markers.clone())
            .unwrap_or_default();
        for (line_index, range) in ranges.into_iter().enumerate() {
            let line_text = text[range.clone()].to_owned();
            let line_number = line_index as u32 + 1;
            let run = TextRun {
                len: line_text.len(),
                font: style.font(),
                color: if markers.iter().any(|marker| {
                    marker.start_line <= line_number && marker.end_line >= line_number
                }) {
                    rgb(0xfbbf24).into()
                } else {
                    style.color
                },
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
        let mut selections = Vec::new();
        if !input.editor_input.selected_range.is_empty() {
            for (line_index, (range, line)) in lines.iter().enumerate() {
                let start = input.editor_input.selected_range.start.max(range.start);
                let end = input.editor_input.selected_range.end.min(range.end);
                if start >= end {
                    continue;
                }
                let top = bounds.top() + line_height * line_index;
                selections.push(fill(
                    Bounds::from_corners(
                        point(bounds.left() + line.x_for_index(start - range.start), top),
                        point(
                            bounds.left() + line.x_for_index(end - range.start),
                            top + line_height,
                        ),
                    ),
                    rgba(0x335b8def),
                ));
            }
        }

        let cursor = if input.editor_input.selected_range.is_empty() {
            let cursor_offset = input.editor_input.cursor_offset();
            lines
                .iter()
                .enumerate()
                .find_map(|(line_index, (range, line))| {
                    if cursor_offset < range.start || cursor_offset > range.end {
                        return None;
                    }
                    let top = bounds.top() + line_height * line_index;
                    Some(fill(
                        Bounds::new(
                            point(
                                bounds.left() + line.x_for_index(cursor_offset - range.start),
                                top,
                            ),
                            size(px(2.), line_height),
                        ),
                        rgb(0x60a5fa),
                    ))
                })
        } else {
            None
        };

        TextEditorPrepaint {
            bounds,
            lines,
            selections,
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
        let focus_handle = self.view.read(cx).editor_focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(prepaint.bounds, self.view.clone()),
            cx,
        );
        for selection in prepaint.selections.drain(..) {
            window.paint_quad(selection);
        }
        let line_height = window.line_height();
        for (line_index, (_, line)) in prepaint.lines.iter().enumerate() {
            let _ = line.paint(
                point(
                    prepaint.bounds.left(),
                    prepaint.bounds.top() + line_height * line_index,
                ),
                line_height,
                window,
                cx,
            );
        }
        if focus_handle.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }
        let layout = EditorLayoutCache {
            bounds: prepaint.bounds,
            line_height,
            lines: prepaint
                .lines
                .iter()
                .map(|(range, layout)| CachedEditorLine {
                    range: range.clone(),
                    layout: layout.clone(),
                })
                .collect(),
        };
        self.view.update(cx, |view, _| {
            view.editor_layout_cache = Some(layout);
        });
    }
}

impl LoomView {
    fn try_new(options: &UiOptions, editor_focus_handle: FocusHandle) -> Result<Self, LoomError> {
        let (workspace_root, demo_workspace) = prepare_workspace(options)?;
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate(&connection)?;
        let provider_count = provider_count(&connection)?;
        let model = ModelId::new("deterministic/demo");
        let (session, run) = start_run(&connection, &workspace_root, &model, options.task.clone())?;
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
            editor_input: TextBufferState::new(""),
            editor_focus_handle,
            editor_layout_cache: None,
            backend_status: BackendStatus::Connected,
            demo_workspace,
        };
        view.refresh_workspace_surfaces(true)?;
        view.collect_events()?;
        Ok(view)
    }

    fn refresh_workspace_surfaces(&mut self, open_default: bool) -> Result<(), LoomError> {
        self.backend_status = BackendStatus::Connected;
        let file_tree = self
            .connection
            .request(RequestEnvelope::new(ClientRequest::GetFileTree {
                project_id: self.project_id,
            }));
        match file_tree.result {
            Ok(ServerResponse::FileTree { entries }) => self.file_tree = entries,
            Err(error) => self.record_backend_error("file tree refresh", error),
            Ok(response) => self.record_unexpected_response("file tree refresh", response),
        }

        if open_default && self.open_buffers.is_empty() {
            let path = self
                .file_tree
                .iter()
                .find(|entry| {
                    entry.kind == loom_workspace::FileTreeEntryKind::File
                        && entry.path == "README.md"
                })
                .or_else(|| {
                    self.file_tree
                        .iter()
                        .find(|entry| entry.kind == loom_workspace::FileTreeEntryKind::File)
                })
                .map(|entry| entry.path.clone());
            if let Some(path) = path {
                self.open_file_path(path);
            } else {
                self.record_backend_error(
                    "open initial editor buffer",
                    LoomError::new(
                        ErrorCode::NotFound,
                        "workspace has no file that can be opened",
                        false,
                    ),
                );
            }
        }

        let layout =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::GetEditorLayout {
                    project_id: self.project_id,
                }));
        match layout.result {
            Ok(ServerResponse::EditorLayout(layout)) => self.editor_layout = Some(layout),
            Err(error) => self.record_backend_error("editor layout refresh", error),
            Ok(response) => self.record_unexpected_response("editor layout refresh", response),
        }

        let services = self.connection.request(RequestEnvelope::new(
            ClientRequest::DiscoverLanguageServices {
                project_id: self.project_id,
            },
        ));
        match services.result {
            Ok(ServerResponse::LanguageServices { services }) => self.language_services = services,
            Err(error) => self.record_backend_error("language service discovery", error),
            Ok(response) => self.record_unexpected_response("language service discovery", response),
        }

        let active_path = self.active_path();
        if let Some(path) = active_path {
            let started = self.connection.request(RequestEnvelope::new(
                ClientRequest::StartLanguageService {
                    project_id: self.project_id,
                    path: path.clone(),
                },
            ));
            match started.result {
                Ok(ServerResponse::LanguageServices { services }) => {
                    self.language_services = services
                }
                Err(error) => self.record_backend_error("language service start", error),
                Ok(response) => self.record_unexpected_response("language service start", response),
            }

            let diagnostics =
                self.connection
                    .request(RequestEnvelope::new(ClientRequest::GetDiagnostics {
                        project_id: self.project_id,
                        path: path.clone(),
                    }));
            match diagnostics.result {
                Ok(ServerResponse::Diagnostics {
                    diagnostics,
                    path: _,
                }) => self.diagnostics = diagnostics,
                Err(error) => self.record_backend_error("diagnostics refresh", error),
                Ok(response) => self.record_unexpected_response("diagnostics refresh", response),
            }

            let symbols =
                self.connection
                    .request(RequestEnvelope::new(ClientRequest::GetSymbols {
                        project_id: self.project_id,
                        path,
                    }));
            match symbols.result {
                Ok(ServerResponse::Symbols { symbols, path: _ }) => self.symbols = symbols,
                Err(error) => self.record_backend_error("outline refresh", error),
                Ok(response) => self.record_unexpected_response("outline refresh", response),
            }
        } else {
            self.diagnostics.clear();
            self.symbols.clear();
            self.record_status("No active file for language diagnostics".to_owned());
        }

        let search =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::SearchWorkspace {
                    project_id: self.project_id,
                    query: SearchQuery::literal("TODO"),
                }));
        match search.result {
            Ok(ServerResponse::SearchMatches { matches }) => self.search_matches = matches,
            Err(error) => self.record_backend_error("workspace search refresh", error),
            Ok(response) => self.record_unexpected_response("workspace search refresh", response),
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
            Err(error) => {
                self.vcs_error = Some(error.to_string());
                self.record_backend_error("VCS status refresh", error);
            }
            Ok(response) => self.record_unexpected_response("VCS status refresh", response),
        }
        let external = self.connection.request(RequestEnvelope::new(
            ClientRequest::MarkExternalEditorChanges {
                project_id: self.project_id,
            },
        ));
        match external.result {
            Ok(ServerResponse::EditorBuffers { buffers }) => {
                for buffer in buffers {
                    if let Some(existing) = self
                        .open_buffers
                        .iter_mut()
                        .find(|existing| existing.id == buffer.id)
                    {
                        *existing = buffer;
                    }
                }
            }
            Err(error) => self.record_backend_error("external change refresh", error),
            Ok(response) => self.record_unexpected_response("external change refresh", response),
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
        for task_id in ids {
            let response = self
                .connection
                .request(RequestEnvelope::new(ClientRequest::GetTask {
                    project_id: self.project_id,
                    task_id,
                }));
            match response.result {
                Ok(ServerResponse::Task(task)) => {
                    if let Some(existing) = self
                        .task_results
                        .iter_mut()
                        .find(|existing| existing.id == task.id)
                    {
                        *existing = task;
                    } else {
                        self.task_results.push(task);
                    }
                }
                Err(error) => self.record_backend_error("task refresh", error),
                Ok(response) => self.record_unexpected_response("task refresh", response),
            }
        }
    }

    fn record_status(&mut self, status: String) {
        self.timeline.push(TimelineItem::Status(status));
    }

    fn record_backend_error(&mut self, operation: &str, error: LoomError) {
        self.backend_status = BackendStatus::Error(error.clone());
        self.timeline
            .push(backend_error_timeline_item(operation, &error));
    }

    fn record_unexpected_response(&mut self, operation: &str, response: ServerResponse) {
        self.record_backend_error(operation, unexpected_response(operation, response));
    }

    fn active_path(&self) -> Option<String> {
        let active_id = self.active_buffer?;
        self.open_buffers
            .iter()
            .find(|buffer| buffer.id == active_id)
            .map(|buffer| buffer.path.clone())
    }

    fn replace_input_text(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        cx: &mut Context<Self>,
    ) {
        let Some(buffer_id) = self.active_buffer else {
            return;
        };
        let previous = self.editor_input.clone();
        let range = range_utf16
            .as_ref()
            .map(|range| self.editor_input.range_from_utf16(range))
            .or_else(|| self.editor_input.marked_range.clone())
            .unwrap_or_else(|| self.editor_input.selected_range.clone());
        let replacement = new_text.replace("\r\n", "\n").replace('\r', "\n");
        let edit = BufferEdit {
            range: TextRange::new(range.start, range.end),
            replacement: replacement.clone(),
        };
        let replacement_range = self
            .editor_input
            .replace_utf16(range_utf16.clone(), &replacement);
        if let Some(selected_range_utf16) = &new_selected_range_utf16 {
            let replacement_state = TextBufferState::new(replacement.clone());
            let selected = replacement_state.range_from_utf16(selected_range_utf16);
            self.editor_input.selected_range =
                replacement_range.start + selected.start..replacement_range.start + selected.end;
            self.editor_input.selection_reversed = false;
        }
        if new_selected_range_utf16.is_some() && !replacement.is_empty() {
            self.editor_input.marked_range = Some(replacement_range);
        }

        let response =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::EditEditorBuffer {
                    project_id: self.project_id,
                    buffer_id,
                    edit,
                }));
        match response.result {
            Ok(ServerResponse::EditorBuffer(buffer)) => {
                self.replace_buffer_snapshot(buffer);
                self.backend_status = BackendStatus::Connected;
            }
            Err(error) => {
                self.editor_input = previous;
                self.record_backend_error("editor input", error);
            }
            Ok(response) => {
                self.editor_input = previous;
                self.record_unexpected_response("editor input", response);
            }
        }
        cx.notify();
    }

    fn move_editor_left(&mut self, extend: bool, cx: &mut Context<Self>) {
        if self.editor_input.selected_range.is_empty() || extend {
            let offset = self
                .editor_input
                .previous_boundary(self.editor_input.cursor_offset());
            self.editor_input.move_to(offset, extend);
        } else {
            self.editor_input
                .move_to(self.editor_input.selected_range.start, false);
        }
        cx.notify();
    }

    fn move_editor_right(&mut self, extend: bool, cx: &mut Context<Self>) {
        if self.editor_input.selected_range.is_empty() || extend {
            let offset = self
                .editor_input
                .next_boundary(self.editor_input.cursor_offset());
            self.editor_input.move_to(offset, extend);
        } else {
            self.editor_input
                .move_to(self.editor_input.selected_range.end, false);
        }
        cx.notify();
    }

    fn delete_backward(&mut self, cx: &mut Context<Self>) {
        if self.editor_input.selected_range.is_empty() {
            let cursor = self.editor_input.cursor_offset();
            if cursor == 0 {
                return;
            }
            self.editor_input.selected_range = self.editor_input.previous_boundary(cursor)..cursor;
            self.editor_input.selection_reversed = false;
        }
        self.replace_input_text(None, "", None, cx);
    }

    fn delete_forward(&mut self, cx: &mut Context<Self>) {
        if self.editor_input.selected_range.is_empty() {
            let cursor = self.editor_input.cursor_offset();
            if cursor >= self.editor_input.text.len() {
                return;
            }
            self.editor_input.selected_range = cursor..self.editor_input.next_boundary(cursor);
            self.editor_input.selection_reversed = false;
        }
        self.replace_input_text(None, "", None, cx);
    }

    fn copy_editor_selection(&mut self, cx: &mut Context<Self>) {
        if !self.editor_input.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.editor_input.text[self.editor_input.selected_range.clone()].to_owned(),
            ));
        }
    }

    fn editor_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(offset) = self.editor_utf8_index_for_point(event.position) else {
            return;
        };
        if event.modifiers.shift {
            self.editor_input.select_to(offset);
        } else {
            self.editor_input.move_to(offset, false);
        }
        self.editor_focus_handle.focus(window);
        cx.notify();
    }

    fn editor_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        if let Some(offset) = self.editor_utf8_index_for_point(event.position) {
            self.editor_input.select_to(offset);
            cx.notify();
        }
    }

    fn editor_mouse_up(
        &mut self,
        _event: &MouseUpEvent,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        self.delete_backward(cx);
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        self.delete_forward(cx);
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        self.move_editor_left(false, cx);
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        self.move_editor_right(false, cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_editor_left(true, cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_editor_right(true, cx);
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        self.editor_input.move_vertical(-1, false);
        cx.notify();
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        self.editor_input.move_vertical(1, false);
        cx.notify();
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.editor_input.move_vertical(-1, true);
        cx.notify();
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.editor_input.move_vertical(1, true);
        cx.notify();
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.editor_input.select_all();
        cx.notify();
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.editor_input.move_home(false);
        cx.notify();
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.editor_input.move_end(false);
        cx.notify();
    }

    fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        self.editor_input.move_home(true);
        cx.notify();
    }

    fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.editor_input.move_end(true);
        cx.notify();
    }

    fn paste(&mut self, _: &Paste, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_input_text(None, &text, None, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        self.copy_editor_selection(cx);
    }

    fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        self.copy_editor_selection(cx);
        if !self.editor_input.selected_range.is_empty() {
            self.replace_input_text(None, "", None, cx);
        }
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
            self.record_backend_error("approval", error);
            self.pending_approval = Some(call);
            cx.notify();
            return;
        }
        if let Err(error) = self.collect_events() {
            self.record_backend_error("approval event stream", error);
        }
        cx.notify();
    }

    fn refresh(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        match workspace_snapshot(&self.connection, self.project_id) {
            Ok(snapshot) => self.workspace_entries = snapshot.entries.len(),
            Err(error) => self.record_backend_error("workspace refresh", error),
        }
        let _ = self.refresh_workspace_surfaces(false);
        if let Err(error) = self.collect_events() {
            self.record_backend_error("refresh event stream", error);
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
                self.activate_buffer(buffer);
                self.backend_status = BackendStatus::Connected;
            }
            Err(error) => self.record_backend_error("open file", error),
            Ok(response) => self.record_unexpected_response("open file", response),
        }
    }

    fn save_active(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.save_active_now(cx);
    }

    fn save_active_now(&mut self, cx: &mut Context<Self>) {
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
            Ok(ServerResponse::EditorBuffer(buffer)) => {
                self.replace_buffer_snapshot(buffer);
                self.backend_status = BackendStatus::Connected;
            }
            Err(error) => {
                if matches!(error.code, ErrorCode::ExternalChange | ErrorCode::Conflict) {
                    self.mark_active_external_change();
                }
                self.record_backend_error("save buffer", error);
            }
            Ok(response) => self.record_unexpected_response("save buffer", response),
        }
        cx.notify();
    }

    fn mark_active_external_change(&mut self) {
        let Some(buffer_id) = self.active_buffer else {
            return;
        };
        if let Some(buffer) = self
            .open_buffers
            .iter_mut()
            .find(|buffer| buffer.id == buffer_id)
        {
            buffer.external_change = true;
        }
    }

    fn reload_active(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(buffer_id) = self.active_buffer else {
            return;
        };
        let response =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::ReloadEditorBuffer {
                    project_id: self.project_id,
                    buffer_id,
                    discard_dirty: true,
                }));
        match response.result {
            Ok(ServerResponse::EditorBuffer(buffer)) => {
                self.replace_buffer_snapshot(buffer);
                self.backend_status = BackendStatus::Connected;
            }
            Err(error) => self.record_backend_error("reload buffer", error),
            Ok(response) => self.record_unexpected_response("reload buffer", response),
        }
        cx.notify();
    }

    fn undo_active(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.undo_active_now(cx);
    }

    fn undo_active_now(&mut self, cx: &mut Context<Self>) {
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
            Ok(ServerResponse::EditorBuffer(buffer)) => {
                self.replace_buffer_snapshot(buffer);
                self.backend_status = BackendStatus::Connected;
            }
            Err(error) => self.record_backend_error("undo buffer", error),
            Ok(response) => self.record_unexpected_response("undo buffer", response),
        }
        cx.notify();
    }

    fn redo_active(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.redo_active_now(cx);
    }

    fn redo_active_now(&mut self, cx: &mut Context<Self>) {
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
            Ok(ServerResponse::EditorBuffer(buffer)) => {
                self.replace_buffer_snapshot(buffer);
                self.backend_status = BackendStatus::Connected;
            }
            Err(error) => self.record_backend_error("redo buffer", error),
            Ok(response) => self.record_unexpected_response("redo buffer", response),
        }
        cx.notify();
    }

    fn save_action(&mut self, _: &Save, _: &mut Window, cx: &mut Context<Self>) {
        self.save_active_now(cx);
    }

    fn undo_action(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        self.undo_active_now(cx);
    }

    fn redo_action(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        self.redo_active_now(cx);
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
            Err(error) => self.record_backend_error("start task", error),
            Ok(response) => self.record_unexpected_response("start task", response),
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

    fn activate_buffer(&mut self, buffer: BufferSnapshot) {
        self.active_buffer = Some(buffer.id);
        if let Some(existing) = self
            .open_buffers
            .iter_mut()
            .find(|existing| existing.id == buffer.id)
        {
            *existing = buffer.clone();
        } else {
            self.open_buffers.push(buffer.clone());
        }
        self.editor_input.set_text(buffer.text);
        self.editor_layout_cache = None;
        self.diagnostics.clear();
        self.symbols.clear();
    }

    fn replace_buffer_snapshot(&mut self, buffer: BufferSnapshot) {
        self.active_buffer = Some(buffer.id);
        if let Some(existing) = self
            .open_buffers
            .iter_mut()
            .find(|existing| existing.id == buffer.id)
        {
            *existing = buffer.clone();
        } else {
            self.open_buffers.push(buffer.clone());
        }
        if self.editor_input.text != buffer.text {
            self.editor_input.set_text(buffer.text);
            self.editor_layout_cache = None;
        }
    }

    fn focus_tab(&mut self, buffer_id: BufferId, cx: &mut Context<Self>) {
        let Some(pane_id) = self
            .editor_layout
            .as_ref()
            .map(|layout| layout.focused_pane)
        else {
            self.record_backend_error(
                "focus editor tab",
                LoomError::invalid_state("editor layout is unavailable"),
            );
            cx.notify();
            return;
        };
        let response =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::FocusEditorTab {
                    project_id: self.project_id,
                    pane_id,
                    buffer_id,
                }));
        match response.result {
            Ok(ServerResponse::EditorLayout(layout)) => {
                self.editor_layout = Some(layout);
                if let Some(buffer) = self
                    .open_buffers
                    .iter()
                    .find(|buffer| buffer.id == buffer_id)
                    .cloned()
                {
                    self.activate_buffer(buffer);
                } else {
                    self.record_backend_error(
                        "focus editor tab",
                        LoomError::not_found("editor buffer", buffer_id),
                    );
                }
                self.backend_status = BackendStatus::Connected;
            }
            Err(error) => self.record_backend_error("focus editor tab", error),
            Ok(response) => self.record_unexpected_response("focus editor tab", response),
        }
        cx.notify();
    }

    fn restart(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Err(error) = self.restart_demo() {
            self.record_backend_error("new run", error);
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
        if self.demo_workspace {
            reset_demo_workspace(&self.workspace_root)?;
        }
        let (session, run) = start_run(
            &self.connection,
            &self.workspace_root,
            &self.model,
            self.task.clone(),
        )?;
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
        self.open_buffers.clear();
        self.active_buffer = None;
        self.editor_input.set_text("");
        self.editor_layout_cache = None;
        self.editor_layout = None;
        self.diagnostics.clear();
        self.symbols.clear();
        self.task_results.clear();
        self.workspace_entries = workspace_snapshot(&self.connection, self.project_id)?
            .entries
            .len();
        self.refresh_workspace_surfaces(true)?;
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
            self.record_backend_error("rejection", error);
            self.pending_approval = Some(call);
            cx.notify();
            return;
        }
        if let Err(error) = self.collect_events() {
            self.record_backend_error("rejection event stream", error);
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
            self.record_backend_error("interrupt", error);
        } else if let Err(error) = self.collect_events() {
            self.record_backend_error("interrupt event stream", error);
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
            self.record_backend_error("pause", error);
        } else if let Err(error) = self.collect_events() {
            self.record_backend_error("pause event stream", error);
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
            self.record_backend_error("resume", error);
        } else if let Err(error) = self.collect_events() {
            self.record_backend_error("resume event stream", error);
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
            self.record_backend_error("retry", error);
        } else if let Err(error) = self.collect_events() {
            self.record_backend_error("retry event stream", error);
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
            TimelineItem::Error { operation, error } => div()
                .px_2()
                .py_1()
                .rounded_sm()
                .bg(rgb(0x4b2020))
                .text_sm()
                .text_color(rgb(0xfca5a5))
                .child(format!(
                    "{} [{}] {}{}",
                    operation,
                    error.code,
                    error.message,
                    if error.retryable { " (retryable)" } else { "" }
                ))
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

    fn render_editor(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
        for (index, buffer) in self.open_buffers.iter().enumerate() {
            let active = self.active_buffer == Some(buffer.id);
            let buffer_id = buffer.id;
            tabs = tabs.child(
                div()
                    .id(("editor-tab", index))
                    .px_2()
                    .py_1()
                    .bg(if active { rgb(0x293244) } else { rgb(0x20242c) })
                    .text_color(if active { rgb(0xf3f4f6) } else { rgb(0x8f98a6) })
                    .cursor_pointer()
                    .child(format!(
                        "{}{}",
                        if buffer.dirty { "● " } else { "" },
                        if buffer.external_change {
                            format!("{} !", buffer.path)
                        } else {
                            buffer.path.clone()
                        }
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.focus_tab(buffer_id, cx);
                    })),
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
        let code = div()
            .flex_1()
            .id("editor-code")
            .overflow_y_scroll()
            .p_3()
            .bg(rgb(0x0f1115))
            .text_color(rgb(0xd1d5db))
            .text_size(px(13.))
            .key_context("CodeEditor")
            .track_focus(&self.editor_focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::save_action))
            .on_action(cx.listener(Self::undo_action))
            .on_action(cx.listener(Self::redo_action))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::editor_mouse_down))
            .on_mouse_move(cx.listener(Self::editor_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::editor_mouse_up))
            .child(TextEditorElement { view: cx.entity() })
            .when(self.active_buffer.is_none(), |element| {
                element.child("Open a file to begin editing")
            })
            .when(
                self.open_buffers
                    .iter()
                    .find(|buffer| Some(buffer.id) == self.active_buffer)
                    .is_some_and(|buffer| buffer.external_change),
                |element| {
                    element.child(
                        div()
                            .mt_2()
                            .p_2()
                            .bg(rgb(0x4b2020))
                            .text_color(rgb(0xfca5a5))
                            .child(
                                div()
                                    .child("External change detected — reload before saving")
                                    .child(
                                        div()
                                            .id("reload-editor")
                                            .mt_1()
                                            .px_2()
                                            .py_1()
                                            .rounded_sm()
                                            .bg(rgb(0x78350f))
                                            .text_color(rgb(0xfef3c7))
                                            .cursor_pointer()
                                            .child("Reload external file")
                                            .on_click(cx.listener(Self::reload_active)),
                                    ),
                            ),
                    )
                },
            );
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

    fn render_backend_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (label, color) = match &self.backend_status {
            BackendStatus::Connected => {
                ("Backend: in-process / connected".to_owned(), rgb(0x9ad7bd))
            }
            BackendStatus::Error(error) => (
                format!(
                    "Backend: error [{}]{}",
                    error.code,
                    if error.retryable { " / retryable" } else { "" }
                ),
                rgb(0xfca5a5),
            ),
        };
        div()
            .flex()
            .items_center()
            .gap_1()
            .text_sm()
            .text_color(color)
            .child(label)
            .child(
                div()
                    .id("reconnect-backend")
                    .px_1()
                    .py_1()
                    .rounded_sm()
                    .bg(rgb(0x293244))
                    .text_color(rgb(0xdbeafe))
                    .cursor_pointer()
                    .child("Reconnect")
                    .on_click(cx.listener(Self::reconnect)),
            )
    }

    fn reconnect(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.backend_status = BackendStatus::Connected;
        let _ = self.refresh_workspace_surfaces(false);
        if let Err(error) = self.collect_events() {
            self.record_backend_error("reconnect event stream", error);
        }
        cx.notify();
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
                    .child(self.render_backend_status(cx))
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
                                            .child(self.render_editor(cx)),
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
                        "{}  |  {}  |  {} events  |  {} buffers{}  |  {}",
                        session_state_name(self.session_state),
                        self.workspace_root.display(),
                        self.timeline.len(),
                        self.open_buffers.len(),
                        if self.open_buffers.iter().any(|buffer| buffer.dirty) {
                            "  |  unsaved"
                        } else {
                            ""
                        },
                        match &self.backend_status {
                            BackendStatus::Connected => "backend connected",
                            BackendStatus::Error(_) => "backend error",
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

fn start_run(
    connection: &InProcessConnection,
    workspace_root: &Path,
    model: &ModelId,
    task: String,
) -> Result<(loom_core::AgentSessionSnapshot, AgentRunSnapshot), LoomError> {
    let session = create_session(connection)?;
    let response = connection.request(RequestEnvelope::new(ClientRequest::StartAgentRun {
        session_id: session.id,
        task,
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

#[derive(Clone, Debug)]
struct UiOptions {
    workspace: Option<PathBuf>,
    task: String,
    demo: bool,
}

impl UiOptions {
    fn parse<I>(args: I) -> Result<Self, LoomError>
    where
        I: IntoIterator<Item = String>,
    {
        let mut workspace = None;
        let mut task = "make a small repository change and validate it".to_owned();
        let mut demo = true;
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
                "--demo" => {
                    workspace = None;
                    demo = true;
                }
                "--help" | "-h" => {
                    return Err(LoomError::invalid_request(
                        "usage: loom-ui [--workspace PATH] [--task DESCRIPTION] [--demo]",
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

    let root = std::env::temp_dir().join("loom-m5-ui-demo");
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
    Ok((root, options.demo))
}

fn reset_demo_workspace(root: &Path) -> Result<(), LoomError> {
    let demo_file = root.join("loom-m5-demo.txt");
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

fn backend_error_timeline_item(operation: &str, error: &LoomError) -> TimelineItem {
    TimelineItem::Error {
        operation: operation.to_owned(),
        error: error.clone(),
    }
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
    let options = match UiOptions::parse(std::env::args()) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("could not parse Loom UI arguments: {error}");
            std::process::exit(1);
        }
    };
    Application::new().run(move |cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("backspace", Backspace, Some("CodeEditor")),
            KeyBinding::new("delete", Delete, Some("CodeEditor")),
            KeyBinding::new("left", Left, Some("CodeEditor")),
            KeyBinding::new("right", Right, Some("CodeEditor")),
            KeyBinding::new("up", Up, Some("CodeEditor")),
            KeyBinding::new("down", Down, Some("CodeEditor")),
            KeyBinding::new("shift-left", SelectLeft, Some("CodeEditor")),
            KeyBinding::new("shift-right", SelectRight, Some("CodeEditor")),
            KeyBinding::new("shift-up", SelectUp, Some("CodeEditor")),
            KeyBinding::new("shift-down", SelectDown, Some("CodeEditor")),
            KeyBinding::new("home", Home, Some("CodeEditor")),
            KeyBinding::new("end", End, Some("CodeEditor")),
            KeyBinding::new("shift-home", SelectHome, Some("CodeEditor")),
            KeyBinding::new("shift-end", SelectEnd, Some("CodeEditor")),
            KeyBinding::new("cmd-a", SelectAll, Some("CodeEditor")),
            KeyBinding::new("ctrl-a", SelectAll, Some("CodeEditor")),
            KeyBinding::new("cmd-v", Paste, Some("CodeEditor")),
            KeyBinding::new("ctrl-v", Paste, Some("CodeEditor")),
            KeyBinding::new("cmd-c", Copy, Some("CodeEditor")),
            KeyBinding::new("ctrl-c", Copy, Some("CodeEditor")),
            KeyBinding::new("cmd-x", Cut, Some("CodeEditor")),
            KeyBinding::new("ctrl-x", Cut, Some("CodeEditor")),
            KeyBinding::new("cmd-s", Save, Some("CodeEditor")),
            KeyBinding::new("ctrl-s", Save, Some("CodeEditor")),
            KeyBinding::new("cmd-z", Undo, Some("CodeEditor")),
            KeyBinding::new("ctrl-z", Undo, Some("CodeEditor")),
            KeyBinding::new("cmd-shift-z", Redo, Some("CodeEditor")),
            KeyBinding::new("ctrl-shift-z", Redo, Some("CodeEditor")),
            KeyBinding::new("ctrl-y", Redo, Some("CodeEditor")),
        ]);
        let view = match LoomView::try_new(&options, cx.focus_handle()) {
            Ok(view) => view,
            Err(error) => {
                eprintln!("could not initialize Loom UI: {error}");
                cx.quit();
                return;
            }
        };
        let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
        let window = match cx.open_window(
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
            Ok(window) => window,
            Err(error) => {
                eprintln!("failed to open Loom window: {error}");
                cx.quit();
                return;
            }
        };
        if let Err(error) = window.update(cx, |view, window, cx| {
            view.editor_focus_handle.focus(window);
            cx.activate(true);
        }) {
            eprintln!("failed to focus Loom editor: {error}");
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
        buffer.move_to(4, true);
        assert_eq!(buffer.selected_range, 2..4);
        buffer.replace_range(buffer.selected_range.clone(), "😀");
        assert_eq!(buffer.text, "ax😀second");
        assert_eq!(buffer.cursor_offset(), 6);
        assert_eq!(buffer.offset_to_utf16(6), 4);
        assert_eq!(buffer.offset_from_utf16(4), 6);
    }

    #[test]
    fn backend_error_projection_keeps_structured_error_fields() {
        let error = LoomError::new(ErrorCode::ExternalChange, "changed on disk", true);
        let item = backend_error_timeline_item("save buffer", &error);
        assert_eq!(
            item,
            TimelineItem::Error {
                operation: "save buffer".to_owned(),
                error,
            }
        );
    }

    #[test]
    fn ui_options_allow_explicit_workspace_and_task() {
        let options = UiOptions::parse([
            "loom-ui".to_owned(),
            "--workspace".to_owned(),
            "/tmp/project".to_owned(),
            "--task".to_owned(),
            "fix the editor".to_owned(),
        ])
        .unwrap();
        assert_eq!(options.workspace, Some(PathBuf::from("/tmp/project")));
        assert_eq!(options.task, "fix the editor");
        assert!(!options.demo);
    }

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
