use loom_core::{EventSequence, TaskId, TerminalId, Timestamp};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalStatus {
    Starting,
    Running,
    Exited,
    Cancelled,
    Failed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TerminalSnapshot {
    pub id: TerminalId,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub status: TerminalStatus,
    pub started_at: Timestamp,
    pub updated_at: Timestamp,
    pub exited_at: Option<Timestamp>,
    pub exit_code: Option<i32>,
    pub rows: u16,
    pub columns: u16,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum TerminalEvent {
    StateChanged {
        status: TerminalStatus,
    },
    Output {
        stream: TerminalStream,
        chunk: String,
    },
    Resized {
        rows: u16,
        columns: u16,
    },
    Exited {
        status: TerminalStatus,
        exit_code: Option<i32>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TerminalEventRecord {
    pub sequence: EventSequence,
    pub terminal_id: TerminalId,
    pub event: TerminalEvent,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Build,
    Test,
    Lint,
    Command,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskSpec {
    pub kind: TaskKind,
    pub label: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub output_limit_bytes: Option<usize>,
    pub artifact_paths: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskArtifact {
    pub path: String,
    pub exists: bool,
    pub size: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskEvidenceLink {
    pub label: String,
    pub uri: String,
    pub artifact_path: String,
    pub exists: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskSnapshot {
    pub id: TaskId,
    pub kind: TaskKind,
    pub label: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub status: TaskStatus,
    pub started_at: Timestamp,
    pub updated_at: Timestamp,
    pub completed_at: Option<Timestamp>,
    pub exit_code: Option<i32>,
    pub output: String,
    pub output_truncated: bool,
    pub artifacts: Vec<TaskArtifact>,
    #[serde(default)]
    pub evidence: Vec<TaskEvidenceLink>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum TaskEvent {
    StateChanged {
        status: TaskStatus,
    },
    OutputChunk {
        chunk: String,
    },
    Completed {
        status: TaskStatus,
        exit_code: Option<i32>,
        artifacts: Vec<TaskArtifact>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskEventRecord {
    pub sequence: EventSequence,
    pub task_id: TaskId,
    pub event: TaskEvent,
}
