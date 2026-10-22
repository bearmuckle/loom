use std::collections::BTreeMap;

use crate::{
    AgentSessionId, EventSequence, LoomError, Result, SessionTitleSource, Timestamp, WorkspaceId,
};
use serde::{Deserialize, Serialize};

/// Longest accepted session goal, in bytes of its trimmed UTF-8 encoding.
pub const MAX_AGENT_SESSION_GOAL_BYTES: usize = 8192;

/// Trims and validates a session goal before any state changes.
pub fn normalize_session_goal(raw: &str) -> Result<String> {
    let goal = raw.trim();
    if goal.is_empty() {
        return Err(LoomError::invalid_request(
            "agent session goal must not be empty",
        ));
    }
    if goal.len() > MAX_AGENT_SESSION_GOAL_BYTES {
        return Err(LoomError::invalid_request(format!(
            "agent session goal must be at most {MAX_AGENT_SESSION_GOAL_BYTES} bytes"
        )));
    }
    Ok(goal.to_owned())
}

/// Serializable snapshot of the session manager, owned by the neutral domain
/// layer so storage does not depend on the session crate.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionManagerState {
    pub sessions: BTreeMap<AgentSessionId, AgentSessionSnapshot>,
    pub next_sequence: EventSequence,
}

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
    pub workspace_id: WorkspaceId,
    /// The session's name. A project's row in the sidebar is rendered from its
    /// root session's name, so this doubles as the project title.
    pub name: String,
    /// The session's stated goal, trimmed and non-empty when set. A project's
    /// root goal is what the manager is told it owns. The field is additive, so
    /// a snapshot written before it existed reads as no goal.
    #[serde(default)]
    pub goal: Option<String>,
    pub state: AgentSessionState,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// Who chose the current name. A manual rename pins it and stops automatic
    /// re-titling. A snapshot written before this field existed reads as
    /// `auto`.
    #[serde(default)]
    pub title_source: SessionTitleSource,
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
        /// Who chose the new name. The field is additive, so a record written
        /// before it existed reads as `auto`.
        #[serde(default)]
        source: SessionTitleSource,
    },
    AgentSessionGoalChanged {
        session_id: AgentSessionId,
        goal: String,
    },
    /// The session's title source changed without renaming it, so automation
    /// starts or stops for that session.
    AgentSessionTitleSourceChanged {
        session_id: AgentSessionId,
        source: SessionTitleSource,
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
