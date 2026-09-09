use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::ErrorKind,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::UNIX_EPOCH,
};

use loom_core::{
    AgentSessionId, CheckpointId, ErrorCode, EventSequence, LoomError, ProjectId, Result, Timestamp,
};
use serde::{Deserialize, Serialize};

const MAX_SNAPSHOT_ENTRIES: usize = 100_000;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceEntryKind {
    File,
    Directory,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceEntry {
    pub path: String,
    pub kind: WorkspaceEntryKind,
    pub size: u64,
    pub modified_at: Option<Timestamp>,
    pub revision: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceSnapshot {
    pub project_id: ProjectId,
    pub root: String,
    pub captured_at: Timestamp,
    pub entries: Vec<WorkspaceEntry>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceChangeKind {
    Created,
    Modified,
    Deleted,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceChange {
    pub sequence: EventSequence,
    pub project_id: ProjectId,
    pub path: String,
    pub kind: WorkspaceChangeKind,
    pub revision: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceFile {
    pub path: String,
    pub content: String,
    pub revision: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceEdit {
    pub path: String,
    pub old_text: String,
    pub new_text: String,
    pub expected_revision: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceEditResult {
    pub path: String,
    pub before_revision: String,
    pub after_revision: String,
    pub diff: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceControl {
    Agent,
    User,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Checkpoint {
    pub id: CheckpointId,
    pub project_id: ProjectId,
    pub session_id: Option<AgentSessionId>,
    pub label: String,
    pub created_at: Timestamp,
    pub files: BTreeMap<String, CheckpointFile>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckpointFile {
    pub existed: bool,
    pub content: String,
    pub revision: String,
    pub expected_revision: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RevertResult {
    pub checkpoint_id: CheckpointId,
    pub reverted_paths: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UndoResult {
    pub path: String,
    pub revision: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceEditHistory {
    pub path: String,
    pub before: Option<String>,
    pub after_revision: String,
    pub source: WorkspaceControl,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceStateSnapshot {
    pub project_id: ProjectId,
    pub root: String,
    pub control: WorkspaceControl,
    pub checkpoints: Vec<Checkpoint>,
    pub edits: Vec<WorkspaceEditHistory>,
    pub next_sequence: EventSequence,
    pub changes: Vec<WorkspaceChange>,
}

#[derive(Debug)]
struct WorkspaceState {
    control: WorkspaceControl,
    checkpoints: BTreeMap<CheckpointId, Checkpoint>,
    edits: Vec<EditRecord>,
    watcher_snapshot: Option<WorkspaceSnapshot>,
    next_sequence: EventSequence,
    changes: Vec<WorkspaceChange>,
}

#[derive(Clone, Debug)]
struct EditRecord {
    path: String,
    before: Option<String>,
    after_revision: String,
    source: WorkspaceControl,
}

#[derive(Debug)]
struct WorkspaceInner {
    project_id: ProjectId,
    root: PathBuf,
    state: Mutex<WorkspaceState>,
}

#[derive(Clone, Debug)]
pub struct Workspace {
    inner: Arc<WorkspaceInner>,
}

#[derive(Clone, Debug)]
pub struct WorkspaceWatcher {
    workspace: Workspace,
}

impl Workspace {
    pub fn open(project_id: ProjectId, root: impl Into<PathBuf>) -> Result<Self> {
        let requested = root.into();
        if !requested.is_dir() {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!(
                    "workspace root '{}' is not a directory",
                    requested.display()
                ),
                false,
            ));
        }
        let root = fs::canonicalize(&requested).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!(
                    "could not resolve workspace root '{}': {error}",
                    requested.display()
                ),
                false,
            )
        })?;
        let workspace = Self {
            inner: Arc::new(WorkspaceInner {
                project_id,
                root,
                state: Mutex::new(WorkspaceState {
                    control: WorkspaceControl::Agent,
                    checkpoints: BTreeMap::new(),
                    edits: Vec::new(),
                    watcher_snapshot: None,
                    next_sequence: EventSequence::default(),
                    changes: Vec::new(),
                }),
            }),
        };
        let snapshot = workspace.snapshot()?;
        workspace.lock_state()?.watcher_snapshot = Some(snapshot);
        Ok(workspace)
    }

    pub fn project_id(&self) -> ProjectId {
        self.inner.project_id
    }

    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    pub fn watch(&self) -> WorkspaceWatcher {
        WorkspaceWatcher {
            workspace: self.clone(),
        }
    }

    pub fn directory_path(&self, relative: &str) -> Result<PathBuf> {
        let path = self.resolve_relative(relative, false)?;
        if !path.is_dir() {
            return Err(LoomError::invalid_request(format!(
                "workspace path '{relative}' is not a directory"
            )));
        }
        Ok(path)
    }

    pub fn control(&self) -> Result<WorkspaceControl> {
        Ok(self.lock_state()?.control)
    }

    pub fn take_control(&self, control: WorkspaceControl) -> Result<WorkspaceControl> {
        let mut state = self.lock_state()?;
        let previous = state.control;
        state.control = control;
        Ok(previous)
    }

    pub fn export_state(&self) -> Result<WorkspaceStateSnapshot> {
        let state = self.lock_state()?;
        Ok(WorkspaceStateSnapshot {
            project_id: self.inner.project_id,
            root: self.inner.root.display().to_string(),
            control: state.control,
            checkpoints: state.checkpoints.values().cloned().collect(),
            edits: state
                .edits
                .iter()
                .map(|edit| WorkspaceEditHistory {
                    path: edit.path.clone(),
                    before: edit.before.clone(),
                    after_revision: edit.after_revision.clone(),
                    source: edit.source,
                })
                .collect(),
            next_sequence: state.next_sequence,
            changes: state.changes.clone(),
        })
    }

    pub fn state(&self) -> Result<WorkspaceStateSnapshot> {
        self.export_state()
    }

    pub fn restore_state(&self, persisted: WorkspaceStateSnapshot) -> Result<()> {
        if persisted.project_id != self.inner.project_id
            || Path::new(&persisted.root) != self.inner.root
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted workspace identity does not match the opened workspace",
                false,
            ));
        }

        if persisted
            .changes
            .iter()
            .any(|change| change.project_id != self.inner.project_id)
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted workspace change belongs to another project",
                false,
            ));
        }
        if persisted
            .checkpoints
            .iter()
            .any(|checkpoint| checkpoint.project_id != self.inner.project_id)
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted checkpoint belongs to another project",
                false,
            ));
        }
        for checkpoint in &persisted.checkpoints {
            for (path, file) in &checkpoint.files {
                self.resolve_relative(path, true)?;
                if file.revision != revision(&file.content) {
                    return Err(LoomError::new(
                        ErrorCode::MalformedPayload,
                        format!("persisted checkpoint revision for '{path}' is invalid"),
                        false,
                    ));
                }
            }
        }
        for edit in &persisted.edits {
            self.resolve_relative(&edit.path, true)?;
        }
        if persisted
            .changes
            .windows(2)
            .any(|changes| changes[0].sequence >= changes[1].sequence)
            || persisted
                .changes
                .last()
                .is_some_and(|change| change.sequence != persisted.next_sequence)
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted workspace change sequences are invalid",
                false,
            ));
        }
        let mut state = self.lock_state()?;
        state.control = persisted.control;
        state.checkpoints = persisted
            .checkpoints
            .into_iter()
            .map(|checkpoint| (checkpoint.id, checkpoint))
            .collect();
        state.edits = persisted
            .edits
            .into_iter()
            .map(|edit| EditRecord {
                path: edit.path,
                before: edit.before,
                after_revision: edit.after_revision,
                source: edit.source,
            })
            .collect();
        state.next_sequence = persisted.next_sequence;
        state.changes = persisted.changes;
        state.watcher_snapshot = Some(self.snapshot()?);
        Ok(())
    }

    pub fn restore(&self, persisted: WorkspaceStateSnapshot) -> Result<()> {
        self.restore_state(persisted)
    }

    pub fn checkpoints(&self) -> Result<Vec<Checkpoint>> {
        Ok(self.lock_state()?.checkpoints.values().cloned().collect())
    }

    pub fn snapshot(&self) -> Result<WorkspaceSnapshot> {
        let mut entries = Vec::new();
        self.collect_entries(&self.inner.root, Path::new("."), &mut entries)?;
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(WorkspaceSnapshot {
            project_id: self.inner.project_id,
            root: self.inner.root.display().to_string(),
            captured_at: Timestamp::now(),
            entries,
        })
    }

    pub fn read_file(&self, relative: &str) -> Result<WorkspaceFile> {
        let path = self.resolve_relative(relative, false)?;
        let content = fs::read_to_string(&path).map_err(|error| {
            LoomError::new(
                ErrorCode::ToolExecution,
                format!("could not read '{relative}': {error}"),
                false,
            )
        })?;
        Ok(WorkspaceFile {
            path: relative.to_owned(),
            revision: revision(&content),
            content,
        })
    }

    pub fn apply_edit(&self, edit: WorkspaceEdit) -> Result<WorkspaceEditResult> {
        self.apply_edit_from(edit, WorkspaceControl::Agent)
    }

    pub fn apply_user_edit(&self, edit: WorkspaceEdit) -> Result<WorkspaceEditResult> {
        self.apply_edit_from(edit, WorkspaceControl::User)
    }

    fn apply_edit_from(
        &self,
        edit: WorkspaceEdit,
        source: WorkspaceControl,
    ) -> Result<WorkspaceEditResult> {
        let mut state = self.lock_state()?;
        if source == WorkspaceControl::Agent && state.control == WorkspaceControl::User {
            return Err(LoomError::new(
                ErrorCode::Conflict,
                "workspace is under user control; agent edits are paused",
                false,
            ));
        }
        let path = self.resolve_relative(&edit.path, true)?;
        let before = match fs::read_to_string(&path) {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => {
                return Err(LoomError::new(
                    ErrorCode::ToolExecution,
                    format!("could not read '{}' before editing: {error}", edit.path),
                    false,
                ));
            }
        };
        let before_content = before.clone().unwrap_or_default();
        let before_revision = revision(&before_content);
        if let Some(expected) = edit.expected_revision.as_deref() {
            if expected != before_revision {
                return Err(LoomError::conflict(format!(
                    "workspace file '{}' changed before the edit (expected {expected}, found {before_revision})",
                    edit.path
                )));
            }
        }
        let next = if edit.old_text.is_empty() {
            if before.is_some() && !before_content.is_empty() {
                return Err(LoomError::conflict(format!(
                    "old_text is required when replacing existing file '{}'",
                    edit.path
                )));
            }
            edit.new_text
        } else {
            let occurrences = before_content.match_indices(&edit.old_text).count();
            if occurrences != 1 {
                return Err(LoomError::conflict(format!(
                    "expected old_text exactly once in '{}', found {occurrences} matches",
                    edit.path
                )));
            }
            before_content.replacen(&edit.old_text, &edit.new_text, 1)
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                LoomError::new(
                    ErrorCode::ToolExecution,
                    format!("could not create edit destination: {error}"),
                    false,
                )
            })?;
        }
        fs::write(&path, &next).map_err(|error| {
            LoomError::new(
                ErrorCode::ToolExecution,
                format!("could not write '{}': {error}", edit.path),
                false,
            )
        })?;
        let after_revision = revision(&next);
        state.edits.push(EditRecord {
            path: edit.path.clone(),
            before,
            after_revision: after_revision.clone(),
            source,
        });
        if source == WorkspaceControl::Agent {
            for checkpoint in state.checkpoints.values_mut() {
                if let Some(file) = checkpoint.files.get_mut(&edit.path) {
                    file.expected_revision = after_revision.clone();
                } else {
                    checkpoint.files.insert(
                        edit.path.clone(),
                        CheckpointFile {
                            existed: false,
                            content: String::new(),
                            revision: revision(""),
                            expected_revision: after_revision.clone(),
                        },
                    );
                }
            }
        }
        let diff = unified_diff(&edit.path, &before_content, &next);
        Ok(WorkspaceEditResult {
            path: edit.path,
            before_revision,
            after_revision,
            diff,
        })
    }

    pub fn create_checkpoint(
        &self,
        session_id: Option<AgentSessionId>,
        label: impl Into<String>,
    ) -> Result<Checkpoint> {
        let label = label.into();
        if label.trim().is_empty() {
            return Err(LoomError::invalid_request(
                "checkpoint label must not be empty",
            ));
        }
        let snapshot = self.snapshot()?;
        let mut files = BTreeMap::new();
        for entry in snapshot
            .entries
            .iter()
            .filter(|entry| entry.kind == WorkspaceEntryKind::File)
        {
            let file = self.read_file(&entry.path)?;
            files.insert(
                entry.path.clone(),
                CheckpointFile {
                    existed: true,
                    content: file.content,
                    revision: file.revision.clone(),
                    expected_revision: file.revision,
                },
            );
        }
        let checkpoint = Checkpoint {
            id: CheckpointId::new(),
            project_id: self.inner.project_id,
            session_id,
            label,
            created_at: Timestamp::now(),
            files,
        };
        self.lock_state()?
            .checkpoints
            .insert(checkpoint.id, checkpoint.clone());
        Ok(checkpoint)
    }

    pub fn checkpoint(&self, checkpoint_id: CheckpointId) -> Result<Checkpoint> {
        self.lock_state()?
            .checkpoints
            .get(&checkpoint_id)
            .cloned()
            .ok_or_else(|| LoomError::not_found("checkpoint", checkpoint_id))
    }

    pub fn revert_checkpoint(&self, checkpoint_id: CheckpointId) -> Result<RevertResult> {
        let mut state = self.lock_state()?;
        let checkpoint = state
            .checkpoints
            .get(&checkpoint_id)
            .cloned()
            .ok_or_else(|| LoomError::not_found("checkpoint", checkpoint_id))?;
        let mut paths = Vec::new();
        let mut planned = Vec::new();
        for (relative, file) in &checkpoint.files {
            let path = self.resolve_relative(relative, true)?;
            let current = match fs::read_to_string(&path) {
                Ok(contents) => contents,
                Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
                Err(error) => {
                    return Err(LoomError::new(
                        ErrorCode::ToolExecution,
                        format!("could not read '{relative}' before reverting: {error}"),
                        false,
                    ));
                }
            };
            let current_revision = revision(&current);
            if current_revision != file.expected_revision {
                return Err(LoomError::conflict(format!(
                    "cannot revert '{relative}': it changed after checkpoint {}",
                    checkpoint_id
                )));
            }
            planned.push((
                path,
                relative.clone(),
                current,
                file.content.clone(),
                file.existed,
            ));
            paths.push(relative.clone());
        }
        for (path, relative, current, content, existed) in planned {
            let write_result = if existed {
                fs::write(&path, &content)
            } else {
                fs::remove_file(&path).or_else(|error| {
                    if error.kind() == ErrorKind::NotFound {
                        Ok(())
                    } else {
                        Err(error)
                    }
                })
            };
            write_result.map_err(|error| {
                LoomError::new(
                    ErrorCode::ToolExecution,
                    format!("could not revert '{relative}': {error}"),
                    false,
                )
            })?;
            let next_revision = revision(&content);
            state.edits.push(EditRecord {
                path: relative,
                before: Some(current),
                after_revision: next_revision,
                source: WorkspaceControl::User,
            });
        }
        Ok(RevertResult {
            checkpoint_id,
            reverted_paths: paths,
        })
    }

    pub fn undo_last_agent_edit(&self) -> Result<UndoResult> {
        let mut state = self.lock_state()?;
        let index = state
            .edits
            .iter()
            .rposition(|edit| edit.source == WorkspaceControl::Agent)
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::InvalidState,
                    "there is no agent edit to undo",
                    false,
                )
            })?;
        let edit = state.edits[index].clone();
        let path = self.resolve_relative(&edit.path, true)?;
        let current = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
            Err(error) => {
                return Err(LoomError::new(
                    ErrorCode::ToolExecution,
                    format!("could not read '{}' before undoing: {error}", edit.path),
                    false,
                ));
            }
        };
        if revision(&current) != edit.after_revision {
            return Err(LoomError::conflict(format!(
                "cannot undo '{}': the file changed after the agent edit",
                edit.path
            )));
        }
        match edit.before {
            Some(before) => fs::write(&path, &before),
            None => fs::remove_file(&path).or_else(|error| {
                if error.kind() == ErrorKind::NotFound {
                    Ok(())
                } else {
                    Err(error)
                }
            }),
        }
        .map_err(|error| {
            LoomError::new(
                ErrorCode::ToolExecution,
                format!("could not undo '{}': {error}", edit.path),
                false,
            )
        })?;
        state.edits.remove(index);
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
            Err(error) => {
                return Err(LoomError::new(
                    ErrorCode::ToolExecution,
                    format!("could not verify undo '{}': {error}", edit.path),
                    false,
                ));
            }
        };
        Ok(UndoResult {
            path: edit.path,
            revision: revision(&content),
        })
    }

    pub fn poll_changes(&self) -> Result<Vec<WorkspaceChange>> {
        let current = self.snapshot()?;
        let mut state = self.lock_state()?;
        let previous = state
            .watcher_snapshot
            .replace(current.clone())
            .unwrap_or_else(|| current.clone());
        let before = previous
            .entries
            .into_iter()
            .map(|entry| (entry.path.clone(), entry))
            .collect::<BTreeMap<_, _>>();
        let after = current
            .entries
            .into_iter()
            .map(|entry| (entry.path.clone(), entry))
            .collect::<BTreeMap<_, _>>();
        let paths = before
            .keys()
            .chain(after.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut changes = Vec::new();
        for path in paths {
            let kind = match (before.get(&path), after.get(&path)) {
                (None, Some(_)) => Some(WorkspaceChangeKind::Created),
                (Some(_), None) => Some(WorkspaceChangeKind::Deleted),
                (Some(previous), Some(current)) if previous.revision != current.revision => {
                    Some(WorkspaceChangeKind::Modified)
                }
                _ => None,
            };
            if let Some(kind) = kind {
                state.next_sequence = state.next_sequence.next();
                let change = WorkspaceChange {
                    sequence: state.next_sequence,
                    project_id: self.inner.project_id,
                    path: path.clone(),
                    kind,
                    revision: after.get(&path).map(|entry| entry.revision.clone()),
                };
                state.changes.push(change.clone());
                changes.push(change);
            }
        }
        Ok(changes)
    }

    pub fn changes_since(&self, after: Option<EventSequence>) -> Result<Vec<WorkspaceChange>> {
        let _ = self.poll_changes()?;
        Ok(self
            .lock_state()?
            .changes
            .iter()
            .filter(|change| after.is_none_or(|sequence| change.sequence > sequence))
            .cloned()
            .collect())
    }

    fn collect_entries(
        &self,
        path: &Path,
        relative: &Path,
        entries: &mut Vec<WorkspaceEntry>,
    ) -> Result<()> {
        if entries.len() >= MAX_SNAPSHOT_ENTRIES {
            return Err(LoomError::new(
                ErrorCode::ToolExecution,
                format!("workspace snapshot exceeds {MAX_SNAPSHOT_ENTRIES} entries"),
                false,
            ));
        }
        let directory = fs::read_dir(path).map_err(|error| {
            LoomError::new(
                ErrorCode::ToolExecution,
                format!("could not list '{}': {error}", relative.display()),
                false,
            )
        })?;
        for entry in directory {
            let entry = entry.map_err(|error| {
                LoomError::new(
                    ErrorCode::ToolExecution,
                    format!("could not read workspace entry: {error}"),
                    false,
                )
            })?;
            let name = entry.file_name();
            if name == ".git" {
                continue;
            }
            let child_relative = relative.join(&name);
            let child_path = entry.path();
            let file_type = entry.file_type().map_err(|error| {
                LoomError::new(
                    ErrorCode::ToolExecution,
                    format!("could not inspect '{}': {error}", child_relative.display()),
                    false,
                )
            })?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                entries.push(WorkspaceEntry {
                    path: display_relative(&child_relative),
                    kind: WorkspaceEntryKind::Directory,
                    size: 0,
                    modified_at: modified_at(&child_path),
                    revision: "directory".to_owned(),
                });
                self.collect_entries(&child_path, &child_relative, entries)?;
            } else if file_type.is_file() {
                let content = fs::read(&child_path).map_err(|error| {
                    LoomError::new(
                        ErrorCode::ToolExecution,
                        format!("could not read '{}': {error}", child_relative.display()),
                        false,
                    )
                })?;
                entries.push(WorkspaceEntry {
                    path: display_relative(&child_relative),
                    kind: WorkspaceEntryKind::File,
                    size: content.len() as u64,
                    modified_at: modified_at(&child_path),
                    revision: revision_bytes(&content),
                });
            }
        }
        Ok(())
    }

    fn resolve_relative(&self, relative: &str, allow_missing: bool) -> Result<PathBuf> {
        let path = Path::new(relative);
        if path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
        {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("path '{relative}' must stay inside the workspace root"),
                false,
            ));
        }
        let mut resolved = self.inner.root.clone();
        let mut unresolved = false;
        for component in path.components() {
            let Component::Normal(component) = component else {
                continue;
            };
            if unresolved {
                resolved.push(component);
                continue;
            }
            let candidate = resolved.join(component);
            match fs::symlink_metadata(&candidate) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    let canonical = fs::canonicalize(&candidate).map_err(|error| {
                        LoomError::new(
                            ErrorCode::WorkspaceAccessDenied,
                            format!("could not resolve '{relative}': {error}"),
                            false,
                        )
                    })?;
                    if !canonical.starts_with(&self.inner.root) {
                        return Err(LoomError::new(
                            ErrorCode::WorkspaceAccessDenied,
                            format!("path '{relative}' must stay inside the workspace root"),
                            false,
                        ));
                    }
                    resolved = canonical;
                }
                Ok(_) => {
                    let canonical = fs::canonicalize(&candidate).map_err(|error| {
                        LoomError::new(
                            ErrorCode::WorkspaceAccessDenied,
                            format!("could not resolve '{relative}': {error}"),
                            false,
                        )
                    })?;
                    if !canonical.starts_with(&self.inner.root) {
                        return Err(LoomError::new(
                            ErrorCode::WorkspaceAccessDenied,
                            format!("path '{relative}' must stay inside the workspace root"),
                            false,
                        ));
                    }
                    resolved = canonical;
                }
                Err(error) if error.kind() == ErrorKind::NotFound && allow_missing => {
                    resolved.push(component);
                    unresolved = true;
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    return Err(LoomError::new(
                        ErrorCode::NotFound,
                        format!("workspace path '{relative}' was not found"),
                        false,
                    ));
                }
                Err(error) => {
                    return Err(LoomError::new(
                        ErrorCode::WorkspaceAccessDenied,
                        format!("could not resolve '{relative}': {error}"),
                        false,
                    ));
                }
            }
        }
        if !resolved.starts_with(&self.inner.root) {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("path '{relative}' must stay inside the workspace root"),
                false,
            ));
        }
        Ok(resolved)
    }

    fn lock_state(&self) -> Result<MutexGuard<'_, WorkspaceState>> {
        self.inner.state.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "workspace state lock was poisoned",
                true,
            )
        })
    }
}

impl WorkspaceWatcher {
    pub fn poll(&self) -> Result<Vec<WorkspaceChange>> {
        self.workspace.poll_changes()
    }

    pub fn events_since(&self, after: Option<EventSequence>) -> Result<Vec<WorkspaceChange>> {
        self.workspace.changes_since(after)
    }
}

fn display_relative(path: &Path) -> String {
    let value = path.to_string_lossy().replace('\\', "/");
    value.strip_prefix("./").unwrap_or(&value).to_owned()
}

fn modified_at(path: &Path) -> Option<Timestamp> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| duration.as_millis().try_into().ok())
        .map(Timestamp::from_unix_millis)
}

fn revision(content: &str) -> String {
    revision_bytes(content.as_bytes())
}

fn revision_bytes(content: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in content {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn unified_diff(path: &str, before: &str, after: &str) -> String {
    let mut diff = format!("--- {path}\n+++ {path}\n");
    for line in before.lines() {
        diff.push('-');
        diff.push_str(line);
        diff.push('\n');
    }
    for line in after.lines() {
        diff.push('+');
        diff.push_str(line);
        diff.push('\n');
    }
    diff
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use loom_core::{ErrorCode, ProjectId};

    use super::*;

    fn workspace() -> (Workspace, PathBuf) {
        let root = std::env::temp_dir().join(format!("loom-workspace-{}", ProjectId::new()));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("README.md"), "hello\n").unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn answer() -> u8 { 1 }\n").unwrap();
        (Workspace::open(ProjectId::new(), &root).unwrap(), root)
    }

    #[test]
    fn snapshots_and_reads_are_workspace_scoped() {
        let (workspace, root) = workspace();
        let snapshot = workspace.snapshot().unwrap();
        assert!(
            snapshot
                .entries
                .iter()
                .any(|entry| entry.path == "README.md")
        );
        assert_eq!(workspace.read_file("README.md").unwrap().content, "hello\n");
        let error = workspace.read_file("../outside").unwrap_err();
        assert_eq!(error.code, ErrorCode::WorkspaceAccessDenied);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn edits_require_matching_revision_and_emit_changes() {
        let (workspace, root) = workspace();
        let before = workspace.read_file("README.md").unwrap();
        let result = workspace
            .apply_edit(WorkspaceEdit {
                path: "README.md".to_owned(),
                old_text: "hello".to_owned(),
                new_text: "updated".to_owned(),
                expected_revision: Some(before.revision),
            })
            .unwrap();
        assert_ne!(result.before_revision, result.after_revision);
        assert_eq!(
            workspace.read_file("README.md").unwrap().content,
            "updated\n"
        );
        let changes = workspace.changes_since(None).unwrap();
        assert!(changes.iter().any(|change| change.path == "README.md"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checkpoint_revert_and_user_takeover_are_conflict_safe() {
        let (workspace, root) = workspace();
        let checkpoint = workspace
            .create_checkpoint(None, "before agent edit")
            .unwrap();
        let before = workspace.read_file("README.md").unwrap();
        workspace
            .apply_edit(WorkspaceEdit {
                path: "README.md".to_owned(),
                old_text: "hello".to_owned(),
                new_text: "agent".to_owned(),
                expected_revision: Some(before.revision),
            })
            .unwrap();
        workspace.take_control(WorkspaceControl::User).unwrap();
        let error = workspace
            .apply_edit(WorkspaceEdit {
                path: "README.md".to_owned(),
                old_text: "agent".to_owned(),
                new_text: "blocked".to_owned(),
                expected_revision: None,
            })
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Conflict);
        workspace
            .apply_user_edit(WorkspaceEdit {
                path: "README.md".to_owned(),
                old_text: "agent".to_owned(),
                new_text: "user".to_owned(),
                expected_revision: None,
            })
            .unwrap();
        let error = workspace.revert_checkpoint(checkpoint.id).unwrap_err();
        assert_eq!(error.code, ErrorCode::Conflict);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn undo_reverts_an_uncontested_agent_edit() {
        let (workspace, root) = workspace();
        let before = workspace.read_file("README.md").unwrap();
        workspace
            .apply_edit(WorkspaceEdit {
                path: "README.md".to_owned(),
                old_text: "hello".to_owned(),
                new_text: "agent".to_owned(),
                expected_revision: Some(before.revision),
            })
            .unwrap();
        workspace.undo_last_agent_edit().unwrap();
        assert_eq!(workspace.read_file("README.md").unwrap().content, "hello\n");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspace_state_restores_checkpoints_and_user_control() {
        let (workspace, root) = workspace();
        let checkpoint = workspace
            .create_checkpoint(None, "durable checkpoint")
            .unwrap();
        workspace.take_control(WorkspaceControl::User).unwrap();
        let state = workspace.export_state().unwrap();
        let restored = Workspace::open(workspace.project_id(), &root).unwrap();
        restored.restore_state(state).unwrap();

        assert_eq!(restored.control().unwrap(), WorkspaceControl::User);
        assert_eq!(restored.checkpoint(checkpoint.id).unwrap(), checkpoint);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_that_escape_the_workspace() {
        use std::os::unix::fs::symlink;

        let (workspace, root) = workspace();
        let outside = root.with_extension("outside");
        fs::write(&outside, "secret\n").unwrap();
        symlink(&outside, root.join("outside-link")).unwrap();

        let read = workspace.read_file("outside-link").unwrap_err();
        assert_eq!(read.code, ErrorCode::WorkspaceAccessDenied);
        let edit = workspace
            .apply_edit(WorkspaceEdit {
                path: "outside-link".to_owned(),
                old_text: "secret".to_owned(),
                new_text: "changed".to_owned(),
                expected_revision: None,
            })
            .unwrap_err();
        assert_eq!(edit.code, ErrorCode::WorkspaceAccessDenied);

        fs::remove_file(root.join("outside-link")).unwrap();
        fs::remove_file(outside).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
