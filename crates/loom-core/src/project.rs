use crate::{AgentSessionId, AgentSessionState, EventSequence, ProjectId, Timestamp};
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
}
