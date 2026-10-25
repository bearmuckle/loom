//! The operator verbs: worker node health, archive retention, the session and
//! project lifecycle, and deletion.
//!
//! `node`, `retention get`, and the worktree inspection verbs are reads.
//! `retention set`, the archive, delete, integration, and worktree-cleanup verbs
//! are operator-level mutations that the token's capabilities already gate. The
//! policy is worker-wide, so `retention set` reports it as stored and says the
//! next sweep applies it rather than claiming an immediate deletion. The
//! lifecycle verbs read the project snapshot before they mutate anything,
//! because an id alone does not say whether it names a project root whose
//! operation reaches other sessions.

use std::io::Write;

use loom_core::{
    AgentSessionId, AgentSessionState, GitDiff, GitDiffLineKind, GitRepositoryStatus,
    ProjectSnapshot, ProjectWorktreeCleanupDisposition, ProjectWorktreeRecord,
    ProjectWorktreeStatus, RequestId, TaskId,
};
use loom_protocol::{
    ArchiveRetentionPolicy, ClientRequest, ControlRequest, ControlResponse, ProjectRequest,
    ProjectResponse, ServerResponse, SessionRequest, SessionResponse,
};
use serde_json::{Value, json};

use crate::cli::args::{Invocation, RetentionSetArgs, WorktreeDisposition};
use crate::cli::client::Client;
use crate::cli::text::escape;

use super::supervise::file_status_name;
use super::{Caller, Failure, Outcome, capability_names, emit, handshake};

/// `node`: the worker's id, name, and the health its status carries.
pub(super) async fn node<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let (request_id, response) = caller
        .call(ClientRequest::Control(ControlRequest::GetWorkerNodeStatus))
        .await?;
    let node = match response {
        ServerResponse::Control(ControlResponse::WorkerNodeStatus(node)) => node,
        _ => return Err(Failure::unexpected(request_id, "worker node status")),
    };
    if !invocation.globals.json {
        let capabilities = capability_names(&node.capabilities);
        emit(out, &format!("node_id {}\n", node.node_id))?;
        emit(out, &format!("name {}\n", node.name))?;
        emit(out, &format!("online {}\n", node.online))?;
        emit(
            out,
            &format!(
                "capabilities {} {}\n",
                capabilities.len(),
                capabilities.join(",")
            ),
        )?;
        emit(out, &format!("cpu_count {}\n", node.resources.cpu_count))?;
        emit(
            out,
            &format!(
                "cpu_usage_percent {}\n",
                optional(node.resources.cpu_usage_percent)
            ),
        )?;
        emit(
            out,
            &format!(
                "memory_usage_percent {}\n",
                optional(node.resources.memory_usage_percent)
            ),
        )?;
        emit(
            out,
            &format!(
                "memory_total_bytes {}\n",
                optional(node.resources.memory_total_bytes)
            ),
        )?;
        emit(
            out,
            &format!(
                "memory_available_bytes {}\n",
                optional(node.resources.memory_available_bytes)
            ),
        )?;
        emit(
            out,
            &format!(
                "disk_total_bytes {}\n",
                optional(node.resources.disk_total_bytes)
            ),
        )?;
        emit(
            out,
            &format!(
                "disk_available_bytes {}\n",
                optional(node.resources.disk_available_bytes)
            ),
        )?;
    }
    Ok(Outcome::new(
        serde_json::to_value(&node).unwrap_or(Value::Null),
        caller.primary,
    ))
}

/// `retention get`: the policy the worker stored.
pub(super) async fn retention_get<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let (request_id, response) = caller
        .call(ClientRequest::Control(
            ControlRequest::GetArchiveRetentionPolicy,
        ))
        .await?;
    let policy = match response {
        ServerResponse::Control(ControlResponse::ArchiveRetentionPolicy(policy)) => policy,
        _ => return Err(Failure::unexpected(request_id, "archive retention policy")),
    };
    if !invocation.globals.json {
        emit_policy(out, &policy)?;
    }
    Ok(Outcome::new(policy_value(&policy), caller.primary))
}

/// `retention set`: merge the given fields into the stored policy and report
/// what the worker now holds.
pub(super) async fn retention_set<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    args: &RetentionSetArgs,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let (request_id, response) = caller
        .call(ClientRequest::Control(
            ControlRequest::GetArchiveRetentionPolicy,
        ))
        .await?;
    let mut policy = match response {
        ServerResponse::Control(ControlResponse::ArchiveRetentionPolicy(policy)) => policy,
        _ => return Err(Failure::unexpected(request_id, "archive retention policy")),
    };
    if let Some(window_ms) = args.window_ms {
        policy.retention_ms = window_ms;
    }
    if let Some(force_discard_worktrees) = args.force_discard_worktrees {
        policy.force_discard_worktrees = force_discard_worktrees;
    }
    if let Some(sub_agent_ms) = args.sub_agent_ms {
        policy.sub_agent_retention_ms = sub_agent_ms;
    }
    let (request_id, response) = caller
        .call(ClientRequest::Control(
            ControlRequest::SetArchiveRetentionPolicy { policy },
        ))
        .await?;
    let stored = match response {
        ServerResponse::Control(ControlResponse::ArchiveRetentionPolicy(policy)) => policy,
        _ => return Err(Failure::unexpected(request_id, "archive retention policy")),
    };
    if !invocation.globals.json {
        emit_policy(out, &stored)?;
        emit(out, "stored for this worker; the next sweep applies it\n")?;
    }
    let mut data = policy_value(&stored);
    data["applies_from"] = json!("next_sweep");
    Ok(Outcome::new(data, caller.primary))
}

/// `session archive` and `project archive`: archive one session, and the live
/// descendants a project root cascades to.
///
/// The snapshot is read first because the worker's cascade depends on the
/// target being a project root, which an id alone does not say. `project
/// archive` refuses an id that is not such a root, so the project spelling can
/// never quietly archive a single unrelated session. Archiving keeps every
/// checkout, so the command reports the worktrees it retained.
pub(super) async fn archive<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    session_id: AgentSessionId,
    require_project_root: bool,
    yes: bool,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let project: ProjectSnapshot = load_project(&mut caller, session_id).await?;
    let scope = ArchiveScope::of(&project, session_id);
    if require_project_root && !scope.cascades() {
        return Err(Failure::Usage(format!(
            "'{session_id}' is not a project root with another member or a delegated task; use \
             'session archive <session-id>'"
        )));
    }
    let targets = scope.targets();
    // The project spelling names the tree itself, so only `session archive`
    // asks for a confirmation that the id reaches more than one session.
    if !require_project_root && targets.len() > 1 && !yes {
        return Err(Failure::Usage(format!(
            "archive would reach {} sessions because this project root cascades; pass --yes to \
             confirm",
            targets.len()
        )));
    }
    let (request_id, response) = caller
        .call(ClientRequest::Session(
            SessionRequest::ArchiveAgentSession { session_id },
        ))
        .await?;
    match response {
        ServerResponse::Session(SessionResponse::AgentSessionArchived(session))
            if session.id == session_id => {}
        _ => return Err(Failure::unexpected(request_id, "session archive")),
    }
    let worktrees = scope.worktrees();
    if !invocation.globals.json {
        for target in &targets {
            emit(out, &format!("archived {target}\n"))?;
        }
        if worktrees.is_empty() {
            emit(out, "retained 0 worktrees\n")?;
        } else {
            for worktree in &worktrees {
                emit(
                    out,
                    &format!(
                        "retained worktree {} {}\n",
                        worktree.worktree_name, worktree.relative_path
                    ),
                )?;
            }
        }
    }
    let data = json!({
        "session_id": session_id.to_string(),
        "archived_sessions": targets.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "retained_worktrees": worktrees
            .iter()
            .map(|worktree| json!({
                "name": worktree.worktree_name,
                "path": worktree.relative_path,
            }))
            .collect::<Vec<_>>(),
        "cascade": targets.len() > 1,
    });
    Ok(Outcome::new(data, caller.primary))
}

/// `project worktree cleanup`: keep or remove the checkout recorded for one
/// delegated task.
///
/// The project id and the manager session come from the worktree record in the
/// snapshot, so the caller names the task and the worker's own record decides
/// the rest. The disposition is explicit: `--keep` retains the checkout, and
/// `--remove` refuses one that holds changes while `--discard` throws them
/// away. The worker also refuses a task that is not terminal, and this command
/// never retries that refusal.
pub(super) async fn worktree_cleanup<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    session_id: AgentSessionId,
    task_id: TaskId,
    disposition: WorktreeDisposition,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let project: ProjectSnapshot = load_project(&mut caller, session_id).await?;
    let worktree = worktree_record(&project, task_id)?;
    let (request_id, response) = caller
        .call(ClientRequest::Project(
            ProjectRequest::CleanupProjectChildWorktree {
                project_id: worktree.project_id,
                manager_session_id: worktree.parent_session_id,
                task_id,
                disposition: cleanup_disposition(disposition),
            },
        ))
        .await?;
    let updated = match response {
        ServerResponse::Project(ProjectResponse::ProjectChildWorktreeUpdated(worktree)) => {
            *worktree
        }
        _ => {
            return Err(Failure::unexpected(
                request_id,
                "project child worktree cleanup",
            ));
        }
    };
    if !invocation.globals.json {
        match updated.status {
            ProjectWorktreeStatus::Retained => emit(
                out,
                &format!(
                    "retained worktree {} {}\n",
                    updated.worktree_name, updated.relative_path
                ),
            )?,
            ProjectWorktreeStatus::Removed => emit(
                out,
                &format!(
                    "removed worktree {} {}\n",
                    updated.worktree_name, updated.relative_path
                ),
            )?,
            status => emit(
                out,
                &format!(
                    "worktree {} {} {}\n",
                    updated.worktree_name,
                    updated.relative_path,
                    worktree_status_name(status)
                ),
            )?,
        }
        // A forced removal records what it threw away, which is the only place
        // that loss is stated, so it is never dropped from the text output.
        if let Some(note) = &updated.error {
            emit(out, &format!("note {note}\n"))?;
        }
    }
    let data = json!({
        "session_id": session_id.to_string(),
        "task_id": task_id.to_string(),
        "disposition": disposition.name(),
        "worktree": &updated,
    });
    Ok(Outcome::new(data, caller.primary))
}

/// `project worktrees`: every checkout the project's snapshot records.
///
/// The snapshot is the whole answer, so this is a read: it lists the durable
/// records in snapshot order and never opens a checkout.
pub(super) async fn worktrees<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    session_id: AgentSessionId,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let project: ProjectSnapshot = load_project(&mut caller, session_id).await?;
    if !invocation.globals.json {
        emit(out, &format!("project {}\n", project.project_id))?;
        emit(out, &format!("worktrees {}\n", project.worktrees.len()))?;
        for worktree in &project.worktrees {
            emit(
                out,
                &format!(
                    "worktree {} {} {} {}\n",
                    worktree.task_id,
                    worktree_status_name(worktree.status),
                    worktree.worktree_name,
                    worktree.relative_path
                ),
            )?;
        }
    }
    let data = json!({
        "session_id": session_id.to_string(),
        "project_id": project.project_id.to_string(),
        "root_session_id": project.root_session_id.to_string(),
        "worktrees": project.worktrees,
    });
    Ok(Outcome::new(data, caller.primary))
}

/// `project worktree get`: the durable record one delegated task owns.
///
/// Only the snapshot is read: the record is the worker's own state, so the verb
/// reports it without opening the checkout or reviewing it.
pub(super) async fn worktree_get<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    session_id: AgentSessionId,
    task_id: TaskId,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let project: ProjectSnapshot = load_project(&mut caller, session_id).await?;
    let worktree = worktree_record(&project, task_id)?;
    if !invocation.globals.json {
        emit(
            out,
            &format!(
                "worktree {} {}\n",
                worktree.worktree_name, worktree.relative_path
            ),
        )?;
        emit(out, &format!("task {}\n", worktree.task_id))?;
        emit(out, &format!("project {}\n", worktree.project_id))?;
        emit(out, &format!("owner {}\n", worktree.parent_session_id))?;
        emit(out, &format!("child {}\n", worktree.child_session_id))?;
        emit(
            out,
            &format!("parent-repository {}\n", worktree.parent_repository_id),
        )?;
        emit(
            out,
            &format!("child-repository {}\n", worktree.child_repository_id),
        )?;
        emit(out, &format!("branch {}\n", worktree.branch_name))?;
        emit(
            out,
            &format!("parent-revision {}\n", worktree.base_revision),
        )?;
        emit(
            out,
            &format!(
                "child-revision {}\n",
                or_dash(worktree.result_revision.as_deref())
            ),
        )?;
        emit(
            out,
            &format!(
                "integrated-revision {}\n",
                or_dash(worktree.integrated_revision.as_deref())
            ),
        )?;
        emit(
            out,
            &format!("status {}\n", worktree_status_name(worktree.status)),
        )?;
        for path in &worktree.conflict_paths {
            emit(out, &format!("conflict {}\n", escape(path)))?;
        }
        emit(
            out,
            &format!(
                "cleanup {}\n",
                or_dash(worktree.cleanup_disposition.map(cleanup_disposition_name))
            ),
        )?;
        if let Some(note) = &worktree.error {
            emit(out, &format!("note {}\n", escape(note)))?;
        }
    }
    let data = json!({
        "session_id": session_id.to_string(),
        "project_id": worktree.project_id.to_string(),
        "task_id": task_id.to_string(),
        "worktree": worktree,
    });
    Ok(Outcome::new(data, caller.primary))
}

/// `project worktree diff`: the worker's bounded review of one checkout.
///
/// The review is the worker's own answer, so this verb never re-bounds the
/// diff and never runs Git itself. Every printed line is escaped, so a hunk can
/// never span two physical lines.
pub(super) async fn worktree_diff<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    session_id: AgentSessionId,
    task_id: TaskId,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let project: ProjectSnapshot = load_project(&mut caller, session_id).await?;
    let record = worktree_record(&project, task_id)?;
    let (request_id, response) = caller
        .call(ClientRequest::Project(
            ProjectRequest::GetProjectChildReview {
                project_id: record.project_id,
                manager_session_id: record.parent_session_id,
                task_id,
            },
        ))
        .await?;
    let (worktree, status, diff) = match response {
        ServerResponse::Project(ProjectResponse::ProjectChildReview {
            worktree,
            status,
            diff,
        }) => (*worktree, status, diff),
        _ => return Err(Failure::unexpected(request_id, "project child review")),
    };
    let counts = diffstat(&diff, &status);
    let changed = changed_paths(&status);
    if !invocation.globals.json {
        emit(
            out,
            &format!(
                "worktree {} {}\n",
                worktree.worktree_name, worktree.relative_path
            ),
        )?;
        emit(
            out,
            &format!("branch {}\n", or_dash(status.branch.as_deref())),
        )?;
        emit(out, &format!("head {}\n", or_dash(status.head.as_deref())))?;
        emit(
            out,
            &format!("parent-revision {}\n", worktree.base_revision),
        )?;
        emit(
            out,
            &format!("status {}\n", worktree_status_name(worktree.status)),
        )?;
        emit(
            out,
            &format!(
                "diffstat {} {} {}\n",
                counts.files, counts.added, counts.removed
            ),
        )?;
        for changed in &changed {
            emit(
                out,
                &format!(
                    "changed {} {}\n",
                    changed["status"].as_str().unwrap_or("unknown"),
                    escape(changed["path"].as_str().unwrap_or("-"))
                ),
            )?;
        }
        if diff.binary {
            emit(out, "diff binary\n")?;
        }
        if diff.truncated {
            emit(out, "diff truncated\n")?;
        }
        // A bounded diff that held no hunks says so, so an empty report cannot
        // be mistaken for an unchanged checkout that Git failed to describe.
        if diff.hunks.is_empty() && !diff.binary {
            emit(out, "diff none\n")?;
        }
        for line in diff.hunks.iter().flat_map(|hunk| &hunk.lines) {
            emit(
                out,
                &format!(
                    "diff {} {}\n",
                    diff_line_name(line.kind),
                    escape(&line.content)
                ),
            )?;
        }
    }
    let data = json!({
        "session_id": session_id.to_string(),
        "project_id": project.project_id.to_string(),
        "task_id": task_id.to_string(),
        "worktree": worktree,
        "status": status,
        "diff": diff,
        "diffstat": {
            "files": counts.files,
            "added": counts.added,
            "removed": counts.removed,
        },
        "changed_paths": changed,
    });
    Ok(Outcome::new(data, caller.primary))
}

/// `project worktree integrate`: fast-forward the parent with the task's result.
///
/// The expected parent revision is the flag when it is given and the record's
/// base revision otherwise, because that is what the checkout and the review
/// were created from. The worker keeps the ancestor check and answers a
/// conflict when the parent moved; this verb sends exactly one request, so a
/// conflict is reported as it is and the checkout is left untouched.
pub(super) async fn worktree_integrate<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    session_id: AgentSessionId,
    task_id: TaskId,
    expected_parent_revision: Option<&str>,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let project: ProjectSnapshot = load_project(&mut caller, session_id).await?;
    let record = worktree_record(&project, task_id)?;
    let expected = expected_parent_revision.unwrap_or(&record.base_revision);
    let (request_id, response) = caller
        .call(ClientRequest::Project(
            ProjectRequest::IntegrateProjectChild {
                project_id: record.project_id,
                manager_session_id: record.parent_session_id,
                task_id,
                expected_parent_revision: expected.to_owned(),
            },
        ))
        .await?;
    let updated = match response {
        ServerResponse::Project(ProjectResponse::ProjectChildWorktreeUpdated(worktree)) => {
            *worktree
        }
        _ => {
            return Err(Failure::unexpected(request_id, "project child integration"));
        }
    };
    if !invocation.globals.json {
        emit(
            out,
            &format!(
                "integrated worktree {} {}\n",
                updated.worktree_name, updated.relative_path
            ),
        )?;
        emit(
            out,
            &format!(
                "child-revision {}\n",
                or_dash(updated.result_revision.as_deref())
            ),
        )?;
        emit(
            out,
            &format!(
                "integrated-revision {}\n",
                or_dash(updated.integrated_revision.as_deref())
            ),
        )?;
    }
    let data = json!({
        "session_id": session_id.to_string(),
        "project_id": updated.project_id.to_string(),
        "task_id": task_id.to_string(),
        "expected_parent_revision": expected,
        "worktree": &updated,
    });
    Ok(Outcome::new(data, caller.primary))
}

/// The worktree record one delegated task owns, which every worktree verb
/// resolves from the snapshot the worker reports.
fn worktree_record(
    project: &ProjectSnapshot,
    task_id: TaskId,
) -> Result<&ProjectWorktreeRecord, Failure> {
    project
        .worktrees
        .iter()
        .find(|worktree| worktree.task_id == task_id)
        .ok_or_else(|| {
            Failure::Usage(format!(
                "no project worktree is recorded for task {task_id}"
            ))
        })
}

/// The `-` placeholder the worktree text uses for an absent value.
fn or_dash(value: Option<&str>) -> &str {
    value.unwrap_or("-")
}

/// The protocol's snake_case spelling of a cleanup disposition, stated
/// explicitly so a wire rename cannot change the text contract.
fn cleanup_disposition_name(disposition: ProjectWorktreeCleanupDisposition) -> &'static str {
    match disposition {
        ProjectWorktreeCleanupDisposition::Retain => "retain",
        ProjectWorktreeCleanupDisposition::RemoveClean => "remove_clean",
        ProjectWorktreeCleanupDisposition::DiscardChanges => "discard_changes",
    }
}

/// One line's kind word in the `diff` projection.
fn diff_line_name(kind: GitDiffLineKind) -> &'static str {
    match kind {
        GitDiffLineKind::Context => "context",
        GitDiffLineKind::Added => "added",
        GitDiffLineKind::Removed => "removed",
    }
}

/// The file count and hunk-line counts of one review, which `result` reports the
/// same way.
struct DiffCounts {
    files: usize,
    added: usize,
    removed: usize,
}

fn diffstat(diff: &GitDiff, status: &GitRepositoryStatus) -> DiffCounts {
    let lines = || diff.hunks.iter().flat_map(|hunk| &hunk.lines);
    DiffCounts {
        files: status.files.len(),
        added: lines()
            .filter(|line| line.kind == GitDiffLineKind::Added)
            .count(),
        removed: lines()
            .filter(|line| line.kind == GitDiffLineKind::Removed)
            .count(),
    }
}

/// The changed paths of one review, with the status word `result` already uses.
fn changed_paths(status: &GitRepositoryStatus) -> Vec<Value> {
    status
        .files
        .iter()
        .map(|file| {
            json!({
                "path": file.path,
                "status": file_status_name(file.worktree),
                "conflicted": file.conflicted,
            })
        })
        .collect()
}

/// The sessions one archive reaches, resolved from the snapshot read before the
/// request.
///
/// A project root cascades when it owns a live member or a delegated task, which
/// is the worker's own rule for archiving descendants; an already archived
/// member is skipped because the worker leaves it untouched rather than
/// archiving it twice.
struct ArchiveScope<'a> {
    project: &'a ProjectSnapshot,
    session_id: AgentSessionId,
    descendants: Vec<AgentSessionId>,
}

impl<'a> ArchiveScope<'a> {
    fn of(project: &'a ProjectSnapshot, session_id: AgentSessionId) -> Self {
        let descendants = if project.root_session_id == session_id {
            project
                .agents
                .iter()
                .filter(|agent| {
                    agent.session_id != session_id && agent.state != AgentSessionState::Archived
                })
                .map(|agent| agent.session_id)
                .collect()
        } else {
            Vec::new()
        };
        Self {
            project,
            session_id,
            descendants,
        }
    }

    /// Whether the worker archives more than the named session.
    fn cascades(&self) -> bool {
        !self.descendants.is_empty() || !self.project.tasks.is_empty()
    }

    /// The named session first, then each descendant the worker also archives.
    fn targets(&self) -> Vec<AgentSessionId> {
        std::iter::once(self.session_id)
            .chain(self.descendants.iter().copied())
            .collect()
    }

    /// The checkouts whose parent or child session is in the archived set, which
    /// archiving always keeps.
    fn worktrees(&self) -> Vec<&'a ProjectWorktreeRecord> {
        let targets = self.targets();
        self.project
            .worktrees
            .iter()
            .filter(|worktree| {
                targets.contains(&worktree.parent_session_id)
                    || targets.contains(&worktree.child_session_id)
            })
            .collect()
    }
}

/// The project snapshot for one session, which every lifecycle verb reads
/// before it mutates anything.
async fn load_project<C: Client>(
    caller: &mut Caller<'_, C>,
    session_id: AgentSessionId,
) -> Result<ProjectSnapshot, Failure> {
    let (request_id, response) = caller
        .call(ClientRequest::Project(
            ProjectRequest::GetProjectSnapshotForSession { session_id },
        ))
        .await?;
    match response {
        ServerResponse::Project(ProjectResponse::ProjectSnapshot(project)) => Ok(project),
        _ => Err(Failure::unexpected(request_id, "project snapshot")),
    }
}

fn cleanup_disposition(disposition: WorktreeDisposition) -> ProjectWorktreeCleanupDisposition {
    match disposition {
        WorktreeDisposition::Keep => ProjectWorktreeCleanupDisposition::Retain,
        WorktreeDisposition::Remove => ProjectWorktreeCleanupDisposition::RemoveClean,
        WorktreeDisposition::Discard => ProjectWorktreeCleanupDisposition::DiscardChanges,
    }
}

/// The protocol's snake_case spelling of a worktree status, stated explicitly so
/// a wire rename cannot change the text contract.
fn worktree_status_name(status: ProjectWorktreeStatus) -> &'static str {
    match status {
        ProjectWorktreeStatus::Creating => "creating",
        ProjectWorktreeStatus::Ready => "ready",
        ProjectWorktreeStatus::Stale => "stale",
        ProjectWorktreeStatus::Conflict => "conflict",
        ProjectWorktreeStatus::Integrating => "integrating",
        ProjectWorktreeStatus::Integrated => "integrated",
        ProjectWorktreeStatus::RecoveryRequired => "recovery_required",
        ProjectWorktreeStatus::CleanupPending => "cleanup_pending",
        ProjectWorktreeStatus::Retained => "retained",
        ProjectWorktreeStatus::Removed => "removed",
    }
}

/// `delete`: remove one archived session, or a project root with its tree.
///
/// The project snapshot is read first so the command can require `--yes` for a
/// cascade and report the worktrees a force deletion discards before it sends
/// the irreversible request. The pre-read keeps a fresh id: it is a lookup, not
/// the mutation `--request-id` keys. A non-archived session is still refused by
/// the worker, which is the authority on which sessions may be deleted.
/// `project delete` additionally requires the id to name a project root, so a
/// mistyped id is refused instead of quietly deleting one session.
pub(super) async fn delete<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    session_id: AgentSessionId,
    force: bool,
    yes: bool,
    require_project_root: bool,
    request_id: Option<RequestId>,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    // The caller's key, when it named one, is the deletion's id: a repeated
    // command is answered from the worker's cache with the same payload, even
    // though the session no longer exists to delete again. The pre-read below
    // keeps a fresh id.
    let key = request_id.unwrap_or_default();
    let project: ProjectSnapshot = load_project(&mut caller, session_id).await?;
    // Only a project root cascades, and the worker applies the same rule: a
    // root owns another member or at least one delegated task.
    let cascades = project.root_session_id == session_id
        && (project.agents.len() > 1 || !project.tasks.is_empty());
    if require_project_root && !cascades {
        return Err(Failure::Usage(format!(
            "'{session_id}' is not a project root with another member or a delegated task; use \
             'session delete <session-id>'"
        )));
    }
    let mut targets = vec![session_id];
    if cascades {
        for agent in &project.agents {
            if agent.session_id != session_id {
                targets.push(agent.session_id);
            }
        }
    }
    if targets.len() > 1 && !yes {
        return Err(Failure::Usage(format!(
            "delete would remove {} sessions because this project root cascades; pass --yes to \
             confirm",
            targets.len()
        )));
    }
    let worktrees = if force {
        project
            .worktrees
            .iter()
            .filter(|worktree| {
                targets.contains(&worktree.parent_session_id)
                    || targets.contains(&worktree.child_session_id)
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    // The caller's key, when it named one, is the deletion's id: a repeated
    // command is answered from the worker's cache with the same payload, even
    // though the session no longer exists to delete again.
    let (request_id, response) = caller
        .call_with_id(
            key,
            ClientRequest::Session(SessionRequest::DeleteAgentSession { session_id, force }),
        )
        .await?;
    match response {
        ServerResponse::Session(SessionResponse::AgentSessionDeleted {
            session_id: deleted,
        }) if deleted == session_id => {}
        _ => return Err(Failure::unexpected(request_id, "session deletion")),
    }
    if !invocation.globals.json {
        for target in &targets {
            emit(out, &format!("deleted {target}\n"))?;
        }
        if force {
            if worktrees.is_empty() {
                emit(out, "discarded 0 worktrees\n")?;
            } else {
                for worktree in &worktrees {
                    emit(
                        out,
                        &format!(
                            "discarded worktree {} {}\n",
                            worktree.worktree_name, worktree.relative_path
                        ),
                    )?;
                }
            }
        }
    }
    let data = json!({
        "session_id": session_id.to_string(),
        "deleted_sessions": targets.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "force": force,
        "discarded_worktrees": worktrees
            .iter()
            .map(|worktree| json!({
                "name": worktree.worktree_name,
                "path": worktree.relative_path,
            }))
            .collect::<Vec<_>>(),
    });
    Ok(Outcome::new(data, caller.primary).with_replayed(caller.replayed()))
}

/// A compact duration for the text projection: `off` when the window is
/// disabled, otherwise the largest whole unit the parser accepts, falling back
/// to milliseconds.
fn window_name(window_ms: Option<u64>) -> String {
    let Some(window_ms) = window_ms.filter(|window_ms| *window_ms > 0) else {
        return "off".to_owned();
    };
    for (unit, size) in [
        ("d", 86_400_000_u64),
        ("h", 3_600_000),
        ("m", 60_000),
        ("s", 1_000),
    ] {
        if window_ms % size == 0 {
            return format!("{}{unit}", window_ms / size);
        }
    }
    format!("{window_ms}ms")
}

fn policy_value(policy: &ArchiveRetentionPolicy) -> Value {
    json!({
        "retention_ms": policy.retention_ms,
        "force_discard_worktrees": policy.force_discard_worktrees,
        "sub_agent_retention_ms": policy.sub_agent_retention_ms,
        "window": window_name(policy.retention_ms),
        "sub_agent": window_name(policy.sub_agent_retention_ms),
    })
}

fn emit_policy(out: &mut dyn Write, policy: &ArchiveRetentionPolicy) -> Result<(), Failure> {
    emit(
        out,
        &format!("window {}\n", window_name(policy.retention_ms)),
    )?;
    emit(
        out,
        &format!("force-discard {}\n", policy.force_discard_worktrees),
    )?;
    emit(
        out,
        &format!("sub-agent {}\n", window_name(policy.sub_agent_retention_ms)),
    )?;
    Ok(())
}

fn optional<T: std::fmt::Display>(value: Option<T>) -> String {
    match value {
        Some(value) => value.to_string(),
        None => "unknown".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::window_name;

    #[test]
    fn window_names_use_the_largest_whole_unit_and_off_for_disabled() {
        assert_eq!(window_name(None), "off");
        assert_eq!(window_name(Some(0)), "off");
        assert_eq!(window_name(Some(604_800_000)), "7d");
        assert_eq!(window_name(Some(1_209_600_000)), "14d");
        assert_eq!(window_name(Some(7_200_000)), "2h");
        assert_eq!(window_name(Some(1_500)), "1500ms");
        assert_eq!(window_name(Some(600_000)), "10m");
    }
}
