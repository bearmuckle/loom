use std::collections::BTreeMap;

use crate::{AgentSessionId, CheckpointId, EventSequence, RepositoryId, Timestamp};
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
    /// Maximum number of delegated project agents running at once in this
    /// workspace. Queued tasks start as an active child finishes.
    #[serde(default = "default_project_agent_concurrency")]
    pub project_agent_concurrency: u8,
}

pub const MIN_PROJECT_AGENT_CONCURRENCY: u8 = 1;
pub const MAX_PROJECT_AGENT_CONCURRENCY: u8 = 16;

const fn default_project_agent_concurrency() -> u8 {
    4
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
            project_agent_concurrency: default_project_agent_concurrency(),
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionDirectory {
    pub source: String,
    pub path: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitHubRepository {
    pub full_name: String,
    pub description: Option<String>,
    pub clone_url: String,
    pub private: bool,
    pub default_branch: String,
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
pub struct SessionFilesystemChange {
    pub sequence: EventSequence,
    pub session_id: AgentSessionId,
    pub path: String,
    pub kind: WorkspaceChangeKind,
    pub revision: Option<String>,
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
    pub session_id: AgentSessionId,
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

#[cfg(test)]
mod tests {
    use super::WorkspaceConfig;

    #[test]
    fn older_workspace_config_defaults_project_agent_concurrency() {
        let config: WorkspaceConfig = serde_json::from_str(
            r#"{"revision":3,"worker_nodes":[],"cpu_pulse_threshold_percent":15}"#,
        )
        .unwrap();

        assert_eq!(config.revision, 3);
        assert_eq!(config.cpu_pulse_threshold_percent, 15);
        assert_eq!(config.project_agent_concurrency, 4);
    }
}
