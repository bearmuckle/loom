//! The project verbs: `project get`, `tree`, `tasks`, `children`, `messages`,
//! `send`, and `child`.
//!
//! Project coordination is backend-authoritative. Every verb reads the worker's
//! own `Project/GetProjectSnapshotForSession` projection and prints it, so this
//! client never builds a tree, keeps a cache, or infers a lifecycle a worker
//! could disagree with.
//!
//! `project send` is the one direction verb, and it deliberately does not send
//! `Project/SendProjectAgentMessage`: the worker refuses that request for every
//! client connection ("agent messages may only be sent by a server-bound agent
//! run"), so a direction is delivered as a follow-up to the child session's
//! active run through `Collaboration/DirectAgent`, with the same revision read
//! and single conflict retry the `send` verb uses. The durable parent-child log
//! stays read-only through `project messages`.

use std::io::Write;

use loom_core::{
    AgentMessageKind, AgentMessageRecord, AgentSessionId, DelegatedTaskRecord, DelegatedTaskStatus,
    ErrorCode, LoomError, ProjectSnapshot,
};
use loom_protocol::{
    ActivityDetail, AgentSessionSnapshotProjection, ClientRequest, CollaborationRequest,
    ProjectChildControlAction, ProjectRequest, ProjectResponse, RunActionOutcome, ServerResponse,
    SessionRequest, SessionResponse,
};
use serde_json::{Value, json};

use crate::cli::args::{Invocation, ProjectChildAction, ProjectMessagesArgs, ProjectVerb};
use crate::cli::client::Client;
use crate::cli::text::{bound, escape, run_state_name, session_state_name};

use super::supervise::{act, run_value};
use super::{Caller, Failure, Outcome, emit, handshake};

/// Runs one `project` sub-verb.
pub(super) async fn project<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    verb: &ProjectVerb,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    match verb {
        ProjectVerb::Get { session } => get(&mut caller, invocation, *session, out).await,
        ProjectVerb::Tree { session } => tree(&mut caller, invocation, *session, out).await,
        ProjectVerb::Tasks { session } => tasks(&mut caller, invocation, *session, out).await,
        ProjectVerb::Children { session } => children(&mut caller, invocation, *session, out).await,
        ProjectVerb::Messages(args) => messages(&mut caller, invocation, args, out).await,
        ProjectVerb::Send { session, text } => {
            send(&mut caller, invocation, *session, text, out).await
        }
        ProjectVerb::Child { session, action } => {
            child(&mut caller, invocation, *session, *action, out).await
        }
    }
}

/// The worker's project projection for one member session.
async fn snapshot<C: Client>(
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

/// One session's metadata with the run detail the child verbs report, together
/// with the id of the request that answered it.
async fn projection<C: Client>(
    caller: &mut Caller<'_, C>,
    session_id: AgentSessionId,
) -> Result<(loom_core::RequestId, AgentSessionSnapshotProjection), Failure> {
    let (request_id, response) = caller
        .call(ClientRequest::Session(
            SessionRequest::GetAgentSessionSnapshotMetadata {
                session_id,
                activity_detail: ActivityDetail::Replies,
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

/// `project get`: one project's ids and member counts.
async fn get<C: Client>(
    caller: &mut Caller<'_, C>,
    invocation: &Invocation,
    session_id: AgentSessionId,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let project = snapshot(caller, session_id).await?;
    if !invocation.globals.json {
        emit(out, &format!("project {}\n", project.project_id))?;
        emit(out, &format!("root {}\n", project.root_session_id))?;
        emit(
            out,
            &format!("parent-project {}\n", id_or_none(project.parent_project_id)),
        )?;
        emit(out, &format!("agents {}\n", project.agents.len()))?;
        emit(out, &format!("tasks {}\n", project.tasks.len()))?;
        emit(out, &format!("worktrees {}\n", project.worktrees.len()))?;
    }
    Ok(Outcome::new(
        serde_json::to_value(&project).unwrap_or(Value::Null),
        caller.primary,
    ))
}

/// `project tree`: one line per agent, indented by its depth.
async fn tree<C: Client>(
    caller: &mut Caller<'_, C>,
    invocation: &Invocation,
    session_id: AgentSessionId,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let project = snapshot(caller, session_id).await?;
    if !invocation.globals.json {
        for agent in &project.agents {
            // Depth one is the root, so an agent's indent is two spaces per
            // level below it. The summary is escaped and never spans a line.
            let indent = "  ".repeat(agent.depth.saturating_sub(1) as usize);
            let summary = agent
                .task_summary
                .as_deref()
                .filter(|summary| !summary.is_empty())
                .map(escape)
                .unwrap_or_else(|| "-".to_owned());
            emit(
                out,
                &format!(
                    "{indent}{} {} {summary}\n",
                    agent.session_id,
                    session_state_name(agent.state)
                ),
            )?;
        }
    }
    let data = json!({
        "project_id": project.project_id.to_string(),
        "agents": serde_json::to_value(&project.agents).unwrap_or(Value::Null),
    });
    Ok(Outcome::new(data, caller.primary))
}

/// `project tasks`: one line per delegated task of the projection.
async fn tasks<C: Client>(
    caller: &mut Caller<'_, C>,
    invocation: &Invocation,
    session_id: AgentSessionId,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let project = snapshot(caller, session_id).await?;
    if !invocation.globals.json {
        for task in &project.tasks {
            emit(
                out,
                &format!(
                    "{} {} {} {} {} {} {}\n",
                    task.task_id,
                    task_status_name(task.status),
                    task.requester_session_id,
                    task.target_session_id,
                    escape(&task.model_id),
                    dependencies(task),
                    escape(&task.intent),
                ),
            )?;
        }
    }
    let data = json!({
        "project_id": project.project_id.to_string(),
        "tasks": serde_json::to_value(&project.tasks).unwrap_or(Value::Null),
    });
    Ok(Outcome::new(data, caller.primary))
}

/// `project children`: every delegated child with its task status, its active
/// run, and the decision that run waits on.
async fn children<C: Client>(
    caller: &mut Caller<'_, C>,
    invocation: &Invocation,
    session_id: AgentSessionId,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let project = snapshot(caller, session_id).await?;
    let mut children = Vec::new();
    for agent in project.agents.iter().filter(|agent| agent.depth > 1) {
        let (_, metadata) = projection(caller, agent.session_id).await?;
        let task = project
            .tasks
            .iter()
            .find(|task| task.target_session_id == agent.session_id);
        let active = metadata.active_run.as_ref();
        let pending_approval = active
            .and_then(|active| active.pending_approval.as_ref())
            .map(|call| call.id.to_string());
        let pending_input = active.and_then(|active| active.pending_input.clone());
        let state = session_state_name(metadata.session.state);
        let task_state = task.map(|task| task_status_name(task.status));
        let run_state = active.map(|active| run_state_name(active.run.state));
        if !invocation.globals.json {
            // A run waits on a decision or a question, never both, so the line
            // carries at most one marker and stays one physical line.
            let marker = if let Some(tool_call_id) = &pending_approval {
                format!(" approval {tool_call_id}")
            } else if let Some(prompt) = &pending_input {
                format!(" question {}", escape(prompt))
            } else {
                String::new()
            };
            emit(
                out,
                &format!(
                    "{} {state} {} {}{marker}\n",
                    agent.session_id,
                    task_state.unwrap_or("none"),
                    run_state.unwrap_or("none"),
                ),
            )?;
        }
        children.push(json!({
            "session_id": agent.session_id.to_string(),
            "state": state,
            "task": task_state,
            "run": run_state,
            "pending_approval": pending_approval,
            "pending_input": pending_input,
        }));
    }
    let data = json!({
        "project_id": project.project_id.to_string(),
        "children": children,
    });
    Ok(Outcome::new(data, caller.primary))
}

/// `project messages`: the durable parent-child log, oldest first.
async fn messages<C: Client>(
    caller: &mut Caller<'_, C>,
    invocation: &Invocation,
    args: &ProjectMessagesArgs,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    // The page is scoped to a project, and the id the caller passed names one
    // member, so the project id comes from the worker's projection rather than
    // from the id itself.
    let project = snapshot(caller, args.session).await?;
    let (request_id, response) = caller
        .call(ClientRequest::Project(
            ProjectRequest::ListProjectAgentMessages {
                project_id: project.project_id,
                session_id: args.session,
                after_project_sequence: args.after,
                limit: args.limit,
            },
        ))
        .await?;
    let (mut messages, next_after_project_sequence) = match response {
        ServerResponse::Project(ProjectResponse::ProjectAgentMessages {
            messages,
            next_after_project_sequence,
        }) => (messages, next_after_project_sequence),
        _ => return Err(Failure::unexpected(request_id, "project agent messages")),
    };
    messages.sort_by_key(|message| message.project_sequence);
    if !invocation.globals.json {
        for message in &messages {
            emit(out, &message_line(message, args.max_bytes))?;
        }
    }
    let data = json!({
        "project_id": project.project_id.to_string(),
        "messages": serde_json::to_value(&messages).unwrap_or(Value::Null),
        "next_after_project_sequence": next_after_project_sequence,
    });
    Ok(Outcome::new(data, caller.primary))
}

/// One message as its text line, with the body bounded before it is escaped so
/// the omitted count is a count of the original bytes.
fn message_line(message: &AgentMessageRecord, max_bytes: usize) -> String {
    let body = escape(&bound(&message.body, max_bytes));
    if body.is_empty() {
        format!(
            "{} {} {}\n",
            message.project_sequence,
            message_kind_name(message.kind),
            message.sender_session_id
        )
    } else {
        format!(
            "{} {} {} {body}\n",
            message.project_sequence,
            message_kind_name(message.kind),
            message.sender_session_id
        )
    }
}

/// `project send`: a follow-up direction to the child session's active run.
async fn send<C: Client>(
    caller: &mut Caller<'_, C>,
    invocation: &Invocation,
    session_id: AgentSessionId,
    text: &str,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let project = snapshot(caller, session_id).await?;
    if !is_delegated_child(&project, session_id) {
        return Err(Failure::Usage(not_a_child(&project, session_id)));
    }
    let (request_id, metadata) = projection(caller, session_id).await?;
    let Some(run_id) = metadata.active_run.as_ref().map(|active| active.run.id) else {
        // A direction needs a live run to carry it, and starting one is a
        // separate decision, so the caller is told to resume the child.
        return Err(Failure::Server {
            request_id,
            error: LoomError::new(
                ErrorCode::InvalidState,
                format!(
                    "child session {session_id} has no active run; resume it first with \
                     `project child resume {session_id}`"
                ),
                false,
            ),
        });
    };
    let outcome = act(caller, run_id, true, "direct the child agent", |expected| {
        ClientRequest::Collaboration(CollaborationRequest::DirectAgent {
            run_id,
            expected,
            text: text.to_owned(),
        })
    })
    .await?;
    let RunActionOutcome::Accepted { run, revision, .. } = &outcome else {
        unreachable!("a conflict is reported by `act`")
    };
    if !invocation.globals.json {
        emit(
            out,
            &format!(
                "sent {} {} {}\n",
                run.session_id,
                run.id,
                run_state_name(run.state)
            ),
        )?;
    }
    let data = json!({
        "action": "send",
        "child_session_id": run.session_id.to_string(),
        "run": run_value(run),
        "revision": revision,
    });
    Ok(Outcome::new(data, caller.primary))
}

/// `project child`: pause, resume, interrupt, or cancel one delegated child
/// task. The worker owns every lifecycle rule, so a refusal is reported as it
/// comes.
async fn child<C: Client>(
    caller: &mut Caller<'_, C>,
    invocation: &Invocation,
    session_id: AgentSessionId,
    action: ProjectChildAction,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let project = snapshot(caller, session_id).await?;
    let Some(task) = project
        .tasks
        .iter()
        .find(|task| task.target_session_id == session_id)
    else {
        return Err(Failure::Usage(not_a_child(&project, session_id)));
    };
    // The manager that owns the delegation is the task's requester, and the
    // worker checks that link itself before it acts.
    let (request_id, response) = caller
        .call(ClientRequest::Project(
            ProjectRequest::ControlProjectChild {
                project_id: project.project_id,
                manager_session_id: task.requester_session_id,
                task_id: task.task_id,
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
                "{} {} {}\n",
                action.past(),
                task.task_id,
                task_status_name(task.status)
            ),
        )?;
        if let Some(run) = &run {
            emit(
                out,
                &format!("run {} {}\n", run.id, run_state_name(run.state)),
            )?;
        }
    }
    let data = json!({
        "action": action.name(),
        "task": serde_json::to_value(&*task).unwrap_or(Value::Null),
        "run": run.as_deref().map(run_value),
    });
    Ok(Outcome::new(data, caller.primary))
}

/// Why a `project send` or `project child` refused a session: the id is not the
/// target of any delegation in this project.
fn not_a_child(project: &ProjectSnapshot, session_id: AgentSessionId) -> String {
    format!(
        "{session_id} is not a delegated child of project {}; list its children with \
         `project children {}`",
        project.project_id, project.root_session_id
    )
}

/// Whether some delegation of this project targets `session_id`.
fn is_delegated_child(project: &ProjectSnapshot, session_id: AgentSessionId) -> bool {
    project
        .tasks
        .iter()
        .any(|task| task.target_session_id == session_id)
}

fn control_action(action: ProjectChildAction) -> ProjectChildControlAction {
    match action {
        ProjectChildAction::Pause => ProjectChildControlAction::Pause,
        ProjectChildAction::Resume => ProjectChildControlAction::Continue,
        ProjectChildAction::Interrupt => ProjectChildControlAction::Interrupt,
        ProjectChildAction::Cancel => ProjectChildControlAction::Cancel,
    }
}

/// The dependencies of one task, ascending, or `-` when it has none.
fn dependencies(task: &DelegatedTaskRecord) -> String {
    if task.dependencies.is_empty() {
        return "-".to_owned();
    }
    let mut dependencies = task
        .dependencies
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    dependencies.sort();
    dependencies.join(",")
}

/// The snake_case name of a delegated task status, spelled exactly like the
/// protocol's serde name.
const fn task_status_name(status: DelegatedTaskStatus) -> &'static str {
    match status {
        DelegatedTaskStatus::Queued => "queued",
        DelegatedTaskStatus::Running => "running",
        DelegatedTaskStatus::Blocked => "blocked",
        DelegatedTaskStatus::Completed => "completed",
        DelegatedTaskStatus::Failed => "failed",
        DelegatedTaskStatus::Cancelled => "cancelled",
    }
}

/// The snake_case name of a message kind, spelled exactly like the protocol's
/// serde name.
const fn message_kind_name(kind: AgentMessageKind) -> &'static str {
    match kind {
        AgentMessageKind::Progress => "progress",
        AgentMessageKind::Result => "result",
        AgentMessageKind::Question => "question",
        AgentMessageKind::Blocker => "blocker",
        AgentMessageKind::Direction => "direction",
        AgentMessageKind::Answer => "answer",
    }
}

fn id_or_none<T: std::fmt::Display>(value: Option<T>) -> String {
    match value {
        Some(value) => value.to_string(),
        None => "none".to_owned(),
    }
}
