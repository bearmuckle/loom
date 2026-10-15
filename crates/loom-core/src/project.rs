use crate::{
    AgentMessageId, AgentSessionId, AgentSessionState, EventSequence, ProjectId,
    ProjectManagerWaitId, RunAttemptId, RunId, TaskId, Timestamp, ToolCallId,
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
    /// Durable task intent and lifecycle state for delegated project agents.
    #[serde(default)]
    pub tasks: Vec<DelegatedTaskRecord>,
    /// Durable checkout ownership, review, integration, and cleanup state.
    pub worktrees: Vec<ProjectWorktreeRecord>,
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
    /// Provider model selected for this child; persisted so retries and restart
    /// recovery use the same model.
    pub model_id: String,
    pub context_references: Vec<TaskContextReference>,
    pub dependencies: Vec<TaskId>,
    pub code_change: bool,
    /// Independent project-agent capabilities granted to the child task.
    #[serde(default)]
    pub permissions: ProjectAgentPermissions,
}

/// Capabilities independently granted to one delegated project agent.
/// Missing fields in older serialized task records default to disabled.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct ProjectAgentPermissions {
    pub delegation: bool,
    pub branch_messaging: bool,
    pub child_control: bool,
    pub inspection: bool,
    pub worktree_creation: bool,
    pub review: bool,
    pub integration: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DelegatedTaskRecord {
    pub task_id: TaskId,
    pub project_id: ProjectId,
    pub requester_session_id: AgentSessionId,
    pub target_session_id: AgentSessionId,
    pub child_name: String,
    pub intent: String,
    pub model_id: String,
    pub context_references: Vec<TaskContextReference>,
    pub dependencies: Vec<TaskId>,
    pub code_change: bool,
    /// The child task's independent project-agent capability grants.
    #[serde(default)]
    pub permissions: ProjectAgentPermissions,
    pub status: DelegatedTaskStatus,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectWorktreeStatus {
    Creating,
    Ready,
    Stale,
    Conflict,
    Integrating,
    Integrated,
    RecoveryRequired,
    CleanupPending,
    Retained,
    Removed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectWorktreeCleanupDisposition {
    Retain,
    RemoveClean,
    DiscardChanges,
}

/// The revision a delegated child checkout was created from, together with how
/// far that revision is from the default branch of the remote that was fetched
/// for it.
///
/// A record exists only for a refresh that ran while the checkout was created,
/// so a stale `refs/remotes/origin/*` ref is never presented as the current
/// upstream tip. When that fetch fails the record states the failure and leaves
/// the drift unknown instead of reporting a cached answer.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum ProjectWorktreeBaseRefresh {
    /// The fetch performed for this record resolved the default-branch tip.
    Fetched {
        /// Remote that was fetched.
        remote: String,
        /// Revision the child checkout was created from.
        base_revision: String,
        /// Default branch that the fetch resolved for that remote.
        default_branch: String,
        /// Default-branch tip that the fetch resolved.
        default_branch_revision: String,
        /// Commits the base revision has that the tip does not.
        ahead: u32,
        /// Commits the tip has that the base revision does not.
        behind: u32,
        /// When that fetch finished.
        fetched_at: Timestamp,
    },
    /// The refresh failed, so the base revision's drift is unknown.
    Unavailable {
        /// Remote whose refresh failed.
        remote: String,
        /// Revision the child checkout was created from.
        base_revision: String,
        /// When the failed refresh was attempted.
        attempted_at: Timestamp,
        /// Why the refresh failed, as reported by the VCS layer.
        warning: String,
    },
}

impl ProjectWorktreeBaseRefresh {
    /// When the refresh behind this record finished or was attempted.
    pub fn reported_at(&self) -> Timestamp {
        match self {
            Self::Fetched { fetched_at, .. } => *fetched_at,
            Self::Unavailable { attempted_at, .. } => *attempted_at,
        }
    }

    /// One-line, manager-facing description of the base and its drift from the
    /// default branch resolved by the fetch behind this record.
    pub fn summary(&self) -> String {
        match self {
            Self::Fetched {
                remote,
                base_revision,
                default_branch,
                default_branch_revision,
                ahead,
                behind,
                ..
            } => {
                let base = short_revision(base_revision);
                let tip = short_revision(default_branch_revision);
                let upstream = format!("{remote}/{default_branch}");
                match (*ahead, *behind) {
                    (0, 0) => {
                        format!("created from {base}; {upstream} is at the same revision {tip}")
                    }
                    (0, behind) => format!(
                        "created from {base}; {upstream} is {} ahead at {tip}",
                        commit_count(behind)
                    ),
                    (ahead, 0) => format!(
                        "created from {base}; {upstream} is {} behind the base at {tip}",
                        commit_count(ahead)
                    ),
                    (ahead, behind) => format!(
                        "created from {base}, which has diverged from {upstream} at {tip}: {} only in the base, {} only upstream",
                        commit_count(ahead),
                        commit_count(behind)
                    ),
                }
            }
            Self::Unavailable {
                remote,
                base_revision,
                warning,
                ..
            } => format!(
                "created from {}; {remote} could not be refreshed, so the base's drift from its default branch is unknown ({warning})",
                short_revision(base_revision)
            ),
        }
    }
}

/// Shortens a Git revision for display; shorter revisions are unchanged.
fn short_revision(revision: &str) -> &str {
    revision.get(..8).unwrap_or(revision)
}

fn commit_count(count: u32) -> String {
    if count == 1 {
        "1 commit".to_owned()
    } else {
        format!("{count} commits")
    }
}

/// Durable identity and disposition of the isolated code checkout assigned to
/// a delegated task. The path is relative to the child's session filesystem.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectWorktreeRecord {
    pub project_id: ProjectId,
    pub task_id: TaskId,
    pub parent_session_id: AgentSessionId,
    pub child_session_id: AgentSessionId,
    pub parent_repository_id: crate::RepositoryId,
    pub child_repository_id: crate::RepositoryId,
    pub relative_path: String,
    pub worktree_name: String,
    pub branch_name: String,
    pub base_revision: String,
    /// Base freshness reported when this checkout was created. `None` only for
    /// records written before the refresh existed.
    #[serde(default)]
    pub base_refresh: Option<ProjectWorktreeBaseRefresh>,
    pub result_revision: Option<String>,
    pub integrated_revision: Option<String>,
    pub status: ProjectWorktreeStatus,
    pub conflict_paths: Vec<String>,
    pub error: Option<String>,
    pub cleanup_disposition: Option<ProjectWorktreeCleanupDisposition>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// Lifecycle state for a manager parked while it waits for selected child tasks.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectManagerWaitStatus {
    Waiting,
    Ready,
    Resuming,
    Consumed,
    Abandoned,
}

/// Durable identity and ordered child set for one manager wait/join operation.
/// The run, attempt, and tool-call tuple is the idempotency key used when a
/// checkpoint is retried after a crash.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectManagerWaitRecord {
    pub wait_id: ProjectManagerWaitId,
    pub run_id: RunId,
    pub attempt_id: RunAttemptId,
    pub tool_call_id: ToolCallId,
    pub manager_session_id: AgentSessionId,
    pub child_task_ids: Vec<TaskId>,
    pub status: ProjectManagerWaitStatus,
    /// Optional bounded summary persisted when selected children are ready.
    pub result_summary: Option<String>,
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
            tasks: vec![],
            worktrees: vec![],
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
            DelegatedTaskSpec, DelegatedTaskStatus, ProjectAgentPermissions, TaskContextReference,
        };
        use crate::{AgentMessageId, AgentSessionId, ProjectId, TaskId, Timestamp};

        let project_id = ProjectId::new();
        let requester = AgentSessionId::new();
        let target = AgentSessionId::new();
        let task_id = TaskId::new();
        let spec = DelegatedTaskSpec {
            intent: "Review migration".into(),
            model_id: "provider/review-model".into(),
            context_references: vec![TaskContextReference {
                label: "Design".into(),
                uri: "docs/design.md".into(),
            }],
            dependencies: vec![],
            code_change: true,
            permissions: ProjectAgentPermissions {
                delegation: true,
                branch_messaging: true,
                child_control: false,
                inspection: true,
                worktree_creation: true,
                review: true,
                integration: false,
            },
        };
        let task = DelegatedTaskRecord {
            task_id,
            project_id,
            requester_session_id: requester,
            target_session_id: target,
            child_name: "reviewer".into(),
            intent: spec.intent.clone(),
            model_id: spec.model_id.clone(),
            context_references: spec.context_references.clone(),
            dependencies: spec.dependencies.clone(),
            code_change: spec.code_change,
            permissions: spec.permissions,
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
        let legacy_spec = serde_json::json!({
            "intent": "Legacy task",
            "model_id": "provider/review-model",
            "context_references": [],
            "dependencies": [],
            "code_change": false
        });
        let legacy_spec: DelegatedTaskSpec = serde_json::from_value(legacy_spec).unwrap();
        assert_eq!(legacy_spec.permissions, ProjectAgentPermissions::default());
    }

    #[test]
    fn worktree_base_refresh_summarizes_drift_from_a_fetch() {
        use super::{ProjectWorktreeBaseRefresh, ProjectWorktreeRecord, ProjectWorktreeStatus};
        use crate::{RepositoryId, TaskId};

        let base = "0123456789abcdef0123456789abcdef01234567";
        let tip = "fedcba9876543210fedcba9876543210fedcba98";
        let fetched_at = Timestamp::from_unix_millis(1_760_000_000_000);
        let fetched = |ahead: u32, behind: u32| ProjectWorktreeBaseRefresh::Fetched {
            remote: "origin".into(),
            base_revision: base.into(),
            default_branch: "main".into(),
            default_branch_revision: tip.into(),
            ahead,
            behind,
            fetched_at,
        };

        assert_eq!(fetched(0, 0).reported_at(), fetched_at);
        assert_eq!(
            fetched(0, 0).summary(),
            "created from 01234567; origin/main is at the same revision fedcba98"
        );
        assert_eq!(
            fetched(0, 1).summary(),
            "created from 01234567; origin/main is 1 commit ahead at fedcba98"
        );
        assert_eq!(
            fetched(0, 15).summary(),
            "created from 01234567; origin/main is 15 commits ahead at fedcba98"
        );
        assert_eq!(
            fetched(2, 0).summary(),
            "created from 01234567; origin/main is 2 commits behind the base at fedcba98"
        );
        assert_eq!(
            fetched(2, 3).summary(),
            "created from 01234567, which has diverged from origin/main at fedcba98: 2 commits only in the base, 3 commits only upstream"
        );

        let unavailable = ProjectWorktreeBaseRefresh::Unavailable {
            remote: "origin".into(),
            base_revision: base.into(),
            attempted_at: fetched_at,
            warning: "could not fetch the 'origin' remote: connection timed out".into(),
        };
        assert_eq!(unavailable.reported_at(), fetched_at);
        assert_eq!(
            unavailable.summary(),
            "created from 01234567; origin could not be refreshed, so the base's drift from its default branch is unknown (could not fetch the 'origin' remote: connection timed out)"
        );

        // The record round-trips with the refresh, and a record written before
        // the refresh existed decodes with the drift simply unknown.
        let record = ProjectWorktreeRecord {
            project_id: ProjectId::new(),
            task_id: TaskId::new(),
            parent_session_id: AgentSessionId::new(),
            child_session_id: AgentSessionId::new(),
            parent_repository_id: RepositoryId::new(),
            child_repository_id: RepositoryId::new(),
            relative_path: "project-worktrees/child".into(),
            worktree_name: "loom-child".into(),
            branch_name: "loom/project-child".into(),
            base_revision: base.into(),
            base_refresh: Some(fetched(0, 15)),
            result_revision: None,
            integrated_revision: None,
            status: ProjectWorktreeStatus::Ready,
            conflict_paths: vec![],
            error: None,
            cleanup_disposition: None,
            created_at: fetched_at,
            updated_at: fetched_at,
        };
        let encoded = serde_json::to_value(&record).unwrap();
        assert_eq!(encoded["base_refresh"]["state"], "fetched");
        assert_eq!(encoded["base_refresh"]["behind"], 15);
        assert_eq!(
            serde_json::from_value::<ProjectWorktreeRecord>(encoded).unwrap(),
            record
        );

        let mut legacy = serde_json::to_value(&record).unwrap();
        legacy.as_object_mut().unwrap().remove("base_refresh");
        let legacy: ProjectWorktreeRecord = serde_json::from_value(legacy).unwrap();
        assert_eq!(legacy.base_refresh, None);
    }
}
