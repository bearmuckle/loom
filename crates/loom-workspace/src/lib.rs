use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::ErrorKind,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::UNIX_EPOCH,
};

use ignore::WalkBuilder;
use loom_core::{
    AgentSessionId, CheckpointId, ErrorCode, EventSequence, LoomError, Result, Timestamp,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use similar::TextDiff;

mod instructions;

pub use instructions::MAX_CONTEXT_FILE_BYTES;
pub use loom_protocol::{
    Checkpoint, CheckpointFile, ContextFileKind, ContextFileReference, RevertResult,
    SessionFilesystemChange, SessionFilesystemFile, SessionFilesystemSnapshot, UndoResult,
    WorkspaceChangeKind, WorkspaceControl, WorkspaceEdit, WorkspaceEditResult, WorkspaceEntry,
    WorkspaceEntryKind,
};

const MAX_SNAPSHOT_ENTRIES: usize = 100_000;

fn checked_mount_path(relative: &str) -> Result<PathBuf> {
    let path = Path::new(relative);
    if relative.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(LoomError::invalid_request(
            "session mount path must be a normalized relative path",
        ));
    }
    Ok(path.to_path_buf())
}

fn is_ignored_directory(name: &std::ffi::OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(".git" | "target" | "dist" | "node_modules" | ".venv" | "vendor")
    )
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceBytes {
    pub path: String,
    pub bytes: Vec<u8>,
    pub revision: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceByteWriteResult {
    pub path: String,
    pub before_revision: String,
    pub after_revision: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceEditHistory {
    pub path: String,
    pub before: Option<String>,
    #[serde(default)]
    pub before_bytes: Option<Vec<u8>>,
    pub after_revision: String,
    pub source: WorkspaceControl,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceStateSnapshot {
    pub session_id: AgentSessionId,
    pub root: String,
    pub control: WorkspaceControl,
    pub checkpoints: Vec<Checkpoint>,
    pub edits: Vec<WorkspaceEditHistory>,
    pub next_sequence: EventSequence,
    pub changes: Vec<SessionFilesystemChange>,
}

#[derive(Debug)]
struct WorkspaceState {
    control: WorkspaceControl,
    checkpoints: BTreeMap<CheckpointId, Checkpoint>,
    edits: Vec<EditRecord>,
    watcher_snapshot: Option<SessionFilesystemSnapshot>,
    next_sequence: EventSequence,
    changes: Vec<SessionFilesystemChange>,
}

#[derive(Clone, Debug)]
struct EditRecord {
    path: String,
    before: Option<String>,
    before_bytes: Option<Vec<u8>>,
    after_revision: String,
    source: WorkspaceControl,
}

#[derive(Debug)]
struct WorkspaceInner {
    session_id: AgentSessionId,
    root: PathBuf,
    state: Mutex<WorkspaceState>,
    mounts: Mutex<BTreeMap<String, PathBuf>>,
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
    pub fn open(session_id: AgentSessionId, root: impl Into<PathBuf>) -> Result<Self> {
        let requested = root.into();
        let root = Self::canonical_root(&requested)?;
        let workspace = Self {
            inner: Arc::new(WorkspaceInner {
                session_id,
                root,
                state: Mutex::new(WorkspaceState {
                    control: WorkspaceControl::Agent,
                    checkpoints: BTreeMap::new(),
                    edits: Vec::new(),
                    watcher_snapshot: None,
                    next_sequence: EventSequence::default(),
                    changes: Vec::new(),
                }),
                mounts: Mutex::new(BTreeMap::new()),
            }),
        };
        let snapshot = workspace.snapshot()?;
        workspace.lock_state()?.watcher_snapshot = Some(snapshot);
        Ok(workspace)
    }

    pub fn canonical_root(root: impl AsRef<Path>) -> Result<PathBuf> {
        let requested = root.as_ref();
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
        fs::canonicalize(requested).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!(
                    "could not resolve workspace root '{}': {error}",
                    requested.display()
                ),
                false,
            )
        })
    }

    pub fn session_id(&self) -> AgentSessionId {
        self.inner.session_id
    }

    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    pub fn mounted_directories(&self) -> Result<Vec<(String, PathBuf)>> {
        Ok(self
            .inner
            .mounts
            .lock()
            .map_err(|_| LoomError::invalid_state("workspace mounts are unavailable"))?
            .iter()
            .map(|(path, source)| (path.clone(), source.clone()))
            .collect())
    }

    pub fn mounted_source_for(&self, relative: &str) -> Result<Option<PathBuf>> {
        Ok(self
            .mounted_directories()?
            .into_iter()
            .find(|(path, _)| relative == path || relative.starts_with(&format!("{path}/")))
            .map(|(_, source)| source))
    }

    pub fn mount_directory(&self, relative: &str, source: impl AsRef<Path>) -> Result<PathBuf> {
        let source = Self::canonical_root(source)?;
        let relative_path = checked_mount_path(relative)?;
        let destination = self.inner.root.join(&relative_path);
        if destination.starts_with(&source) || source.starts_with(&destination) {
            return Err(LoomError::invalid_request(
                "a session directory cannot be mounted inside itself",
            ));
        }
        if let Some(parent) = destination.parent() {
            let parent_relative = relative_path.parent().unwrap_or(Path::new(""));
            if self
                .mounted_source_for(&display_relative(parent_relative))?
                .is_some()
            {
                return Err(LoomError::invalid_request(
                    "session mount path cannot be inside another attached directory",
                ));
            }
            fs::create_dir_all(parent).map_err(|error| {
                LoomError::new(
                    ErrorCode::WorkspaceAccessDenied,
                    format!("could not create mount parent: {error}"),
                    false,
                )
            })?;
            if !fs::canonicalize(parent)
                .map(|canonical| canonical.starts_with(&self.inner.root))
                .unwrap_or(false)
            {
                return Err(LoomError::new(
                    ErrorCode::WorkspaceAccessDenied,
                    "session mount parent escapes its filesystem root",
                    false,
                ));
            }
        }
        match fs::symlink_metadata(&destination) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                if fs::canonicalize(&destination).ok().as_deref() != Some(source.as_path()) {
                    return Err(LoomError::conflict("session mount path is already in use"));
                }
            }
            Ok(_) => return Err(LoomError::conflict("session mount path is already in use")),
            Err(error) if error.kind() == ErrorKind::NotFound => {
                #[cfg(unix)]
                std::os::unix::fs::symlink(&source, &destination).map_err(|error| {
                    LoomError::new(
                        ErrorCode::WorkspaceAccessDenied,
                        format!("could not attach local directory: {error}"),
                        false,
                    )
                })?;
                #[cfg(windows)]
                std::os::windows::fs::symlink_dir(&source, &destination).map_err(|error| {
                    LoomError::new(
                        ErrorCode::WorkspaceAccessDenied,
                        format!("could not attach local directory: {error}"),
                        false,
                    )
                })?;
            }
            Err(error) => {
                return Err(LoomError::new(
                    ErrorCode::WorkspaceAccessDenied,
                    format!("could not inspect session mount path: {error}"),
                    false,
                ));
            }
        }
        self.inner
            .mounts
            .lock()
            .map_err(|_| LoomError::invalid_state("workspace mounts are unavailable"))?
            .insert(relative.to_owned(), source.clone());
        Ok(source)
    }

    pub fn unmount_directory(&self, relative: &str) -> Result<()> {
        let relative_path = checked_mount_path(relative)?;
        let mut mounts = self
            .inner
            .mounts
            .lock()
            .map_err(|_| LoomError::invalid_state("workspace mounts are unavailable"))?;
        if !mounts.contains_key(relative) {
            return Err(LoomError::new(
                ErrorCode::NotFound,
                "session directory not attached",
                false,
            ));
        }
        fs::remove_file(self.inner.root.join(relative_path)).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not remove session directory link: {error}"),
                false,
            )
        })?;
        mounts.remove(relative);
        Ok(())
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

    pub fn resolve_path(&self, relative: &str, allow_missing: bool) -> Result<PathBuf> {
        self.resolve_relative(relative, allow_missing)
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
            session_id: self.inner.session_id,
            root: self.inner.root.display().to_string(),
            control: state.control,
            checkpoints: state.checkpoints.values().cloned().collect(),
            edits: state
                .edits
                .iter()
                .map(|edit| WorkspaceEditHistory {
                    path: edit.path.clone(),
                    before: edit.before.clone(),
                    before_bytes: edit.before_bytes.clone(),
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
        if persisted.session_id != self.inner.session_id
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
            .any(|change| change.session_id != self.inner.session_id)
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted filesystem change belongs to another session",
                false,
            ));
        }
        if persisted
            .checkpoints
            .iter()
            .any(|checkpoint| checkpoint.session_id != self.inner.session_id)
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted checkpoint belongs to another session",
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
                if !is_current_revision(&file.expected_revision) {
                    return Err(LoomError::new(
                        ErrorCode::MalformedPayload,
                        format!("persisted checkpoint expected revision for '{path}' is invalid"),
                        false,
                    ));
                }
            }
        }
        for edit in &persisted.edits {
            self.resolve_relative(&edit.path, true)?;
            if !is_current_revision(&edit.after_revision) {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted edit revision for '{}' is invalid", edit.path),
                    false,
                ));
            }
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
                before_bytes: edit.before_bytes,
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

    pub fn snapshot(&self) -> Result<SessionFilesystemSnapshot> {
        let mut entries = Vec::new();
        self.collect_entries(&self.inner.root, Path::new("."), &mut entries)?;
        for (relative, source) in self.mounted_directories()? {
            entries.push(WorkspaceEntry {
                path: relative.clone(),
                kind: WorkspaceEntryKind::Directory,
                size: 0,
                modified_at: modified_at(&source),
                revision: "directory".to_owned(),
            });
            self.collect_entries(&source, Path::new(&relative), &mut entries)?;
        }
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(SessionFilesystemSnapshot {
            session_id: self.inner.session_id,
            root: self.inner.root.display().to_string(),
            captured_at: Timestamp::now(),
            entries,
        })
    }

    pub fn read_file(&self, relative: &str) -> Result<SessionFilesystemFile> {
        let file = self.read_file_bytes(relative)?;
        let content = String::from_utf8(file.bytes).map_err(|error| {
            LoomError::new(
                ErrorCode::InvalidEncoding,
                format!("workspace file '{relative}' is not valid UTF-8: {error}"),
                false,
            )
        })?;
        Ok(SessionFilesystemFile {
            session_id: self.inner.session_id,
            path: relative.to_owned(),
            revision: revision(&content),
            content,
        })
    }

    pub fn read_file_bytes(&self, relative: &str) -> Result<WorkspaceBytes> {
        let path = self.resolve_relative(relative, false)?;
        let bytes = fs::read(&path).map_err(|error| {
            LoomError::new(
                ErrorCode::ToolExecution,
                format!("could not read '{relative}': {error}"),
                false,
            )
        })?;
        let revision = revision_bytes(&bytes);
        Ok(WorkspaceBytes {
            path: relative.to_owned(),
            bytes,
            revision,
        })
    }

    pub fn write_file_bytes(
        &self,
        relative: &str,
        bytes: Vec<u8>,
        expected_revision: Option<&str>,
    ) -> Result<WorkspaceByteWriteResult> {
        self.write_file_bytes_from(relative, bytes, expected_revision, WorkspaceControl::User)
    }

    pub fn write_file_bytes_from(
        &self,
        relative: &str,
        bytes: Vec<u8>,
        expected_revision: Option<&str>,
        source: WorkspaceControl,
    ) -> Result<WorkspaceByteWriteResult> {
        let mut state = self.lock_state()?;
        if source == WorkspaceControl::Agent && state.control == WorkspaceControl::User {
            return Err(LoomError::new(
                ErrorCode::Conflict,
                "workspace is under user control; agent edits are paused",
                false,
            ));
        }
        let path = self.resolve_relative(relative, true)?;
        let before_bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                return Err(LoomError::new(
                    ErrorCode::ToolExecution,
                    format!("could not read '{relative}' before writing: {error}"),
                    false,
                ));
            }
        };
        let before_revision = revision_bytes(&before_bytes);
        if expected_revision.is_some_and(|expected| expected != before_revision) {
            return Err(LoomError::new(
                ErrorCode::ExternalChange,
                format!(
                    "workspace file '{relative}' changed before saving (expected {}, found {before_revision})",
                    expected_revision.unwrap_or_default()
                ),
                false,
            ));
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                LoomError::new(
                    ErrorCode::ToolExecution,
                    format!("could not create write destination: {error}"),
                    false,
                )
            })?;
        }
        fs::write(&path, &bytes).map_err(|error| {
            LoomError::new(
                ErrorCode::ToolExecution,
                format!("could not write '{relative}': {error}"),
                false,
            )
        })?;
        let after_revision = revision_bytes(&bytes);
        state.edits.push(EditRecord {
            path: relative.to_owned(),
            before: String::from_utf8(before_bytes.clone()).ok(),
            before_bytes: Some(before_bytes),
            after_revision: after_revision.clone(),
            source,
        });
        Ok(WorkspaceByteWriteResult {
            path: relative.to_owned(),
            before_revision,
            after_revision,
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
        if let Some(expected) = edit.expected_revision.as_deref()
            && expected != before_revision
        {
            return Err(LoomError::conflict(format!(
                "workspace file '{}' changed before the edit (expected {expected}, found {before_revision})",
                edit.path
            )));
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
            before_bytes: None,
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

    pub fn create_checkpoint(&self, label: impl Into<String>) -> Result<Checkpoint> {
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
            let file = match self.read_file(&entry.path) {
                Ok(file) => file,
                Err(error) if error.code == ErrorCode::InvalidEncoding => {
                    // Text checkpoints cannot represent binary files. They
                    // remain in the workspace and are intentionally excluded
                    // from text rollback state.
                    continue;
                }
                Err(error) => return Err(error),
            };
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
            session_id: self.inner.session_id,
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
                before_bytes: None,
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
            None => match edit.before_bytes {
                Some(before) => fs::write(&path, before),
                None => fs::remove_file(&path).or_else(|error| {
                    if error.kind() == ErrorKind::NotFound {
                        Ok(())
                    } else {
                        Err(error)
                    }
                }),
            },
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

    pub fn poll_changes(&self) -> Result<Vec<SessionFilesystemChange>> {
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
                let change = SessionFilesystemChange {
                    sequence: state.next_sequence,
                    session_id: self.inner.session_id,
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

    pub fn changes_since(
        &self,
        after: Option<EventSequence>,
    ) -> Result<Vec<SessionFilesystemChange>> {
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
        let mut walker = WalkBuilder::new(path);
        walker
            .hidden(false)
            .git_ignore(true)
            .require_git(false)
            .git_global(false)
            .git_exclude(false)
            .parents(false)
            .ignore(false)
            .filter_entry(|entry| !is_ignored_directory(entry.file_name()));

        for entry in walker.build() {
            let entry = entry.map_err(|error| {
                LoomError::new(
                    ErrorCode::ToolExecution,
                    format!("could not read workspace entry: {error}"),
                    false,
                )
            })?;
            let child_path = entry.path();
            if child_path == path {
                continue;
            }
            if entries.len() >= MAX_SNAPSHOT_ENTRIES {
                return Err(LoomError::new(
                    ErrorCode::ToolExecution,
                    format!("workspace snapshot exceeds {MAX_SNAPSHOT_ENTRIES} entries"),
                    false,
                ));
            }
            let child_relative = child_path.strip_prefix(path).map_err(|error| {
                LoomError::new(
                    ErrorCode::ToolExecution,
                    format!("could not relativize '{}': {error}", child_path.display()),
                    false,
                )
            })?;
            let file_type = entry.file_type().ok_or_else(|| {
                LoomError::new(
                    ErrorCode::ToolExecution,
                    format!("could not inspect '{}'", child_relative.display()),
                    false,
                )
            })?;
            if file_type.is_symlink() {
                continue;
            }
            let path = display_relative(&relative.join(child_relative));
            if file_type.is_dir() {
                entries.push(WorkspaceEntry {
                    path,
                    kind: WorkspaceEntryKind::Directory,
                    size: 0,
                    modified_at: modified_at(child_path),
                    revision: "directory".to_owned(),
                });
            } else if file_type.is_file() {
                let content = fs::read(child_path).map_err(|error| {
                    LoomError::new(
                        ErrorCode::ToolExecution,
                        format!("could not read '{}': {error}", child_relative.display()),
                        false,
                    )
                })?;
                entries.push(WorkspaceEntry {
                    path,
                    kind: WorkspaceEntryKind::File,
                    size: content.len() as u64,
                    modified_at: modified_at(child_path),
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
        let mount = self
            .mounted_directories()?
            .into_iter()
            .filter_map(|(mount_path, source)| {
                path.strip_prefix(&mount_path)
                    .ok()
                    .map(|suffix| (mount_path.len(), source, suffix.to_path_buf()))
            })
            .max_by_key(|(length, _, _)| *length);
        let (allowed_root, path) = if let Some((_, source, suffix)) = mount {
            (source, suffix)
        } else {
            (self.inner.root.clone(), path.to_path_buf())
        };
        let mut resolved = allowed_root.clone();
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
                    if !canonical.starts_with(&allowed_root) {
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
                    if !canonical.starts_with(&allowed_root) {
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
        if !resolved.starts_with(&allowed_root) {
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
    pub fn poll(&self) -> Result<Vec<SessionFilesystemChange>> {
        self.workspace.poll_changes()
    }

    pub fn events_since(
        &self,
        after: Option<EventSequence>,
    ) -> Result<Vec<SessionFilesystemChange>> {
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
    let digest = Sha256::digest(content);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn is_current_revision(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn unified_diff(path: &str, before: &str, after: &str) -> String {
    TextDiff::from_lines(before, after)
        .unified_diff()
        .header(path, path)
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use loom_core::{AgentSessionId, ErrorCode};

    use super::*;

    fn workspace() -> (Workspace, PathBuf) {
        let session_id = AgentSessionId::new();
        let root = std::env::temp_dir().join(format!("loom-workspace-{session_id}"));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("README.md"), "hello\n").unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn answer() -> u8 { 1 }\n").unwrap();
        (Workspace::open(session_id, &root).unwrap(), root)
    }

    #[test]
    fn snapshots_and_reads_are_workspace_scoped() {
        let (workspace, root) = workspace();
        fs::create_dir_all(root.join("dist")).unwrap();
        fs::write(root.join("dist/generated.wasm"), [0_u8; 1024]).unwrap();
        let snapshot = workspace.snapshot().unwrap();
        assert!(
            snapshot
                .entries
                .iter()
                .any(|entry| entry.path == "README.md")
        );
        assert!(
            snapshot
                .entries
                .iter()
                .all(|entry| !entry.path.starts_with("dist"))
        );
        assert_eq!(workspace.read_file("README.md").unwrap().content, "hello\n");
        let error = workspace.read_file("../outside").unwrap_err();
        assert_eq!(error.code, ErrorCode::WorkspaceAccessDenied);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn watcher_reports_created_modified_and_deleted_paths_in_sequence() {
        let (workspace, root) = workspace();
        let watcher = workspace.watch();
        assert!(watcher.poll().unwrap().is_empty());

        fs::write(root.join("new.txt"), "created").unwrap();
        let created = watcher.poll().unwrap();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].path, "new.txt");
        assert_eq!(created[0].kind, WorkspaceChangeKind::Created);

        fs::write(root.join("new.txt"), "updated").unwrap();
        let modified = watcher.events_since(Some(created[0].sequence)).unwrap();
        assert_eq!(modified.len(), 1);
        assert_eq!(modified[0].kind, WorkspaceChangeKind::Modified);

        fs::remove_file(root.join("new.txt")).unwrap();
        let deleted = watcher.poll().unwrap();
        assert_eq!(deleted.len(), 1);
        assert_eq!(deleted[0].kind, WorkspaceChangeKind::Deleted);
        assert!(deleted[0].revision.is_none());
        assert!(deleted[0].sequence > modified[0].sequence);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn writes_and_edits_reject_stale_revisions_and_duplicate_text() {
        let (workspace, root) = workspace();
        let stale_write = workspace
            .write_file_bytes("README.md", b"overwrite".to_vec(), Some("stale"))
            .unwrap_err();
        assert_eq!(stale_write.code, ErrorCode::ExternalChange);

        let stale_edit = workspace
            .apply_edit(WorkspaceEdit {
                path: "README.md".to_owned(),
                old_text: "hello".to_owned(),
                new_text: "updated".to_owned(),
                expected_revision: Some("stale".to_owned()),
            })
            .unwrap_err();
        assert_eq!(stale_edit.code, ErrorCode::Conflict);

        let duplicate = workspace
            .apply_edit(WorkspaceEdit {
                path: "README.md".to_owned(),
                old_text: "l".to_owned(),
                new_text: "x".to_owned(),
                expected_revision: None,
            })
            .unwrap_err();
        assert_eq!(duplicate.code, ErrorCode::Conflict);

        let missing = workspace.read_file("missing.txt").unwrap_err();
        assert_eq!(missing.code, ErrorCode::NotFound);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn mounted_directory_edits_the_original_and_unmount_keeps_it() {
        let (workspace, root) = workspace();
        let source = std::env::temp_dir().join(format!("loom-source-{}", AgentSessionId::new()));
        fs::create_dir(&source).unwrap();
        fs::write(source.join("note.txt"), "before").unwrap();
        workspace.mount_directory("sources/local", &source).unwrap();
        assert_eq!(
            workspace
                .read_file("sources/local/note.txt")
                .unwrap()
                .content,
            "before"
        );
        assert!(
            workspace
                .snapshot()
                .unwrap()
                .entries
                .iter()
                .any(|entry| entry.path == "sources/local/note.txt")
        );
        workspace
            .write_file_bytes("sources/local/note.txt", b"after".to_vec(), None)
            .unwrap();
        assert_eq!(
            fs::read_to_string(source.join("note.txt")).unwrap(),
            "after"
        );
        workspace.unmount_directory("sources/local").unwrap();
        assert!(source.join("note.txt").exists());
        assert!(!root.join("sources/local").exists());
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(source).unwrap();
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
        fs::write(root.join("binary.bin"), [0, 159, 146, 150]).unwrap();
        let checkpoint = workspace.create_checkpoint("before agent edit").unwrap();
        assert!(!checkpoint.files.contains_key("binary.bin"));
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
        let checkpoint = workspace.create_checkpoint("durable checkpoint").unwrap();
        workspace.take_control(WorkspaceControl::User).unwrap();
        let state = workspace.export_state().unwrap();
        let restored = Workspace::open(workspace.session_id(), &root).unwrap();
        restored.restore_state(state).unwrap();

        assert_eq!(restored.control().unwrap(), WorkspaceControl::User);
        assert_eq!(restored.checkpoint(checkpoint.id).unwrap(), checkpoint);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mounts_reject_conflicts_nested_mounts_and_missing_unmounts() {
        let (workspace, root) = workspace();
        let source = root.with_extension("mount-source");
        let second_source = root.with_extension("mount-source-two");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&second_source).unwrap();
        assert_eq!(
            workspace
                .mount_directory("../escape", &source)
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            workspace.mount_directory("inside", &root).unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            workspace
                .mount_directory("README.md", &source)
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        workspace.mount_directory("attached", &source).unwrap();
        assert_eq!(
            workspace
                .mounted_source_for("attached/nested/file")
                .unwrap(),
            Some(fs::canonicalize(&source).unwrap())
        );
        assert_eq!(
            workspace
                .mount_directory("attached/child", &second_source)
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            workspace.unmount_directory("missing").unwrap_err().code,
            ErrorCode::NotFound
        );
        assert_eq!(
            workspace.directory_path("README.md").unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        workspace.unmount_directory("attached").unwrap();
        assert!(source.is_dir());
        fs::remove_dir_all(source).unwrap();
        fs::remove_dir_all(second_source).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restored_state_rejects_mismatched_identity_and_change_sequences() {
        let (workspace, root) = workspace();
        workspace.create_checkpoint("checkpoint").unwrap();
        let state = workspace.export_state().unwrap();
        let restored = Workspace::open(workspace.session_id(), &root).unwrap();

        let mut invalid = state.clone();
        invalid.session_id = AgentSessionId::new();
        assert_eq!(
            restored.restore_state(invalid).unwrap_err().code,
            ErrorCode::MalformedPayload
        );
        let mut invalid = state.clone();
        invalid.root.push_str("-different");
        assert_eq!(
            restored.restore_state(invalid).unwrap_err().code,
            ErrorCode::MalformedPayload
        );
        let mut invalid = state.clone();
        invalid.checkpoints[0].session_id = AgentSessionId::new();
        assert_eq!(
            restored.restore_state(invalid).unwrap_err().code,
            ErrorCode::MalformedPayload
        );

        let foreign_session = AgentSessionId::new();
        let mut invalid = state.clone();
        invalid.changes.push(SessionFilesystemChange {
            sequence: EventSequence::new(1),
            session_id: foreign_session,
            path: "README.md".to_owned(),
            kind: WorkspaceChangeKind::Modified,
            revision: None,
        });
        invalid.next_sequence = EventSequence::new(1);
        assert_eq!(
            restored.restore_state(invalid).unwrap_err().code,
            ErrorCode::MalformedPayload
        );

        let mut invalid = state.clone();
        invalid.changes = vec![
            SessionFilesystemChange {
                sequence: EventSequence::new(1),
                session_id: workspace.session_id(),
                path: "README.md".to_owned(),
                kind: WorkspaceChangeKind::Modified,
                revision: None,
            },
            SessionFilesystemChange {
                sequence: EventSequence::new(1),
                session_id: workspace.session_id(),
                path: "src/lib.rs".to_owned(),
                kind: WorkspaceChangeKind::Modified,
                revision: None,
            },
        ];
        invalid.next_sequence = EventSequence::new(1);
        assert_eq!(
            restored.restore_state(invalid).unwrap_err().code,
            ErrorCode::MalformedPayload
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspace_state_rejects_unsupported_revisions() {
        let (workspace, root) = workspace();
        workspace.create_checkpoint("checkpoint").unwrap();
        let before = workspace.read_file("README.md").unwrap();
        workspace
            .apply_edit(WorkspaceEdit {
                path: "README.md".to_owned(),
                old_text: "hello".to_owned(),
                new_text: "agent".to_owned(),
                expected_revision: Some(before.revision),
            })
            .unwrap();
        let state = workspace.export_state().unwrap();
        let restored = Workspace::open(workspace.session_id(), &root).unwrap();
        for field in ["revision", "expected_revision", "after_revision"] {
            let mut invalid = state.clone();
            match field {
                "revision" => {
                    invalid.checkpoints[0]
                        .files
                        .get_mut("README.md")
                        .unwrap()
                        .revision = "0123456789abcdef".to_owned();
                }
                "expected_revision" => {
                    invalid.checkpoints[0]
                        .files
                        .get_mut("README.md")
                        .unwrap()
                        .expected_revision = "0123456789abcdef".to_owned();
                }
                "after_revision" => {
                    invalid.edits[0].after_revision = "0123456789abcdef".to_owned();
                }
                _ => unreachable!(),
            }
            let error = restored.restore_state(invalid).unwrap_err();
            assert_eq!(error.code, ErrorCode::MalformedPayload, "{field}");
            assert!(error.message.contains("revision"), "{field}: {error}");
        }
        restored.restore_state(state).unwrap();
        assert_eq!(
            restored.export_state().unwrap(),
            workspace.export_state().unwrap()
        );
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
