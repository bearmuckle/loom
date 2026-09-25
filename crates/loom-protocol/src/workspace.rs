use std::collections::BTreeMap;

pub use loom_core::WorkspaceRecord;
use loom_core::{AgentSessionId, CheckpointId, EventSequence, ProjectId, RepositoryId, Timestamp};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceConfig {
    /// Monotonically increasing per-workspace version used to ignore stale
    /// config updates arriving after newer node-membership changes.
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub worker_nodes: Vec<WorkerNodeConfig>,
    /// CPU usage above which session-card node indicators begin pulsing.
    #[serde(default = "default_cpu_pulse_threshold_percent")]
    pub cpu_pulse_threshold_percent: u8,
}

const fn default_cpu_pulse_threshold_percent() -> u8 {
    5
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            revision: 0,
            worker_nodes: Vec::new(),
            cpu_pulse_threshold_percent: default_cpu_pulse_threshold_percent(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerNodeConfig {
    /// WebSocket endpoint only. Access tokens are never part of distributed workspace config.
    pub url: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionRepository {
    pub id: RepositoryId,
    pub source: String,
    pub path: String,
    pub revision: Option<String>,
    pub attached_at: Timestamp,
}

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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionFilesystemSnapshot {
    pub session_id: AgentSessionId,
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
pub struct SessionFilesystemChange {
    pub sequence: EventSequence,
    pub session_id: AgentSessionId,
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
pub struct SessionFilesystemFile {
    pub session_id: AgentSessionId,
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
