//! The project verbs: the hierarchy, delegated tasks, child sessions, message
//! history, and child control.
//!
//! Every read is addressed by the id of any project member session, so one
//! verb works from the project root and from any child: the root session id is
//! also the project id, and the worker resolves a child's id to its project.
//! All of them are reads and negotiate no mutating capability.
//!
//! There is deliberately no message send verb: an agent-attributed message is
//! reserved for the server-bound agent tool, and a person redirects a live
//! child with `send --run`.
//!
//! `project child` sends the requester that the server-owned task record names
//! as the manager, never an id from a flag, so a client cannot stand in for the
//! manager's run identity.

use std::io::Write;

use loom_core::{
    AgentMessageKind, AgentSessionId, DelegatedTaskRecord, DelegatedTaskStatus, LoomError,
    ProjectAgentRecord, ProjectSnapshot, RequestId, TaskId,
};
use loom_protocol::{
    ActivityDetail, AgentSessionSnapshotProjection, ClientRequest, ProjectChildControlAction,
    ProjectRequest, ProjectResponse, ServerResponse, SessionRequest, SessionResponse,
};
use serde_json::{Value, json};

use crate::cli::args::{DEFAULT_MAX_BYTES, Invocation, ProjectChildAction};
use crate::cli::client::Client;
use crate::cli::text::{bound, escape, run_state_name, session_state_name};

use super::{Caller, Failure, Outcome, emit, handshake};

/// `project get`: the project's members and the project it was branched from.
pub(super) async fn get<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    session_id: AgentSessionId,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let (_, project) = snapshot(&mut caller, session_id).await?;
    if !invocation.globals.json {
        emit_header(out, &project)?;
        for agent in &project.agents {
            emit(
                out,
                &format!(
                    "agent {} {} depth {} parent {}\n",
                    agent.session_id,
                    session_state_name(agent.state),
                    agent.depth,
                    dash(agent.parent_session_id)
                ),
            )?;
        }
    }
    Ok(Outcome::new(value_of(&project), caller.primary))
}

/// `project tree`: the same hierarchy, walked from the root as an indented tree.
pub(super) async fn tree<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    session_id: AgentSessionId,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let (_, project) = snapshot(&mut caller, session_id).await?;
    if !invocation.globals.json {
        emit_header(out, &project)?;
        for agent in tree_order(&project) {
            let indent = "  ".repeat(usize::from(agent.depth.saturating_sub(1)));
            // The summary is bounded before escaping, so one member stays on one
            // physical line and an absent or empty summary is `-`.
            let summary = agent
                .task_summary
                .as_deref()
                .map(|summary| escape(&bound(summary, DEFAULT_MAX_BYTES)))
                .filter(|summary| !summary.is_empty());
            emit(
                out,
                &format!(
                    "{indent}{} {} {}\n",
                    agent.session_id,
                    session_state_name(agent.state),
                    dash(summary.as_deref())
                ),
            )?;
        }
    }
    Ok(Outcome::new(value_of(&project), caller.primary))
}

/// `project tasks`: the project's delegated tasks in snapshot order.
pub(super) async fn tasks<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    session_id: AgentSessionId,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let (_, project) = snapshot(&mut caller, session_id).await?;
    if !invocation.globals.json {
        emit(
            out,
            &format!(
                "project {} tasks {}\n",
                project.project_id,
                project.tasks.len()
            ),
        )?;
        for task in &project.tasks {
            emit(out, &task_line(&project, task))?;
        }
    }
    let data = json!({
        "project_id": project.project_id.to_string(),
        "tasks": serde_json::to_value(&project.tasks).unwrap_or(Value::Null),
    });
    Ok(Outcome::new(data, caller.primary))
}

/// `project children`: the addressed session's direct children, each with the
/// task that created it and the run state its session metadata reports.
pub(super) async fn children<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    session_id: AgentSessionId,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let (_, project) = snapshot(&mut caller, session_id).await?;
    let direct = project
        .agents
        .iter()
        .filter(|agent| agent.parent_session_id == Some(session_id))
        .collect::<Vec<_>>();
    if !invocation.globals.json {
        emit(
            out,
            &format!("project {} children {}\n", project.project_id, direct.len()),
        )?;
    }
    let mut rows = Vec::with_capacity(direct.len());
    for agent in &direct {
        let (_, projection) = session_projection(&mut caller, agent.session_id).await?;
        let task = project
            .tasks
            .iter()
            .find(|task| task.target_session_id == agent.session_id);
        let active = projection.active_run.as_ref();
        let run = active.map(|active| &active.run);
        let approval = active
            .and_then(|active| active.pending_approval.as_ref())
            .map(|call| call.id);
        // The question and every message body are bounded before escaping, so
        // the omitted count is a count of the original bytes.
        let question = active
            .and_then(|active| active.pending_input.as_deref())
            .map(|question| escape(&bound(question, DEFAULT_MAX_BYTES)))
            .filter(|question| !question.is_empty());
        if !invocation.globals.json {
            emit(
                out,
                &format!(
                    "child {} task {} {} session {} run {} {} approval {} question {}\n",
                    agent.session_id,
                    dash(task.map(|task| task.task_id)),
                    dash(task.map(|task| task_status_name(task.status))),
                    session_state_name(projection.session.state),
                    dash(run.map(|run| run.id)),
                    dash(run.map(|run| run_state_name(run.state))),
                    dash(approval),
                    dash(question),
                ),
            )?;
        }
        rows.push(json!({
            "agent": serde_json::to_value(agent).unwrap_or(Value::Null),
            "task": task.map(|task| serde_json::to_value(task).unwrap_or(Value::Null)),
            "run": run.map(|run| serde_json::to_value(run).unwrap_or(Value::Null)),
        }));
    }
    let count = direct.len();
    let data = json!({
        "project_id": project.project_id.to_string(),
        "children": rows,
        "count": count,
    });
    Ok(Outcome::new(data, caller.primary))
}

/// `project messages`: one page of the project's agent message history.
///
/// The history is read-only in this client: `--after` pages the server's order
/// and `--limit` caps the page, and nothing here can store a message.
pub(super) async fn messages<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    session_id: AgentSessionId,
    after: Option<u64>,
    limit: u32,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    // The project id comes from the authoritative snapshot, so the read is
    // addressed the same way whichever member session the caller names.
    let (_, project) = snapshot(&mut caller, session_id).await?;
    let (request_id, response) = caller
        .call(ClientRequest::Project(
            ProjectRequest::ListProjectAgentMessages {
                project_id: project.project_id,
                session_id,
                after_project_sequence: after,
                limit,
            },
        ))
        .await?;
    let (messages, next) = match response {
        ServerResponse::Project(ProjectResponse::ProjectAgentMessages {
            messages,
            next_after_project_sequence,
        }) => (messages, next_after_project_sequence),
        _ => return Err(Failure::unexpected(request_id, "project agent messages")),
    };
    if !invocation.globals.json {
        emit(
            out,
            &format!(
                "project {} messages {}\n",
                project.project_id,
                messages.len()
            ),
        )?;
        for message in &messages {
            let body = escape(&bound(&message.body, DEFAULT_MAX_BYTES));
            let mut line = format!(
                "message {} {} from {} to {} task {}",
                message.project_sequence,
                message_kind_name(message.kind),
                message.sender_session_id,
                message.target_session_id,
                dash(message.task_id)
            );
            if !body.is_empty() {
                line.push(' ');
                line.push_str(&body);
            }
            line.push('\n');
            emit(out, &line)?;
        }
        if let Some(next) = next {
            emit(out, &format!("next {next}\n"))?;
        }
    }
    let data = json!({
        "project_id": project.project_id.to_string(),
        "session_id": session_id.to_string(),
        "messages": serde_json::to_value(&messages).unwrap_or(Value::Null),
        "next_after_project_sequence": next,
    });
    Ok(Outcome::new(data, caller.primary))
}

/// `project child`: one control action on a delegated task.
///
/// The task is looked up in the authoritative project snapshot first: a task id
/// that this project does not name is reported as not found without sending a
/// control request, and a named task supplies the manager session id.
pub(super) async fn child<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    session_id: AgentSessionId,
    task_id: TaskId,
    action: ProjectChildAction,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let (snapshot_request, project) = snapshot(&mut caller, session_id).await?;
    let Some(task) = project.tasks.iter().find(|task| task.task_id == task_id) else {
        return Err(Failure::Server {
            request_id: snapshot_request,
            error: LoomError::not_found("delegated task", task_id),
        });
    };
    let (request_id, response) = caller
        .call(ClientRequest::Project(
            ProjectRequest::ControlProjectChild {
                project_id: project.project_id,
                manager_session_id: task.requester_session_id,
                task_id,
                action: control_action(action),
            },
        ))
        .await?;
    let (task, run) = match response {
        ServerResponse::Project(ProjectResponse::ProjectChildControlled { task, run }) => {
            (task, run)
        }
        _ => return Err(Failure::unexpected(request_id, "project child control")),
    };
    if !invocation.globals.json {
        emit(
            out,
            &format!(
                "{} {} status {} run {} {}\n",
                action.word(),
                task.task_id,
                task_status_name(task.status),
                dash(run.as_deref().map(|run| run.id)),
                dash(run.as_deref().map(|run| run_state_name(run.state))),
            ),
        )?;
    }
    let data = json!({
        "action": action.word(),
        "task": serde_json::to_value(&*task).unwrap_or(Value::Null),
        "run": run
            .as_deref()
            .map(|run| serde_json::to_value(run).unwrap_or(Value::Null)),
    });
    Ok(Outcome::new(data, caller.primary))
}

/// Reads the project snapshot that any member session addresses, together with
/// the id of that request.
async fn snapshot<C: Client>(
    caller: &mut Caller<'_, C>,
    session_id: AgentSessionId,
) -> Result<(RequestId, ProjectSnapshot), Failure> {
    let (request_id, response) = caller
        .call(ClientRequest::Project(
            ProjectRequest::GetProjectSnapshotForSession { session_id },
        ))
        .await?;
    match response {
        ServerResponse::Project(ProjectResponse::ProjectSnapshot(project)) => {
            Ok((request_id, project))
        }
        _ => Err(Failure::unexpected(request_id, "project snapshot")),
    }
}

/// The session metadata projection of one child agent.
async fn session_projection<C: Client>(
    caller: &mut Caller<'_, C>,
    session_id: AgentSessionId,
) -> Result<(RequestId, AgentSessionSnapshotProjection), Failure> {
    let (request_id, response) = caller
        .call(ClientRequest::Session(
            SessionRequest::GetAgentSessionSnapshotMetadata {
                session_id,
                activity_detail: ActivityDetail::Full,
            },
        ))
        .await?;
    match response {
        ServerResponse::Session(SessionResponse::AgentSessionSnapshot(projection)) => {
            Ok((request_id, *projection))
        }
        _ => Err(Failure::unexpected(request_id, "session metadata")),
    }
}

/// The control action a sub-verb sends: `resume` continues the paused child.
fn control_action(action: ProjectChildAction) -> ProjectChildControlAction {
    match action {
        ProjectChildAction::Pause => ProjectChildControlAction::Pause,
        ProjectChildAction::Resume => ProjectChildControlAction::Continue,
        ProjectChildAction::Interrupt => ProjectChildControlAction::Interrupt,
        ProjectChildAction::Cancel => ProjectChildControlAction::Cancel,
    }
}

/// The `project <id> root <id> parent <id|->` line both `get` and `tree` print.
fn emit_header(out: &mut dyn Write, project: &ProjectSnapshot) -> Result<(), Failure> {
    emit(
        out,
        &format!(
            "project {} root {} parent {}\n",
            project.project_id,
            project.root_session_id,
            dash(project.parent_project_id)
        ),
    )
}

/// One `task` line, with `blocked-by` naming the dependencies that are not
/// completed: an unfinished dependency and one the snapshot does not carry both
/// keep the task from starting.
fn task_line(project: &ProjectSnapshot, task: &DelegatedTaskRecord) -> String {
    let blocked_by = task
        .dependencies
        .iter()
        .filter(|dependency| {
            match project
                .tasks
                .iter()
                .find(|task| task.task_id == **dependency)
            {
                Some(task) => task.status != DelegatedTaskStatus::Completed,
                None => true,
            }
        })
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let blocked_by = if blocked_by.is_empty() {
        "-".to_owned()
    } else {
        blocked_by.join(",")
    };
    // The intent is bounded before escaping, so the omitted count is a count of
    // the original bytes and the line never spans two physical lines.
    let intent = escape(&bound(&task.intent, DEFAULT_MAX_BYTES));
    let mut line = format!(
        "task {} {} owner {} target {} model {} code {} blocked-by {blocked_by} intent",
        task.task_id,
        task_status_name(task.status),
        task.requester_session_id,
        task.target_session_id,
        escape(&task.model_id),
        task.code_change,
    );
    if !intent.is_empty() {
        line.push(' ');
        line.push_str(&intent);
    }
    line.push('\n');
    line
}

/// The members in pre-order from the root, following `parent_session_id` with
/// siblings in snapshot order. A walk that would miss a member (a record whose
/// parent is absent, or a cycle) falls back to snapshot order, so every member
/// is always printed once.
fn tree_order(project: &ProjectSnapshot) -> Vec<&ProjectAgentRecord> {
    let mut ordered = Vec::with_capacity(project.agents.len());
    let mut visited = Vec::with_capacity(project.agents.len());
    depth_first(project, project.root_session_id, &mut visited, &mut ordered);
    if ordered.len() == project.agents.len() {
        ordered
    } else {
        project.agents.iter().collect()
    }
}

fn depth_first<'a>(
    project: &'a ProjectSnapshot,
    session_id: AgentSessionId,
    visited: &mut Vec<AgentSessionId>,
    ordered: &mut Vec<&'a ProjectAgentRecord>,
) {
    if visited.contains(&session_id) {
        return;
    }
    let Some(record) = project
        .agents
        .iter()
        .find(|agent| agent.session_id == session_id)
    else {
        return;
    };
    visited.push(session_id);
    ordered.push(record);
    for child in project
        .agents
        .iter()
        .filter(|agent| agent.parent_session_id == Some(session_id))
    {
        depth_first(project, child.session_id, visited, ordered);
    }
}

/// A field's value, or `-` when it is absent, as the text projection spells an
/// absent id or state.
fn dash<T: std::fmt::Display>(value: Option<T>) -> String {
    match value {
        Some(value) => value.to_string(),
        None => "-".to_owned(),
    }
}

fn value_of(project: &ProjectSnapshot) -> Value {
    serde_json::to_value(project).unwrap_or(Value::Null)
}

/// The snake_case protocol name of a delegated task's status.
fn task_status_name(status: DelegatedTaskStatus) -> &'static str {
    match status {
        DelegatedTaskStatus::Queued => "queued",
        DelegatedTaskStatus::Running => "running",
        DelegatedTaskStatus::Blocked => "blocked",
        DelegatedTaskStatus::Completed => "completed",
        DelegatedTaskStatus::Failed => "failed",
        DelegatedTaskStatus::Cancelled => "cancelled",
    }
}

/// The snake_case protocol name of an agent message's kind.
fn message_kind_name(kind: AgentMessageKind) -> &'static str {
    match kind {
        AgentMessageKind::Progress => "progress",
        AgentMessageKind::Result => "result",
        AgentMessageKind::Question => "question",
        AgentMessageKind::Blocker => "blocker",
        AgentMessageKind::Direction => "direction",
        AgentMessageKind::Answer => "answer",
    }
}
