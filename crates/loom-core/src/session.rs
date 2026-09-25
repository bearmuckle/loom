use serde::{Deserialize, Serialize};
use uuid::Uuid;

fn missing_workspace_id() -> WorkspaceId {
    WorkspaceId::from_uuid(Uuid::nil())
}

use crate::{AgentSessionId, EventSequence, ProjectId, Timestamp, WorkspaceId};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionState {
    Idle,
    Queued,
    Planning,
    AwaitingApproval,
    Paused,
    Executing,
    Evaluating,
    NeedsInput,
    Completed,
    Failed,
    Cancelled,
    Archived,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentSessionSnapshot {
    pub id: AgentSessionId,
    #[serde(default = "missing_workspace_id")]
    pub workspace_id: WorkspaceId,
    /// Compatibility identity for protocol 1.x clients. New code must use
    /// `workspace_id`; this value no longer identifies a filesystem root.
    pub project_id: ProjectId,
    pub name: String,
    pub state: AgentSessionState,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum SessionEvent {
    AgentSessionCreated {
        snapshot: AgentSessionSnapshot,
    },
    AgentSessionStateChanged {
        session_id: AgentSessionId,
        previous: AgentSessionState,
        current: AgentSessionState,
    },
    AgentSessionForked {
        source_session_id: AgentSessionId,
        snapshot: AgentSessionSnapshot,
    },
    AgentSessionRenamed {
        session_id: AgentSessionId,
        name: String,
    },
    AgentSessionArchived {
        session_id: AgentSessionId,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionEventRecord {
    pub sequence: EventSequence,
    pub session_id: AgentSessionId,
    pub occurred_at: Timestamp,
    pub event: SessionEvent,
}
