//! Multi-user collaboration records: who said or did what, and to whom.
//!
//! Every entry is attributed by the server from the authenticated connection;
//! a client never names the author.

use crate::{
    AgentSessionId, CheckpointId, InteractionId, ParticipantIdentity, RunAttemptId, RunId,
    SessionRole, Timestamp, ToolCallId,
};
use serde::{Deserialize, Serialize};

/// Most UTF-8 bytes a peer chat message, direction, or answer may carry.
pub const MAX_COLLABORATION_TEXT_BYTES: usize = 16 * 1024;

/// Who performed a persisted message or visible control action.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Author {
    /// A person, with the role they held in the session when they acted.
    Participant {
        participant: ParticipantIdentity,
        role: SessionRole,
    },
    /// An operator connection that is not bound to a person (for example the
    /// local desktop client or a scope-only token).
    Operator,
}

/// Whom a history entry is addressed to.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Recipient {
    /// The agent: the entry directs or controls the run.
    Agent,
    /// The people in the session: peer chat, never seen by the model.
    People,
}

/// The run state a direction or control request is decided against.
///
/// Ordering: the server serializes every agent direction, answer, and run
/// control action per run. An action is accepted only when the revision it
/// carries equals the run's current revision; it is then applied and
/// `action_sequence` advances by one (the runtime may also advance
/// `control_revision` or start a new attempt). Otherwise the action is
/// answered with a conflict that carries the current revision and it is not
/// applied. Two people acting on the same revision therefore get exactly one
/// acceptance and one conflict, in the order the server received them; the
/// loser re-reads the run and decides again.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct RunRevision {
    pub attempt_id: RunAttemptId,
    /// The runtime's control revision.
    pub control_revision: u64,
    /// Count of people's actions the server accepted for this run since the
    /// run was loaded. It advances even when an action, such as a direction
    /// queued behind an in-flight model turn, leaves the control revision
    /// unchanged.
    pub action_sequence: u64,
}

/// A run control action taken by a person.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RunControlAction {
    Approve {
        tool_call_id: ToolCallId,
    },
    Reject {
        tool_call_id: ToolCallId,
        reason: Option<String>,
    },
    Pause,
    Resume,
    Interrupt,
    Retry,
    RetryFromCheckpoint {
        checkpoint_id: CheckpointId,
    },
}

/// What a history entry records.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum CollaborationEntryKind {
    /// Chat between people. Never part of model context; never starts or
    /// continues a run.
    PeerMessage { text: String },
    /// A task or follow-up direction to the agent.
    AgentDirection { text: String },
    /// An answer to an agent question.
    AgentAnswer {
        interaction_id: Option<InteractionId>,
        text: String,
    },
    /// A visible run control action.
    RunControl { action: RunControlAction },
    /// A person's access changed; `role` is `None` when it was revoked.
    AccessChanged {
        participant: ParticipantIdentity,
        role: Option<SessionRole>,
    },
}

/// One attributed entry in a session's shared history.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CollaborationEntry {
    pub session_id: AgentSessionId,
    pub run_id: Option<RunId>,
    pub author: Author,
    pub recipient: Recipient,
    pub kind: CollaborationEntryKind,
    /// The run revision after the action was applied, for run actions.
    pub revision: Option<RunRevision>,
    pub created_at: Timestamp,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ParticipantId;

    #[test]
    fn entries_round_trip_with_author_and_recipient() {
        let entry = CollaborationEntry {
            session_id: AgentSessionId::new(),
            run_id: None,
            author: Author::Participant {
                participant: ParticipantIdentity {
                    id: ParticipantId::new(),
                    display_name: "Sam".to_owned(),
                },
                role: SessionRole::Participant,
            },
            recipient: Recipient::People,
            kind: CollaborationEntryKind::PeerMessage {
                text: "hi".to_owned(),
            },
            revision: None,
            created_at: Timestamp::now(),
        };
        let encoded = serde_json::to_value(&entry).unwrap();
        assert_eq!(encoded["recipient"], "people");
        assert_eq!(encoded["author"]["data"]["role"], "participant");
        assert_eq!(encoded["kind"]["type"], "peer_message");
        assert_eq!(
            serde_json::from_value::<CollaborationEntry>(encoded).unwrap(),
            entry
        );
        assert_eq!(
            serde_json::to_value(Author::Operator).unwrap()["type"],
            "operator"
        );
    }
}
