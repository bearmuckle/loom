use crate::{
    AgentMessageId, AgentSessionId, AgentSessionState, EventSequence, ProjectId,
    ProjectManagerWaitId, RunAttemptId, RunId, TaskId, Timestamp, ToolCallId,
};
use serde::{Deserialize, Serialize};

/// Maximum hierarchy depth; the project manager is level one.
pub const MAX_PROJECT_AGENT_DEPTH: u8 = 3;

/// Paths one delegation may declare as a child's file scope.
pub const MAX_PROJECT_CHILD_SCOPE_PATHS: usize = 64;

/// Bytes of one declared scope path.
pub const MAX_PROJECT_CHILD_SCOPE_PATH_BYTES: usize = 4096;

/// Bytes of the reason recorded when a commit outside a declared scope lands.
pub const MAX_PROJECT_CHILD_SCOPE_REASON_BYTES: usize = 4096;

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
    /// The project this one was branched out of, when it is a sub project.
    ///
    /// A sub project is a project of its own: it owns a root session, its
    /// agents, and its checkouts. The link only records where it came from, so
    /// a client can group it under its parent.
    #[serde(default)]
    pub parent_project_id: Option<ProjectId>,
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
    /// The files a code child's commit may touch. `None` declares no scope, so
    /// the child's commit is not checked against one.
    #[serde(default)]
    pub scope: Option<ProjectChildScope>,
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

/// How a reviewed code result reached the parent history.
///
/// A manager that lands the reviewed change itself — by cherry-picking,
/// replaying, or squashing the child's commits — leaves no integration for
/// Loom to perform, so the record has to state which way the change arrived
/// instead of leaving the completion guard to refuse the result.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectWorktreeLandedBy {
    /// Loom integrated the reviewed revision: a fast-forward, a merge, or a
    /// completed record for a result the parent already contained.
    Integrated,
    /// The parent history already carried the child's patch, which was matched
    /// by patch identity rather than by commit identity.
    AlreadyPresent,
    /// The manager landed the change itself and named the revision and the
    /// evidence that carry it.
    Replay,
}

/// How a declared file scope reads its paths.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectChildScopeMode {
    /// The listed paths are the allowlist a commit may touch.
    Paths,
    /// The listed paths are the only paths a commit may not touch.
    OutOfScope,
}

/// The files a delegated code child's commit may touch, as its delegation
/// declared them.
///
/// A path is repository-relative and matches itself and everything below it, so
/// `crates/loom-vcs/src` covers `crates/loom-vcs/src/lib.rs` while
/// `crates/loom-vcs/src/lib.rs` covers no other file. A scope declared in
/// [`ProjectChildScopeMode::Paths`] is an allowlist; one declared in
/// [`ProjectChildScopeMode::OutOfScope`] is the inverse and names what the child
/// must leave alone.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectChildScope {
    pub mode: ProjectChildScopeMode,
    pub paths: Vec<String>,
}

/// Normalises one repository-relative scope path.
///
/// A leading `./` and a trailing `/` are dropped, so a caller can name a
/// directory either way. An empty path, an absolute path, and a path that
/// climbs out of the repository are refused rather than silently reinterpreted,
/// because a scope that matches nothing would report every path out of scope.
pub fn normalize_project_child_scope_path(path: &str) -> crate::Result<String> {
    let path = path.trim();
    let path = path.strip_prefix("./").unwrap_or(path);
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        return Err(crate::LoomError::invalid_request(
            "a declared scope path must not be empty",
        ));
    }
    if path.len() > MAX_PROJECT_CHILD_SCOPE_PATH_BYTES {
        return Err(crate::LoomError::invalid_request(format!(
            "a declared scope path must be at most {MAX_PROJECT_CHILD_SCOPE_PATH_BYTES} bytes"
        )));
    }
    if path.starts_with('/') || path.contains('\\') || path.contains('\0') {
        return Err(crate::LoomError::invalid_request(format!(
            "the declared scope path '{path}' must be repository-relative"
        )));
    }
    if path
        .split('/')
        .any(|component| component == ".." || component.is_empty())
    {
        return Err(crate::LoomError::invalid_request(format!(
            "the declared scope path '{path}' must stay inside the repository"
        )));
    }
    Ok(path.to_owned())
}

impl ProjectChildScope {
    /// Normalises a declaration, refusing duplicates and an empty path list.
    ///
    /// A declaration that named no path would put every changed path out of
    /// scope, which is never what a manager means, so it is refused instead of
    /// read as a stricter scope than intended.
    pub fn new(mode: ProjectChildScopeMode, paths: &[String]) -> crate::Result<Self> {
        if paths.is_empty() || paths.len() > MAX_PROJECT_CHILD_SCOPE_PATHS {
            return Err(crate::LoomError::invalid_request(format!(
                "a declared file scope names 1 to {MAX_PROJECT_CHILD_SCOPE_PATHS} paths"
            )));
        }
        let mut normalized = Vec::with_capacity(paths.len());
        for path in paths {
            let path = normalize_project_child_scope_path(path)?;
            if normalized.contains(&path) {
                return Err(crate::LoomError::invalid_request(format!(
                    "the declared scope path '{path}' is repeated"
                )));
            }
            normalized.push(path);
        }
        Ok(Self {
            mode,
            paths: normalized,
        })
    }

    /// Whether one changed path falls inside this declaration.
    pub fn covers(&self, path: &str) -> bool {
        let listed = self.paths.iter().any(|declared| {
            path == declared
                || path
                    .strip_prefix(declared.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        });
        match self.mode {
            ProjectChildScopeMode::Paths => listed,
            ProjectChildScopeMode::OutOfScope => !listed,
        }
    }

    /// The changed paths this declaration leaves out of scope, in the order the
    /// review reported them.
    pub fn out_of_scope<'a>(
        &'a self,
        stats: &'a [crate::GitRevisionFileStat],
    ) -> impl Iterator<Item = &'a crate::GitRevisionFileStat> {
        stats.iter().filter(move |stat| !self.covers(&stat.path))
    }
}

/// The override a manager recorded to land a commit its declaration leaves out
/// of scope.
///
/// The accepted paths and the reason are stored together, so the record states
/// what was landed outside the declaration and why instead of leaving the
/// deviation implicit.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectWorktreeScopeOverride {
    pub reason: String,
    pub paths: Vec<String>,
    pub recorded_at: Timestamp,
}

/// The revision a checkout was created from, together with how far that
/// revision is from the default branch of the remote that was fetched for it.
///
/// A record exists only for a refresh that ran for the revision it names, so a
/// stale `refs/remotes/origin/*` ref is never presented as the current upstream
/// tip. When that fetch fails the record states the failure and leaves the
/// drift unknown instead of reporting a cached answer.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum ProjectWorktreeBaseRefresh {
    /// The fetch performed for this record resolved the default-branch tip.
    Fetched {
        /// Remote that was fetched.
        remote: String,
        /// Revision the refresh measured against.
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
        /// Revision the refresh measured against.
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
    /// The files this child's commit may touch, as its delegation declared
    /// them. `None` means the delegation declared no scope.
    #[serde(default)]
    pub scope: Option<ProjectChildScope>,
    /// The override recorded when a commit outside the declared scope was
    /// integrated, naming the accepted paths and the manager's reason.
    #[serde(default)]
    pub scope_override: Option<ProjectWorktreeScopeOverride>,
    pub result_revision: Option<String>,
    pub integrated_revision: Option<String>,
    /// How the reviewed result reached the parent history. `None` only for
    /// records written before landings were recorded.
    #[serde(default)]
    pub landed_by: Option<ProjectWorktreeLandedBy>,
    /// The manager's note recorded by the explicit `mark_landed` action, so a
    /// landing Loom did not perform is never silent.
    #[serde(default)]
    pub landed_evidence: Option<String>,
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
            parent_project_id: None,
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
        assert_eq!(decoded.parent_project_id, None);
        // A snapshot written before sub projects existed decodes as a project
        // that was not branched out of another one.
        let mut legacy = serde_json::to_value(&snapshot).unwrap();
        legacy.as_object_mut().unwrap().remove("parent_project_id");
        let legacy: ProjectSnapshot = serde_json::from_value(legacy).unwrap();
        assert_eq!(legacy.parent_project_id, None);
    }

    #[test]
    fn delegated_task_and_durable_message_contracts_round_trip() {
        use super::{
            AgentMessageDraft, AgentMessageKind, AgentMessageRecord, DelegatedTaskRecord,
            DelegatedTaskSpec, DelegatedTaskStatus, ProjectAgentPermissions, ProjectChildScope,
            ProjectChildScopeMode, TaskContextReference,
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
            scope: Some(ProjectChildScope {
                mode: ProjectChildScopeMode::Paths,
                paths: vec!["crates/loom-vcs/src".into()],
            }),
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
        assert_eq!(
            legacy_spec.scope, None,
            "a task written before scopes existed declares none"
        );
    }

    #[test]
    fn worktree_base_refresh_summarizes_drift_from_a_fetch() {
        use super::{
            ProjectChildScope, ProjectChildScopeMode, ProjectWorktreeBaseRefresh,
            ProjectWorktreeLandedBy, ProjectWorktreeRecord, ProjectWorktreeStatus,
        };
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
            scope: Some(ProjectChildScope {
                mode: ProjectChildScopeMode::OutOfScope,
                paths: vec!["docs/".into()],
            }),
            scope_override: None,
            result_revision: None,
            integrated_revision: None,
            landed_by: Some(ProjectWorktreeLandedBy::Replay),
            landed_evidence: Some("replayed onto the parent tip".into()),
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
        assert_eq!(encoded["landed_by"], "replay");
        assert_eq!(
            serde_json::from_value::<ProjectWorktreeRecord>(encoded).unwrap(),
            record
        );

        // A record written before a landing was recorded decodes with no
        // landing rather than failing to load.
        let mut legacy = serde_json::to_value(&record).unwrap();
        legacy.as_object_mut().unwrap().remove("base_refresh");
        legacy.as_object_mut().unwrap().remove("landed_by");
        legacy.as_object_mut().unwrap().remove("landed_evidence");
        legacy.as_object_mut().unwrap().remove("scope");
        legacy.as_object_mut().unwrap().remove("scope_override");
        let legacy: ProjectWorktreeRecord = serde_json::from_value(legacy).unwrap();
        assert_eq!(legacy.base_refresh, None);
        assert_eq!(legacy.landed_by, None);
        assert_eq!(legacy.landed_evidence, None);
        assert_eq!(
            legacy.scope, None,
            "a worktree written before scopes declares none"
        );
        assert_eq!(legacy.scope_override, None);
    }

    #[test]
    fn child_scope_covers_exact_paths_and_directories_and_reads_out_of_scope_as_the_inverse() {
        use super::{
            MAX_PROJECT_CHILD_SCOPE_PATHS, ProjectChildScope, ProjectChildScopeMode,
            normalize_project_child_scope_path,
        };
        use crate::{GitRevisionChangeKind, GitRevisionFileStat};

        let allowlist = ProjectChildScope::new(
            ProjectChildScopeMode::Paths,
            &["./crates/loom-vcs/src/".to_owned(), "Cargo.toml".to_owned()],
        )
        .unwrap();
        assert_eq!(
            allowlist.paths,
            vec!["crates/loom-vcs/src".to_owned(), "Cargo.toml".to_owned()]
        );
        assert!(allowlist.covers("crates/loom-vcs/src/lib.rs"));
        assert!(allowlist.covers("crates/loom-vcs/src"));
        assert!(allowlist.covers("Cargo.toml"));
        assert!(
            !allowlist.covers("crates/loom-vcs/srcs/lib.rs"),
            "a directory prefix matches only a whole path component"
        );
        assert!(!allowlist.covers("crates/loom-server/src/lib.rs"));

        let stats = vec![
            GitRevisionFileStat {
                path: "crates/loom-vcs/src/lib.rs".to_owned(),
                original_path: None,
                change: GitRevisionChangeKind::Modified,
                additions: 4,
                deletions: 1,
                binary: false,
            },
            GitRevisionFileStat {
                path: "crates/loom-server/src/new.rs".to_owned(),
                original_path: None,
                change: GitRevisionChangeKind::New,
                additions: 9,
                deletions: 0,
                binary: false,
            },
        ];
        assert_eq!(
            allowlist
                .out_of_scope(&stats)
                .map(|stat| stat.path.as_str())
                .collect::<Vec<_>>(),
            vec!["crates/loom-server/src/new.rs"]
        );

        // The inverse declaration names what the child must leave alone, so the
        // same commit is reported for the opposite reason.
        let deny = ProjectChildScope::new(
            ProjectChildScopeMode::OutOfScope,
            &["crates/loom-server/src/new.rs".to_owned()],
        )
        .unwrap();
        assert!(deny.covers("crates/loom-vcs/src/lib.rs"));
        assert!(!deny.covers("crates/loom-server/src/new.rs"));
        assert_eq!(
            deny.out_of_scope(&stats)
                .map(|stat| stat.path.as_str())
                .collect::<Vec<_>>(),
            vec!["crates/loom-server/src/new.rs"]
        );

        // A declaration that names nothing, repeats a path, or escapes the
        // repository is refused instead of matching nothing.
        assert!(ProjectChildScope::new(ProjectChildScopeMode::Paths, &[]).is_err());
        assert!(
            ProjectChildScope::new(
                ProjectChildScopeMode::Paths,
                &["src/lib.rs".to_owned(), "./src/lib.rs".to_owned()],
            )
            .is_err()
        );
        let too_many = (0..=MAX_PROJECT_CHILD_SCOPE_PATHS)
            .map(|index| format!("src/{index}.rs"))
            .collect::<Vec<_>>();
        assert!(ProjectChildScope::new(ProjectChildScopeMode::Paths, &too_many).is_err());
        for refused in [
            "/etc/passwd",
            "../outside",
            "src/../lib.rs",
            "src//lib.rs",
            "",
        ] {
            assert!(
                normalize_project_child_scope_path(refused).is_err(),
                "'{refused}' must be refused"
            );
        }
    }
}
