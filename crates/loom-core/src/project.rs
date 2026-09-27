use crate::{
    AgentMessageId, AgentSessionId, AgentSessionState, EventSequence, ProjectId, TaskId, Timestamp,
};
use serde::{Deserialize, Serialize};

/// Maximum hierarchy depth; the project manager is level one.
pub const MAX_PROJECT_AGENT_DEPTH: u8 = 3;

/// A session's position and client-visible status within a project hierarchy.
/// Depth is one for the project root, two for its direct children, and three
/// for the final supported level.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectAgentRecord {
    pub session_id: AgentSessionId,
    pub project_id: ProjectId,
    pub parent_session_id: Option<AgentSessionId>,
    pub depth: u8,
    pub state: AgentSessionState,
    pub task_summary: Option<String>,
    pub output_cursor: EventSequence,
    pub updated_at: Timestamp,
}

/// Authoritative project hierarchy projection. Existing sessions are
/// represented as roots by setting `project_id` to the root session ID.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectSnapshot {
    pub project_id: ProjectId,
    pub root_session_id: AgentSessionId,
    pub agents: Vec<ProjectAgentRecord>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskContextReference {
    pub label: String,
    pub uri: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegatedTaskStatus {
    Queued,
    Running,
    Blocked,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DelegatedTaskSpec {
    pub intent: String,
    pub context_references: Vec<TaskContextReference>,
    pub dependencies: Vec<TaskId>,
    pub code_change: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DelegatedTaskRecord {
    pub task_id: TaskId,
    pub project_id: ProjectId,
    pub requester_session_id: AgentSessionId,
    pub target_session_id: AgentSessionId,
    pub child_name: String,
    pub intent: String,
    pub context_references: Vec<TaskContextReference>,
    pub dependencies: Vec<TaskId>,
    pub code_change: bool,
    pub status: DelegatedTaskStatus,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// A durable, project-ordered message accepted for delivery to a project agent.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentMessageRecord {
    pub message_id: AgentMessageId,
    pub project_id: ProjectId,
    pub task_id: Option<TaskId>,
    pub sender_session_id: AgentSessionId,
    pub target_session_id: AgentSessionId,
    pub kind: AgentMessageKind,
    /// Monotonic sequence assigned when this message is durably accepted.
    pub project_sequence: u64,
    pub accepted_at: Timestamp,
    pub body: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMessageKind {
    Progress,
    Result,
    Question,
    Blocker,
    Direction,
    Answer,
}

/// Caller supplied portion of a message; persistence assigns its ID, project order,
/// and acceptance timestamp atomically with storing the message.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentMessageDraft {
    pub project_id: ProjectId,
    pub task_id: Option<TaskId>,
    pub sender_session_id: AgentSessionId,
    pub target_session_id: AgentSessionId,
    pub kind: AgentMessageKind,
    pub body: String,
}

#[cfg(test)]
mod tests {
    use super::{ProjectAgentRecord, ProjectSnapshot};
    use crate::{AgentSessionId, AgentSessionState, EventSequence, ProjectId, Timestamp};

    #[test]
    fn project_snapshot_represents_legacy_root_and_child_metadata() {
        let root = AgentSessionId::new();
        let project_id = ProjectId::from_uuid(*root.as_uuid());
        let snapshot = ProjectSnapshot {
            project_id,
            root_session_id: root,
            agents: vec![
                ProjectAgentRecord {
                    session_id: root,
                    project_id,
                    parent_session_id: None,
                    depth: 1,
                    state: AgentSessionState::Idle,
                    task_summary: None,
                    output_cursor: EventSequence::default(),
                    updated_at: Timestamp::now(),
                },
                ProjectAgentRecord {
                    session_id: AgentSessionId::new(),
                    project_id,
                    parent_session_id: Some(root),
                    depth: 2,
                    state: AgentSessionState::Executing,
                    task_summary: Some("Inspect protocol".into()),
                    output_cursor: EventSequence::new(3),
                    updated_at: Timestamp::now(),
                },
            ],
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        let decoded: ProjectSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, snapshot);
        assert_eq!(decoded.agents[0].depth, 1);
        assert_eq!(decoded.agents[1].depth, 2);
        assert_eq!(decoded.agents[1].parent_session_id, Some(root));
    }

    #[test]
    fn delegated_task_and_durable_message_contracts_round_trip() {
        use super::{
            AgentMessageDraft, AgentMessageKind, AgentMessageRecord, DelegatedTaskRecord,
            DelegatedTaskSpec, DelegatedTaskStatus, TaskContextReference,
        };
        use crate::{AgentMessageId, AgentSessionId, ProjectId, TaskId, Timestamp};

        let project_id = ProjectId::new();
        let requester = AgentSessionId::new();
        let target = AgentSessionId::new();
        let task_id = TaskId::new();
        let spec = DelegatedTaskSpec {
            intent: "Review migration".into(),
            context_references: vec![TaskContextReference {
                label: "Design".into(),
                uri: "docs/design.md".into(),
            }],
            dependencies: vec![],
            code_change: true,
        };
        let task = DelegatedTaskRecord {
            task_id,
            project_id,
            requester_session_id: requester,
            target_session_id: target,
            child_name: "reviewer".into(),
            intent: spec.intent.clone(),
            context_references: spec.context_references.clone(),
            dependencies: spec.dependencies.clone(),
            code_change: spec.code_change,
            status: DelegatedTaskStatus::Queued,
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
        };
        let draft = AgentMessageDraft {
            project_id,
            task_id: Some(task_id),
            sender_session_id: requester,
            target_session_id: target,
            kind: AgentMessageKind::Direction,
            body: "Please inspect the migration boundary.".into(),
        };
        let message = AgentMessageRecord {
            message_id: AgentMessageId::new(),
            project_id,
            task_id: draft.task_id,
            sender_session_id: draft.sender_session_id,
            target_session_id: draft.target_session_id,
            kind: draft.kind,
            project_sequence: 7,
            accepted_at: Timestamp::now(),
            body: draft.body.clone(),
        };
        for value in [
            serde_json::to_value(spec).unwrap(),
            serde_json::to_value(task).unwrap(),
            serde_json::to_value(draft).unwrap(),
            serde_json::to_value(message).unwrap(),
        ] {
            assert!(value.is_object());
        }
    }
}
