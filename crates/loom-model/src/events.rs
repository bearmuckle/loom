//! Provider-neutral server and workspace event records.
//!
//! These are the durable event shapes persisted by the backend and streamed to
//! clients. They live in the neutral domain layer so storage and the agent
//! runtime do not depend on the protocol contract crate.

use loom_core::{
    AgentMessageRecord, AgentSessionId, AgentSessionSnapshot, CURRENT_PROTOCOL_VERSION,
    DelegatedTaskRecord, EventSequence, ProjectAgentRecord, ProjectWorktreeRecord, ProtocolVersion,
    SessionEvent, SessionEventRecord, SessionFilesystemChange, TaskEventRecord,
    TerminalEventRecord, WorkspaceId,
};
use serde::{Deserialize, Serialize};

use crate::{AgentEvent, ProviderHealth, ProviderId};

pub const MAX_AGENT_RUN_MESSAGE_PAGE_SIZE: u32 = 100;
pub const MAX_AGENT_RUN_MESSAGE_CONTENT_RANGE_BYTES: u32 = 256 * 1024;
pub const MAX_AGENT_RUN_TRANSCRIPT_PAGE_SIZE: u32 = 32;
pub const MAX_AGENT_RUN_TRANSCRIPT_MESSAGE_BYTES: u32 = 32 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServerEventEnvelope {
    pub protocol_version: ProtocolVersion,
    pub sequence: EventSequence,
    pub session_id: AgentSessionId,
    pub event: ServerEvent,
}

/// A change to workspace catalog or configuration state. Workspace events have
/// their own scope and never borrow a session ID.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceEventEnvelope {
    pub protocol_version: ProtocolVersion,
    pub sequence: EventSequence,
    pub workspace_id: WorkspaceId,
    pub event: WorkspaceEvent,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum WorkspaceEvent {
    Renamed { name: String },
    ConfigChanged { revision: u64 },
}

/// Unified entries in a workspace reconnect stream.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
#[allow(clippy::large_enum_variant)]
pub enum WorkspaceFeedEvent {
    Session(ServerEventEnvelope),
    Workspace(WorkspaceEventEnvelope),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum ServerEvent {
    ProjectChildWorktreeUpdated {
        worktree: ProjectWorktreeRecord,
    },
    ProjectTaskUpdated {
        task: DelegatedTaskRecord,
    },
    ProjectAgentMessageAccepted {
        message: AgentMessageRecord,
    },
    ProjectAgentCreated {
        agent: ProjectAgentRecord,
    },
    ProjectAgentUpdated {
        agent: ProjectAgentRecord,
    },
    AgentSessionCreated {
        snapshot: AgentSessionSnapshot,
    },
    AgentSessionStateChanged {
        previous: loom_core::AgentSessionState,
        current: loom_core::AgentSessionState,
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
    Agent {
        event: AgentEvent,
    },
    SessionFilesystemChanged {
        change: SessionFilesystemChange,
    },
    Terminal {
        event: TerminalEventRecord,
    },
    Task {
        event: TaskEventRecord,
    },
    ProviderHealthChanged {
        provider_id: ProviderId,
        health: ProviderHealth,
    },
}

impl From<SessionEventRecord> for ServerEventEnvelope {
    fn from(record: SessionEventRecord) -> Self {
        Self::from_session_event(record.sequence, record.session_id, record.event)
    }
}

impl ServerEventEnvelope {
    pub fn from_session_event(
        sequence: EventSequence,
        session_id: AgentSessionId,
        event: SessionEvent,
    ) -> Self {
        let event = match event {
            SessionEvent::AgentSessionCreated { snapshot } => {
                ServerEvent::AgentSessionCreated { snapshot }
            }
            SessionEvent::AgentSessionStateChanged {
                previous, current, ..
            } => ServerEvent::AgentSessionStateChanged { previous, current },
            SessionEvent::AgentSessionForked {
                source_session_id,
                snapshot,
            } => ServerEvent::AgentSessionForked {
                source_session_id,
                snapshot,
            },
            SessionEvent::AgentSessionRenamed { session_id, name } => {
                ServerEvent::AgentSessionRenamed { session_id, name }
            }
            SessionEvent::AgentSessionArchived { session_id } => {
                ServerEvent::AgentSessionArchived { session_id }
            }
        };
        Self {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            sequence,
            session_id,
            event,
        }
    }

    pub fn from_agent_event(
        sequence: EventSequence,
        session_id: AgentSessionId,
        event: AgentEvent,
    ) -> Self {
        Self {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            sequence,
            session_id,
            event: ServerEvent::Agent { event },
        }
    }
}
