use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use loom_core::{
    AgentSessionId, BufferId as CoreBufferId, ErrorCode, LoomError, PaneId as CorePaneId, Result,
    RunId, Timestamp,
};
use serde::{Deserialize, Serialize};

use crate::{Workspace, WorkspaceBytes, WorkspaceEntryKind, WorkspaceSnapshot};

pub const DEFAULT_MAX_EDITOR_FILE_BYTES: u64 = 2 * 1024 * 1024;
pub const DEFAULT_SEARCH_RESULT_LIMIT: usize = 512;

pub type BufferId = CoreBufferId;
pub type PaneId = CorePaneId;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BufferEncoding {
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NewlineStyle {
    Lf,
    CrLf,
    Cr,
    Mixed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutosavePolicy {
    Manual,
    OnFocusLost,
    AfterIdle { delay_ms: u64 },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TextRange {
    pub start: usize,
    pub end: usize,
}

impl TextRange {
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    fn validate(self, text: &str) -> Result<()> {
        if self.start > self.end
            || self.end > text.len()
            || !text.is_char_boundary(self.start)
            || !text.is_char_boundary(self.end)
        {
            return Err(LoomError::invalid_request(
                "buffer edit range must be valid UTF-8 boundaries",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BufferEdit {
    pub range: TextRange,
    pub replacement: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BufferSnapshot {
    pub id: BufferId,
    pub path: String,
    pub text: String,
    pub revision: String,
    pub saved_revision: String,
    pub dirty: bool,
    pub external_change: bool,
    pub encoding: BufferEncoding,
    pub newline: NewlineStyle,
    pub size_bytes: u64,
    pub large_file: bool,
    pub last_edit_at: Option<Timestamp>,
    pub agent_markers: Vec<AgentChangeMarker>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentChangeKind {
    Inserted,
    Modified,
    Deleted,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentChangeMarker {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub kind: AgentChangeKind,
    pub run_id: Option<RunId>,
    pub session_id: Option<AgentSessionId>,
    pub revision: Option<String>,
    pub description: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EditorTabSnapshot {
    pub buffer_id: BufferId,
    pub pinned: bool,
    pub preview: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EditorPaneSnapshot {
    pub id: PaneId,
    pub tabs: Vec<EditorTabSnapshot>,
    pub active_tab: Option<BufferId>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EditorLayoutSnapshot {
    pub panes: Vec<EditorPaneSnapshot>,
    pub focused_pane: PaneId,
    pub split_direction: Option<SplitDirection>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FileTreeEntry {
    pub path: String,
    pub kind: FileTreeEntryKind,
    pub depth: u16,
    pub size: u64,
    pub modified_at: Option<Timestamp>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileTreeEntryKind {
    File,
    Directory,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SearchQuery {
    pub query: String,
    pub case_sensitive: bool,
    pub max_results: usize,
}

impl SearchQuery {
    pub fn literal(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            case_sensitive: false,
            max_results: DEFAULT_SEARCH_RESULT_LIMIT,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SearchMatch {
    pub path: String,
    pub line: u32,
    pub column: u32,
    pub end_column: u32,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextFileKind {
    RepositoryInstructions,
    ContextReference,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextFileReference {
    pub path: String,
    pub kind: ContextFileKind,
    pub content: String,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SymbolNavigationHint {
    pub path: String,
    pub line: u32,
    pub name: String,
}

#[derive(Clone, Debug)]
struct EditorBuffer {
    id: BufferId,
    path: String,
    text: String,
    saved_text: String,
    revision: String,
    saved_revision: String,
    external_change: bool,
    encoding: BufferEncoding,
    newline: NewlineStyle,
    size_bytes: u64,
    large_file: bool,
    last_edit_at: Option<Timestamp>,
    undo: Vec<String>,
    redo: Vec<String>,
    agent_markers: Vec<AgentChangeMarker>,
}

impl EditorBuffer {
    fn snapshot(&self) -> BufferSnapshot {
        BufferSnapshot {
            id: self.id,
            path: self.path.clone(),
            text: self.text.clone(),
            revision: self.revision.clone(),
            saved_revision: self.saved_revision.clone(),
            dirty: self.text != self.saved_text,
            external_change: self.external_change,
            encoding: self.encoding,
            newline: self.newline,
            size_bytes: self.size_bytes,
            large_file: self.large_file,
            last_edit_at: self.last_edit_at,
            agent_markers: self.agent_markers.clone(),
        }
    }
}

#[derive(Clone, Debug)]
struct EditorTab {
    buffer_id: BufferId,
    pinned: bool,
    preview: bool,
}

#[derive(Clone, Debug)]
struct EditorPane {
    id: PaneId,
    tabs: Vec<EditorTab>,
    active_tab: Option<BufferId>,
}

#[derive(Debug)]
struct EditorState {
    buffers: BTreeMap<BufferId, EditorBuffer>,
    panes: BTreeMap<PaneId, EditorPane>,
    focused_pane: PaneId,
    split_direction: Option<SplitDirection>,
    autosave: AutosavePolicy,
    max_file_bytes: u64,
}

#[derive(Clone, Debug)]
pub struct EditorWorkspace {
    workspace: Workspace,
    state: Arc<Mutex<EditorState>>,
}

impl EditorWorkspace {
    pub fn new(workspace: Workspace) -> Self {
        let pane = PaneId::new();
        let mut panes = BTreeMap::new();
        panes.insert(
            pane,
            EditorPane {
                id: pane,
                tabs: Vec::new(),
                active_tab: None,
            },
        );
        Self {
            workspace,
            state: Arc::new(Mutex::new(EditorState {
                buffers: BTreeMap::new(),
                panes,
                focused_pane: pane,
                split_direction: None,
                autosave: AutosavePolicy::Manual,
                max_file_bytes: DEFAULT_MAX_EDITOR_FILE_BYTES,
            })),
        }
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    pub fn set_max_file_bytes(&self, max_file_bytes: u64) -> Result<()> {
        if max_file_bytes == 0 {
            return Err(LoomError::invalid_request(
                "editor file limit must be greater than zero",
            ));
        }
        self.lock_state()?.max_file_bytes = max_file_bytes;
        Ok(())
    }

    pub fn max_file_bytes(&self) -> Result<u64> {
        Ok(self.lock_state()?.max_file_bytes)
    }

    pub fn set_autosave_policy(&self, policy: AutosavePolicy) -> Result<()> {
        if matches!(policy, AutosavePolicy::AfterIdle { delay_ms: 0 }) {
            return Err(LoomError::invalid_request(
                "autosave idle delay must be greater than zero",
            ));
        }
        self.lock_state()?.autosave = policy;
        Ok(())
    }

    pub fn autosave_policy(&self) -> Result<AutosavePolicy> {
        Ok(self.lock_state()?.autosave)
    }

    pub fn open_buffer(&self, path: &str) -> Result<BufferSnapshot> {
        let mut state = self.lock_state()?;
        if let Some(id) = state
            .buffers
            .values()
            .find(|buffer| buffer.path == path)
            .map(|buffer| buffer.id)
        {
            let snapshot = state
                .buffers
                .get(&id)
                .expect("buffer found by id")
                .snapshot();
            set_active_tab(&mut state, id);
            return Ok(snapshot);
        }
        let file = self.workspace.read_file_bytes(path)?;
        let max_file_bytes = state.max_file_bytes;
        if file.bytes.len() as u64 > max_file_bytes {
            return Err(LoomError::new(
                ErrorCode::FileTooLarge,
                format!(
                    "file '{path}' is {} bytes, above the editor limit of {max_file_bytes}",
                    file.bytes.len()
                ),
                false,
            ));
        }
        let decoded = decode_document(&file)?;
        let id = BufferId::new();
        let buffer = EditorBuffer {
            id,
            path: path.to_owned(),
            text: decoded.text.clone(),
            saved_text: decoded.text,
            revision: file.revision.clone(),
            saved_revision: file.revision,
            external_change: false,
            encoding: decoded.encoding,
            newline: decoded.newline,
            size_bytes: file.bytes.len() as u64,
            large_file: file.bytes.len() as u64 > max_file_bytes.saturating_mul(3) / 4,
            last_edit_at: None,
            undo: Vec::new(),
            redo: Vec::new(),
            agent_markers: Vec::new(),
        };
        let snapshot = buffer.snapshot();
        state.buffers.insert(id, buffer);
        add_tab(&mut state, id);
        Ok(snapshot)
    }

    pub fn buffer(&self, id: BufferId) -> Result<BufferSnapshot> {
        Ok(self
            .lock_state()?
            .buffers
            .get(&id)
            .ok_or_else(|| LoomError::not_found("editor buffer", id))?
            .snapshot())
    }

    pub fn buffers(&self) -> Result<Vec<BufferSnapshot>> {
        Ok(self
            .lock_state()?
            .buffers
            .values()
            .map(EditorBuffer::snapshot)
            .collect())
    }

    pub fn edit_buffer(&self, id: BufferId, edit: BufferEdit) -> Result<BufferSnapshot> {
        let mut state = self.lock_state()?;
        let buffer = state
            .buffers
            .get_mut(&id)
            .ok_or_else(|| LoomError::not_found("editor buffer", id))?;
        edit.range.validate(&buffer.text)?;
        buffer.undo.push(buffer.text.clone());
        buffer
            .text
            .replace_range(edit.range.start..edit.range.end, &edit.replacement);
        buffer.redo.clear();
        buffer.last_edit_at = Some(Timestamp::now());
        Ok(buffer.snapshot())
    }

    pub fn undo(&self, id: BufferId) -> Result<BufferSnapshot> {
        let mut state = self.lock_state()?;
        let buffer = state
            .buffers
            .get_mut(&id)
            .ok_or_else(|| LoomError::not_found("editor buffer", id))?;
        let previous = buffer.undo.pop().ok_or_else(|| {
            LoomError::new(ErrorCode::InvalidState, "buffer has no edit to undo", false)
        })?;
        buffer.redo.push(buffer.text.clone());
        buffer.text = previous;
        buffer.last_edit_at = Some(Timestamp::now());
        Ok(buffer.snapshot())
    }

    pub fn redo(&self, id: BufferId) -> Result<BufferSnapshot> {
        let mut state = self.lock_state()?;
        let buffer = state
            .buffers
            .get_mut(&id)
            .ok_or_else(|| LoomError::not_found("editor buffer", id))?;
        let next = buffer.redo.pop().ok_or_else(|| {
            LoomError::new(ErrorCode::InvalidState, "buffer has no edit to redo", false)
        })?;
        buffer.undo.push(buffer.text.clone());
        buffer.text = next;
        buffer.last_edit_at = Some(Timestamp::now());
        Ok(buffer.snapshot())
    }

    pub fn save_buffer(&self, id: BufferId) -> Result<BufferSnapshot> {
        let mut state = self.lock_state()?;
        let max_file_bytes = state.max_file_bytes;
        let buffer = state
            .buffers
            .get_mut(&id)
            .ok_or_else(|| LoomError::not_found("editor buffer", id))?;
        let current = match self.workspace.read_file_bytes(&buffer.path) {
            Ok(current) => current,
            Err(error) if error.code == ErrorCode::NotFound => {
                buffer.external_change = true;
                return Err(LoomError::new(
                    ErrorCode::ExternalChange,
                    format!("cannot save '{}': the file was removed", buffer.path),
                    false,
                ));
            }
            Err(error) => return Err(error),
        };
        if current.revision != buffer.saved_revision {
            buffer.external_change = true;
            return Err(LoomError::new(
                ErrorCode::ExternalChange,
                format!(
                    "cannot save '{}': the file changed outside this buffer",
                    buffer.path
                ),
                false,
            ));
        }
        if buffer.text == buffer.saved_text {
            return Ok(buffer.snapshot());
        }
        let bytes = encode_document(&buffer.text, buffer.encoding, buffer.newline)?;
        let written = self.workspace.write_file_bytes(
            &buffer.path,
            bytes.clone(),
            Some(&buffer.saved_revision),
        )?;
        buffer.revision = written.after_revision.clone();
        buffer.saved_revision = written.after_revision;
        buffer.saved_text = buffer.text.clone();
        buffer.size_bytes = bytes.len() as u64;
        buffer.large_file = bytes.len() as u64 > max_file_bytes.saturating_mul(3) / 4;
        buffer.external_change = false;
        buffer.last_edit_at = None;
        Ok(buffer.snapshot())
    }

    pub fn save_all(&self) -> Result<Vec<BufferSnapshot>> {
        let ids = self
            .lock_state()?
            .buffers
            .values()
            .filter(|buffer| buffer.text != buffer.saved_text)
            .map(|buffer| buffer.id)
            .collect::<Vec<_>>();
        ids.into_iter().map(|id| self.save_buffer(id)).collect()
    }

    pub fn autosave_due(&self, now: Timestamp) -> Result<Vec<BufferId>> {
        let policy = self.lock_state()?.autosave;
        let AutosavePolicy::AfterIdle { delay_ms } = policy else {
            return Ok(Vec::new());
        };
        let ids = self
            .lock_state()?
            .buffers
            .values()
            .filter(|buffer| {
                buffer.text != buffer.saved_text
                    && buffer.last_edit_at.is_some_and(|edited| {
                        now.as_unix_millis().saturating_sub(edited.as_unix_millis()) >= delay_ms
                    })
            })
            .map(|buffer| buffer.id)
            .collect::<Vec<_>>();
        for id in &ids {
            self.save_buffer(*id)?;
        }
        Ok(ids)
    }

    pub fn mark_focus_lost(&self) -> Result<Vec<BufferId>> {
        if self.autosave_policy()? != AutosavePolicy::OnFocusLost {
            return Ok(Vec::new());
        }
        let ids = self
            .lock_state()?
            .buffers
            .values()
            .filter(|buffer| buffer.text != buffer.saved_text)
            .map(|buffer| buffer.id)
            .collect::<Vec<_>>();
        for id in &ids {
            self.save_buffer(*id)?;
        }
        Ok(ids)
    }

    pub fn mark_external_changes(&self) -> Result<Vec<BufferId>> {
        let mut state = self.lock_state()?;
        let mut changed = Vec::new();
        for buffer in state.buffers.values_mut() {
            match self.workspace.read_file_bytes(&buffer.path) {
                Ok(current) if current.revision != buffer.saved_revision => {
                    buffer.external_change = true;
                    changed.push(buffer.id);
                }
                Ok(_) => {}
                Err(error) if error.code == ErrorCode::NotFound => {
                    buffer.external_change = true;
                    changed.push(buffer.id);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(changed)
    }

    pub fn reload_buffer(&self, id: BufferId, discard_dirty: bool) -> Result<BufferSnapshot> {
        let mut state = self.lock_state()?;
        let max_file_bytes = state.max_file_bytes;
        let buffer = state
            .buffers
            .get_mut(&id)
            .ok_or_else(|| LoomError::not_found("editor buffer", id))?;
        if buffer.text != buffer.saved_text && !discard_dirty {
            return Err(LoomError::new(
                ErrorCode::Conflict,
                "buffer has unsaved changes; pass discard_dirty to reload",
                false,
            ));
        }
        let file = self.workspace.read_file_bytes(&buffer.path)?;
        if file.bytes.len() as u64 > max_file_bytes {
            return Err(LoomError::new(
                ErrorCode::FileTooLarge,
                format!("file '{}' is above the editor size limit", buffer.path),
                false,
            ));
        }
        let decoded = decode_document(&file)?;
        buffer.text = decoded.text.clone();
        buffer.saved_text = decoded.text;
        buffer.revision = file.revision.clone();
        buffer.saved_revision = file.revision;
        buffer.encoding = decoded.encoding;
        buffer.newline = decoded.newline;
        buffer.size_bytes = file.bytes.len() as u64;
        buffer.external_change = false;
        buffer.undo.clear();
        buffer.redo.clear();
        buffer.last_edit_at = None;
        Ok(buffer.snapshot())
    }

    pub fn close_buffer(&self, id: BufferId, force: bool) -> Result<()> {
        let mut state = self.lock_state()?;
        let buffer = state
            .buffers
            .get(&id)
            .ok_or_else(|| LoomError::not_found("editor buffer", id))?;
        if buffer.text != buffer.saved_text && !force {
            return Err(LoomError::new(
                ErrorCode::Conflict,
                format!("buffer '{}' has unsaved changes", buffer.path),
                false,
            ));
        }
        state.buffers.remove(&id);
        for pane in state.panes.values_mut() {
            pane.tabs.retain(|tab| tab.buffer_id != id);
            if pane.active_tab == Some(id) {
                pane.active_tab = pane.tabs.last().map(|tab| tab.buffer_id);
            }
        }
        Ok(())
    }

    pub fn record_agent_change(
        &self,
        id: BufferId,
        mut marker: AgentChangeMarker,
    ) -> Result<BufferSnapshot> {
        let mut state = self.lock_state()?;
        let buffer = state
            .buffers
            .get_mut(&id)
            .ok_or_else(|| LoomError::not_found("editor buffer", id))?;
        marker.path = buffer.path.clone();
        if marker.revision.is_none() {
            marker.revision = Some(buffer.revision.clone());
        }
        buffer.agent_markers.push(marker);
        Ok(buffer.snapshot())
    }

    pub fn clear_agent_markers(&self, id: BufferId) -> Result<BufferSnapshot> {
        let mut state = self.lock_state()?;
        let buffer = state
            .buffers
            .get_mut(&id)
            .ok_or_else(|| LoomError::not_found("editor buffer", id))?;
        buffer.agent_markers.clear();
        Ok(buffer.snapshot())
    }

    pub fn split(&self, direction: SplitDirection) -> Result<EditorLayoutSnapshot> {
        let mut state = self.lock_state()?;
        let source = state
            .panes
            .get(&state.focused_pane)
            .cloned()
            .ok_or_else(|| LoomError::invalid_state("focused editor pane is missing"))?;
        let pane_id = PaneId::new();
        let active = source.active_tab;
        let tabs = active
            .map(|buffer_id| {
                vec![EditorTab {
                    buffer_id,
                    pinned: false,
                    preview: false,
                }]
            })
            .unwrap_or_default();
        state.panes.insert(
            pane_id,
            EditorPane {
                id: pane_id,
                tabs,
                active_tab: active,
            },
        );
        state.focused_pane = pane_id;
        state.split_direction = Some(direction);
        Ok(layout_snapshot(&state))
    }

    pub fn close_pane(&self, id: PaneId) -> Result<EditorLayoutSnapshot> {
        let mut state = self.lock_state()?;
        if state.panes.len() == 1 {
            return Err(LoomError::invalid_state(
                "the last editor pane cannot be closed",
            ));
        }
        if state.panes.remove(&id).is_none() {
            return Err(LoomError::not_found("editor pane", id));
        }
        if state.focused_pane == id {
            state.focused_pane = *state
                .panes
                .keys()
                .next()
                .ok_or_else(|| LoomError::invalid_state("editor has no remaining panes"))?;
        }
        Ok(layout_snapshot(&state))
    }

    pub fn focus_pane(&self, id: PaneId) -> Result<EditorLayoutSnapshot> {
        let mut state = self.lock_state()?;
        if !state.panes.contains_key(&id) {
            return Err(LoomError::not_found("editor pane", id));
        }
        state.focused_pane = id;
        Ok(layout_snapshot(&state))
    }

    pub fn focus_tab(&self, pane_id: PaneId, buffer_id: BufferId) -> Result<EditorLayoutSnapshot> {
        let mut state = self.lock_state()?;
        let pane = state
            .panes
            .get_mut(&pane_id)
            .ok_or_else(|| LoomError::not_found("editor pane", pane_id))?;
        if !pane.tabs.iter().any(|tab| tab.buffer_id == buffer_id) {
            return Err(LoomError::not_found("editor tab", buffer_id));
        }
        pane.active_tab = Some(buffer_id);
        state.focused_pane = pane_id;
        Ok(layout_snapshot(&state))
    }

    pub fn layout(&self) -> Result<EditorLayoutSnapshot> {
        Ok(layout_snapshot(&*self.lock_state()?))
    }

    pub fn file_tree(&self) -> Result<Vec<FileTreeEntry>> {
        let snapshot = self.workspace.snapshot()?;
        Ok(flatten_tree(&snapshot))
    }

    pub fn fuzzy_find_files(&self, query: &str, limit: usize) -> Result<Vec<String>> {
        let query = query.trim().to_ascii_lowercase();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let mut scored = self
            .workspace
            .snapshot()?
            .entries
            .into_iter()
            .filter(|entry| entry.kind == WorkspaceEntryKind::File)
            .filter_map(|entry| {
                subsequence_score(&entry.path.to_ascii_lowercase(), &query)
                    .map(|score| (score, entry.path))
            })
            .collect::<Vec<_>>();
        scored.sort();
        Ok(scored
            .into_iter()
            .take(limit.max(1))
            .map(|(_, path)| path)
            .collect())
    }

    pub fn search(&self, query: SearchQuery) -> Result<Vec<SearchMatch>> {
        if query.query.is_empty() {
            return Err(LoomError::invalid_request(
                "workspace search query is empty",
            ));
        }
        let limit = query.max_results.max(1);
        let needle = if query.case_sensitive {
            query.query.clone()
        } else {
            query.query.to_ascii_lowercase()
        };
        let mut matches = Vec::new();
        for entry in self.workspace.snapshot()?.entries {
            if matches.len() >= limit || entry.kind != WorkspaceEntryKind::File {
                continue;
            }
            if entry.size > self.lock_state()?.max_file_bytes {
                continue;
            }
            let file = match self.workspace.read_file_bytes(&entry.path) {
                Ok(file) => file,
                Err(_) => continue,
            };
            let decoded = match decode_document(&file) {
                Ok(decoded) => decoded,
                Err(_) => continue,
            };
            for (line_index, line) in decoded.text.lines().enumerate() {
                let haystack = if query.case_sensitive {
                    line.to_owned()
                } else {
                    line.to_ascii_lowercase()
                };
                let mut offset = 0;
                while let Some(found) = haystack[offset..].find(&needle) {
                    let column = offset + found;
                    matches.push(SearchMatch {
                        path: entry.path.clone(),
                        line: line_index as u32 + 1,
                        column: column as u32 + 1,
                        end_column: (column + needle.len()) as u32 + 1,
                        text: line.to_owned(),
                    });
                    if matches.len() >= limit {
                        break;
                    }
                    offset = column.saturating_add(needle.len().max(1));
                    if offset >= haystack.len() {
                        break;
                    }
                }
                if matches.len() >= limit {
                    break;
                }
            }
        }
        Ok(matches)
    }

    pub fn context_files(&self) -> Result<Vec<ContextFileReference>> {
        let snapshot = self.workspace.snapshot()?;
        let instruction_names = [
            "AGENTS.md",
            "CLAUDE.md",
            "LOOM.md",
            "CONTRIBUTING.md",
            ".github/copilot-instructions.md",
        ];
        let mut files = Vec::new();
        for entry in snapshot
            .entries
            .iter()
            .filter(|entry| entry.kind == WorkspaceEntryKind::File)
        {
            let normalized = entry.path.trim_start_matches("./");
            let name = normalized.rsplit('/').next().unwrap_or(normalized);
            let kind = if instruction_names
                .iter()
                .any(|candidate| normalized == *candidate || name == *candidate)
            {
                Some((
                    ContextFileKind::RepositoryInstructions,
                    "repository instructions",
                ))
            } else if normalized == "README.md"
                || normalized.starts_with("docs/")
                || normalized.starts_with(".github/")
            {
                Some((ContextFileKind::ContextReference, "repository context"))
            } else {
                None
            };
            let Some((kind, reason)) = kind else {
                continue;
            };
            let file = self.workspace.read_file_bytes(&entry.path)?;
            if file.bytes.len() as u64 > self.lock_state()?.max_file_bytes {
                continue;
            }
            let decoded = decode_document(&file)?;
            files.push(ContextFileReference {
                path: entry.path.clone(),
                kind,
                content: decoded.text,
                reason: reason.to_owned(),
            });
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(files)
    }

    pub fn instruction_text(&self) -> Result<String> {
        Ok(self
            .context_files()?
            .into_iter()
            .filter(|file| file.kind == ContextFileKind::RepositoryInstructions)
            .map(|file| format!("## {}\n{}", file.path, file.content))
            .collect::<Vec<_>>()
            .join("\n\n"))
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, EditorState>> {
        self.state.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "editor workspace state lock was poisoned",
                true,
            )
        })
    }
}

#[derive(Debug)]
struct DecodedDocument {
    text: String,
    encoding: BufferEncoding,
    newline: NewlineStyle,
}

fn decode_document(file: &WorkspaceBytes) -> Result<DecodedDocument> {
    let (encoding, payload) = if file.bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        (BufferEncoding::Utf8Bom, &file.bytes[3..])
    } else if file.bytes.starts_with(&[0xff, 0xfe]) {
        (BufferEncoding::Utf16Le, &file.bytes[2..])
    } else if file.bytes.starts_with(&[0xfe, 0xff]) {
        (BufferEncoding::Utf16Be, &file.bytes[2..])
    } else {
        (BufferEncoding::Utf8, file.bytes.as_slice())
    };
    let raw = match encoding {
        BufferEncoding::Utf8 | BufferEncoding::Utf8Bom => String::from_utf8(payload.to_vec())
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::InvalidEncoding,
                    format!("workspace file '{}' is not valid UTF-8: {error}", file.path),
                    false,
                )
            })?,
        BufferEncoding::Utf16Le | BufferEncoding::Utf16Be => {
            if payload.len() % 2 != 0 {
                return Err(LoomError::new(
                    ErrorCode::InvalidEncoding,
                    format!(
                        "workspace file '{}' has an incomplete UTF-16 code unit",
                        file.path
                    ),
                    false,
                ));
            }
            let mut units = Vec::with_capacity(payload.len() / 2);
            for pair in payload.chunks_exact(2) {
                let unit = if encoding == BufferEncoding::Utf16Le {
                    u16::from_le_bytes([pair[0], pair[1]])
                } else {
                    u16::from_be_bytes([pair[0], pair[1]])
                };
                units.push(unit);
            }
            String::from_utf16(&units).map_err(|error| {
                LoomError::new(
                    ErrorCode::InvalidEncoding,
                    format!(
                        "workspace file '{}' is not valid UTF-16: {error}",
                        file.path
                    ),
                    false,
                )
            })?
        }
    };
    if raw.contains('\0') {
        return Err(LoomError::new(
            ErrorCode::InvalidEncoding,
            format!("workspace file '{}' appears to be binary", file.path),
            false,
        ));
    }
    let newline = newline_style(&raw);
    let text = raw.replace("\r\n", "\n").replace('\r', "\n");
    Ok(DecodedDocument {
        text,
        encoding,
        newline,
    })
}

fn encode_document(text: &str, encoding: BufferEncoding, newline: NewlineStyle) -> Result<Vec<u8>> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let line_ending = match newline {
        NewlineStyle::Lf | NewlineStyle::Mixed => "\n",
        NewlineStyle::CrLf => "\r\n",
        NewlineStyle::Cr => "\r",
    };
    let mut value = String::with_capacity(normalized.len());
    for (index, line) in normalized.split('\n').enumerate() {
        if index > 0 {
            value.push_str(line_ending);
        }
        value.push_str(line);
    }
    Ok(match encoding {
        BufferEncoding::Utf8 => value.into_bytes(),
        BufferEncoding::Utf8Bom => {
            let mut bytes = vec![0xef, 0xbb, 0xbf];
            bytes.extend_from_slice(value.as_bytes());
            bytes
        }
        BufferEncoding::Utf16Le => encode_utf16(&value, true),
        BufferEncoding::Utf16Be => encode_utf16(&value, false),
    })
}

fn encode_utf16(value: &str, little_endian: bool) -> Vec<u8> {
    let mut bytes = vec![
        if little_endian { 0xff } else { 0xfe },
        if little_endian { 0xfe } else { 0xff },
    ];
    for unit in value.encode_utf16() {
        let encoded = if little_endian {
            unit.to_le_bytes()
        } else {
            unit.to_be_bytes()
        };
        bytes.extend_from_slice(&encoded);
    }
    bytes
}

fn newline_style(text: &str) -> NewlineStyle {
    let crlf = text.match_indices("\r\n").count();
    let cr = text
        .chars()
        .filter(|character| *character == '\r')
        .count()
        .saturating_sub(crlf);
    let lf = text
        .chars()
        .filter(|character| *character == '\n')
        .count()
        .saturating_sub(crlf);
    match (lf > 0, crlf > 0, cr > 0) {
        (true, false, false) => NewlineStyle::Lf,
        (false, true, false) => NewlineStyle::CrLf,
        (false, false, true) => NewlineStyle::Cr,
        _ => NewlineStyle::Mixed,
    }
}

fn flatten_tree(snapshot: &WorkspaceSnapshot) -> Vec<FileTreeEntry> {
    let mut entries = snapshot
        .entries
        .iter()
        .map(|entry| FileTreeEntry {
            path: entry.path.clone(),
            kind: match entry.kind {
                WorkspaceEntryKind::File => FileTreeEntryKind::File,
                WorkspaceEntryKind::Directory => FileTreeEntryKind::Directory,
            },
            depth: entry.path.matches('/').count() as u16,
            size: entry.size,
            modified_at: entry.modified_at,
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    entries
}

fn subsequence_score(candidate: &str, query: &str) -> Option<(u32, u32, String)> {
    let mut query_chars = query.chars();
    let mut current = query_chars.next()?;
    let mut score = 0_u32;
    let mut last = None;
    let mut first = None;
    for (index, character) in candidate.chars().enumerate() {
        if character == current {
            first.get_or_insert(index);
            if let Some(previous) = last {
                score = score.saturating_add((index.saturating_sub(previous + 1)) as u32);
            }
            last = Some(index);
            if let Some(next) = query_chars.next() {
                current = next;
            } else {
                return Some((score, first.unwrap_or(0) as u32, candidate.to_owned()));
            }
        }
    }
    None
}

fn add_tab(state: &mut EditorState, buffer_id: BufferId) {
    let pane = state
        .panes
        .get_mut(&state.focused_pane)
        .expect("focused pane exists");
    if !pane.tabs.iter().any(|tab| tab.buffer_id == buffer_id) {
        pane.tabs.push(EditorTab {
            buffer_id,
            pinned: false,
            preview: false,
        });
    }
    pane.active_tab = Some(buffer_id);
}

fn set_active_tab(state: &mut EditorState, buffer_id: BufferId) {
    for pane in state.panes.values_mut() {
        if pane.tabs.iter().any(|tab| tab.buffer_id == buffer_id) {
            pane.active_tab = Some(buffer_id);
            state.focused_pane = pane.id;
            return;
        }
    }
    add_tab(state, buffer_id);
}

fn layout_snapshot(state: &EditorState) -> EditorLayoutSnapshot {
    EditorLayoutSnapshot {
        panes: state
            .panes
            .values()
            .map(|pane| EditorPaneSnapshot {
                id: pane.id,
                tabs: pane
                    .tabs
                    .iter()
                    .map(|tab| EditorTabSnapshot {
                        buffer_id: tab.buffer_id,
                        pinned: tab.pinned,
                        preview: tab.preview,
                    })
                    .collect(),
                active_tab: pane.active_tab,
            })
            .collect(),
        focused_pane: state.focused_pane,
        split_direction: state.split_direction,
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use loom_core::ProjectId;

    use super::*;

    fn editor() -> (EditorWorkspace, PathBuf) {
        let root = std::env::temp_dir().join(format!("loom-editor-{}", ProjectId::new()));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), b"fn answer() {\r\n    41\r\n}\r\n").unwrap();
        fs::write(root.join("README.md"), b"answer workspace\n").unwrap();
        let workspace = Workspace::open(ProjectId::new(), &root).unwrap();
        (EditorWorkspace::new(workspace), root)
    }

    #[test]
    fn buffers_preserve_newlines_and_support_undo_redo() {
        let (editor, root) = editor();
        let buffer = editor.open_buffer("src/lib.rs").unwrap();
        assert_eq!(buffer.newline, NewlineStyle::CrLf);
        editor
            .edit_buffer(
                buffer.id,
                BufferEdit {
                    range: TextRange::new(18, 20),
                    replacement: "42".to_owned(),
                },
            )
            .unwrap();
        assert!(editor.buffer(buffer.id).unwrap().dirty);
        editor.undo(buffer.id).unwrap();
        editor.redo(buffer.id).unwrap();
        editor.save_buffer(buffer.id).unwrap();
        assert_eq!(
            fs::read(root.join("src/lib.rs")).unwrap(),
            b"fn answer() {\r\n    42\r\n}\r\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn external_changes_never_get_overwritten() {
        let (editor, root) = editor();
        let buffer = editor.open_buffer("README.md").unwrap();
        editor
            .edit_buffer(
                buffer.id,
                BufferEdit {
                    range: TextRange::new(0, 6),
                    replacement: "changed".to_owned(),
                },
            )
            .unwrap();
        fs::write(root.join("README.md"), b"external\n").unwrap();
        let error = editor.save_buffer(buffer.id).unwrap_err();
        assert_eq!(error.code, ErrorCode::ExternalChange);
        assert!(editor.buffer(buffer.id).unwrap().external_change);
        assert_eq!(
            fs::read_to_string(root.join("README.md")).unwrap(),
            "external\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn navigation_fuzzy_search_and_context_are_workspace_scoped() {
        let (editor, root) = editor();
        fs::write(root.join("AGENTS.md"), b"Use focused changes.\n").unwrap();
        assert_eq!(
            editor.fuzzy_find_files("sl", 1).unwrap(),
            vec!["src/lib.rs".to_owned()]
        );
        let matches = editor.search(SearchQuery::literal("answer")).unwrap();
        assert!(matches.iter().any(|item| item.path == "src/lib.rs"));
        let context = editor.context_files().unwrap();
        assert!(context.iter().any(|file| file.path == "AGENTS.md"));
        let layout = editor.split(SplitDirection::Vertical).unwrap();
        assert_eq!(layout.panes.len(), 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn large_files_are_rejected_before_loading() {
        let (editor, root) = editor();
        fs::write(root.join("large.txt"), vec![b'x'; 32]).unwrap();
        editor.set_max_file_bytes(8).unwrap();
        assert_eq!(
            editor.open_buffer("large.txt").unwrap_err().code,
            ErrorCode::FileTooLarge
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn utf16_files_keep_encoding_and_newline_style_on_save() {
        let (editor, root) = editor();
        fs::write(
            root.join("utf16.txt"),
            encode_document("first\nsecond", BufferEncoding::Utf16Le, NewlineStyle::CrLf).unwrap(),
        )
        .unwrap();
        let buffer = editor.open_buffer("utf16.txt").unwrap();
        assert_eq!(buffer.encoding, BufferEncoding::Utf16Le);
        assert_eq!(buffer.newline, NewlineStyle::CrLf);
        editor
            .edit_buffer(
                buffer.id,
                BufferEdit {
                    range: TextRange::new(0, 5),
                    replacement: "updated".to_owned(),
                },
            )
            .unwrap();
        editor.save_buffer(buffer.id).unwrap();
        let bytes = fs::read(root.join("utf16.txt")).unwrap();
        assert_eq!(&bytes[..2], &[0xff, 0xfe]);
        assert!(
            bytes
                .windows(4)
                .any(|window| window == [b'\r', 0, b'\n', 0])
        );
        fs::remove_dir_all(root).unwrap();
    }
}
