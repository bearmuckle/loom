//! The control and supervision verbs: `send`, `answer`, `approve`, `deny`,
//! `stop`, `wait`, `status`, `liveness`, and `result`.
//!
//! Every run-scoped verb reads `Collaboration/GetRunRevision` and acts with
//! that revision. `send`, `answer`, `approve`, and `deny` retry once after a
//! `Conflict` because their intent is idempotent; `stop` refuses the conflict
//! unless `--retry-on-conflict` is given, so a stale revision cannot interrupt
//! a run whose state the caller has not seen.

use std::{
    io::Write,
    time::{Duration, Instant},
};

use loom_core::{
    AgentSessionId, AgentSessionState, ErrorCode, GitFileStatusKind, RequestId, RunControlAction,
    RunId, RunRevision, SessionRepository, ToolCallId,
};
use loom_protocol::{
    ActivityDetail, AgentRunSnapshot, AgentSessionSnapshotProjection, ClientRequest,
    CollaborationRequest, CollaborationResponse, RepositoryRequest, RepositoryResponse,
    RunActionOutcome, ServerResponse, SessionRequest, SessionResponse, TaskRequest, TaskResponse,
};
use serde_json::{Value, json};

use crate::cli::args::{Invocation, StatusFormat, WaitUntil};
use crate::cli::client::Client;
use crate::cli::exit;
use crate::cli::text::{escape, run_state_name, session_state_name};

use super::{Caller, Failure, Outcome, emit, handshake};

/// How long `wait` waits before its first poll returns nothing new.
const WAIT_MIN_DELAY: Duration = Duration::from_millis(250);
/// The longest `wait` sleeps between polls.
const WAIT_MAX_DELAY: Duration = Duration::from_secs(2);

/// `send`: a follow-up direction to the run.
pub(super) async fn send<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    run_id: RunId,
    text: &str,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let outcome = act(&mut caller, run_id, true, "direct the agent", |expected| {
        ClientRequest::Collaboration(CollaborationRequest::DirectAgent {
            run_id,
            expected,
            text: text.to_owned(),
        })
    })
    .await?;
    finish_action(invocation, "send", None, &outcome, caller.primary, out)
}

/// `answer`: the answer to the question the run waits on.
pub(super) async fn answer<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    run_id: RunId,
    text: &str,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let outcome = act(
        &mut caller,
        run_id,
        true,
        "answer the agent question",
        |expected| {
            ClientRequest::Collaboration(CollaborationRequest::AnswerAgentQuestion {
                run_id,
                expected,
                text: text.to_owned(),
            })
        },
    )
    .await?;
    finish_action(invocation, "answer", None, &outcome, caller.primary, out)
}

/// `approve`: approve the blocked tool call.
pub(super) async fn approve<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    run_id: RunId,
    tool_call_id: ToolCallId,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let outcome = act(
        &mut caller,
        run_id,
        true,
        "approve the tool call",
        |expected| {
            ClientRequest::Collaboration(CollaborationRequest::ControlRun {
                run_id,
                expected,
                action: RunControlAction::Approve { tool_call_id },
            })
        },
    )
    .await?;
    finish_action(
        invocation,
        "approve",
        Some(tool_call_id),
        &outcome,
        caller.primary,
        out,
    )
}

/// `deny`: reject the blocked tool call with an optional reason.
pub(super) async fn deny<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    run_id: RunId,
    tool_call_id: ToolCallId,
    reason: Option<&str>,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let outcome = act(
        &mut caller,
        run_id,
        true,
        "deny the tool call",
        |expected| {
            ClientRequest::Collaboration(CollaborationRequest::ControlRun {
                run_id,
                expected,
                action: RunControlAction::Reject {
                    tool_call_id,
                    reason: reason.map(str::to_owned),
                },
            })
        },
    )
    .await?;
    finish_action(
        invocation,
        "deny",
        Some(tool_call_id),
        &outcome,
        caller.primary,
        out,
    )
}

/// `stop`: interrupt the run, and archive the session afterwards when asked.
/// A stale revision is refused unless `--retry-on-conflict` is given.
pub(super) async fn stop<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    run_id: RunId,
    archive: Option<AgentSessionId>,
    retry_on_conflict: bool,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let outcome = act(
        &mut caller,
        run_id,
        retry_on_conflict,
        "stop the run",
        |expected| {
            ClientRequest::Collaboration(CollaborationRequest::ControlRun {
                run_id,
                expected,
                action: RunControlAction::Interrupt,
            })
        },
    )
    .await?;
    let RunActionOutcome::Accepted { run, revision, .. } = &outcome else {
        unreachable!("a conflict is reported by `act`")
    };
    let mut archived = false;
    if let Some(session_id) = archive {
        let (request_id, response) = caller
            .call(ClientRequest::Session(
                SessionRequest::ArchiveAgentSession { session_id },
            ))
            .await?;
        match response {
            ServerResponse::Session(SessionResponse::AgentSessionArchived(_)) => archived = true,
            _ => return Err(Failure::unexpected(request_id, "session archive")),
        }
    }
    let state = run_state_name(run.state);
    if !invocation.globals.json {
        emit(out, &format!("stopped {} {state}\n", run.id))?;
        if archived {
            emit(
                out,
                &format!("archived {}\n", archive.expect("archived names a session")),
            )?;
        }
    }
    let data = json!({
        "action": "stop",
        "run": run_value(run),
        "revision": revision,
        "archived": archive.filter(|_| archived).map(|id| id.to_string()),
    });
    Ok(Outcome::new(data, caller.primary))
}

/// Reads the run's current revision.
async fn revision<C: Client>(
    caller: &mut Caller<'_, C>,
    run_id: RunId,
) -> Result<RunRevision, Failure> {
    let (request_id, response) = caller
        .call(ClientRequest::Collaboration(
            CollaborationRequest::GetRunRevision { run_id },
        ))
        .await?;
    match response {
        ServerResponse::Collaboration(CollaborationResponse::RunRevision { revision, .. }) => {
            Ok(revision)
        }
        _ => Err(Failure::unexpected(request_id, "run revision")),
    }
}

/// Reads the revision, sends one run action, and retries once after a conflict
/// when `retry` allows it.
async fn act<C: Client, F>(
    caller: &mut Caller<'_, C>,
    run_id: RunId,
    retry: bool,
    what: &str,
    make: F,
) -> Result<RunActionOutcome, Failure>
where
    F: Fn(RunRevision) -> ClientRequest,
{
    let expected = revision(caller, run_id).await?;
    let (request_id, response) = caller.call(make(expected)).await?;
    let outcome = run_action(request_id, response)?;
    if matches!(outcome, RunActionOutcome::Conflict { .. }) {
        if !retry {
            return Err(Failure::conflict(
                request_id,
                format!(
                    "the run moved since it was read; refusing to {what} against a stale revision \
                     (pass --retry-on-conflict to re-read and retry)"
                ),
            ));
        }
        // The caller's intent is idempotent, so re-read once and act again.
        let refreshed = revision(caller, run_id).await?;
        let (request_id, response) = caller.call(make(refreshed)).await?;
        let outcome = run_action(request_id, response)?;
        if matches!(outcome, RunActionOutcome::Conflict { .. }) {
            return Err(Failure::conflict(
                request_id,
                format!("the run moved again while trying to {what}"),
            ));
        }
        return Ok(outcome);
    }
    Ok(outcome)
}

fn run_action(
    request_id: RequestId,
    response: ServerResponse,
) -> Result<RunActionOutcome, Failure> {
    match response {
        ServerResponse::Collaboration(CollaborationResponse::RunAction(outcome)) => Ok(*outcome),
        _ => Err(Failure::unexpected(request_id, "run action")),
    }
}

/// Emits the text and payload of one accepted run action.
fn finish_action(
    invocation: &Invocation,
    action: &str,
    tool_call_id: Option<ToolCallId>,
    outcome: &RunActionOutcome,
    request_id: Option<RequestId>,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let RunActionOutcome::Accepted { run, revision, .. } = outcome else {
        unreachable!("a conflict is reported by `act`")
    };
    let state = run_state_name(run.state);
    if !invocation.globals.json {
        match (action, tool_call_id) {
            ("send", _) => emit(out, &format!("sent {} {state}\n", run.id))?,
            ("answer", _) => emit(out, &format!("answered {} {state}\n", run.id))?,
            ("approve", Some(tool_call_id)) => {
                emit(
                    out,
                    &format!("approved {tool_call_id} {} {state}\n", run.id),
                )?;
            }
            ("deny", Some(tool_call_id)) => {
                emit(out, &format!("denied {tool_call_id} {} {state}\n", run.id))?;
            }
            _ => emit(out, &format!("{action} {} {state}\n", run.id))?,
        }
    }
    let mut data = json!({
        "action": action,
        "run": run_value(run),
        "revision": revision,
    });
    if let Some(tool_call_id) = tool_call_id {
        data["tool_call_id"] = json!(tool_call_id.to_string());
    }
    Ok(Outcome::new(data, request_id))
}

/// The run fields every control verb reports.
fn run_value(run: &AgentRunSnapshot) -> Value {
    json!({
        "id": run.id.to_string(),
        "session_id": run.session_id.to_string(),
        "state": run_state_name(run.state),
        "attempt_id": run.attempt_id.to_string(),
        "control_revision": run.control_revision,
    })
}

/// `wait`: poll until the run blocks or reaches a terminal state.
pub(super) async fn wait<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    session_id: AgentSessionId,
    until: WaitUntil,
    timeout_secs: u64,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut delay = WAIT_MIN_DELAY;
    loop {
        let (_, projection) = session_projection(&mut caller, session_id).await?;
        let Some(active) = projection.active_run.as_ref() else {
            // A session without a run is not working, so neither mode has a
            // state to observe; the deadline reports that plainly.
            return wait_deadline(caller, session_id, until, timeout_secs);
        };
        let state = active.run.state;
        let blocked = matches!(
            state,
            loom_protocol::AgentRunState::AwaitingApproval
                | loom_protocol::AgentRunState::NeedsInput
        );
        let terminal = matches!(
            state,
            loom_protocol::AgentRunState::Completed
                | loom_protocol::AgentRunState::Failed
                | loom_protocol::AgentRunState::Cancelled
        );
        // A blocked run never reaches a terminal state without a person, so
        // both modes report it at once instead of holding the caller for a
        // decision it was told not to make.
        if blocked || terminal {
            let revision = revision(&mut caller, active.run.id).await?;
            let tool_call_id = active.pending_approval.as_ref().map(|call| call.id);
            let code = if terminal {
                match state {
                    loom_protocol::AgentRunState::Completed => exit::OK,
                    _ => exit::INTERNAL,
                }
            } else {
                exit::BLOCKED
            };
            if !invocation.globals.json {
                emit(
                    out,
                    &format!("run {} {}\n", active.run.id, run_state_name(state)),
                )?;
                if blocked {
                    emit(
                        out,
                        &format!(
                            "blocked {} {}\n",
                            active.run.attempt_id,
                            tool_call_id
                                .map(|id| id.to_string())
                                .unwrap_or_else(|| "-".to_owned())
                        ),
                    )?;
                }
            }
            let data = json!({
                "session": session_id.to_string(),
                "until": until_name(until),
                "run": run_value(&active.run),
                "state": run_state_name(state),
                "attempt_id": active.run.attempt_id.to_string(),
                "tool_call_id": tool_call_id.map(|id| id.to_string()),
                "revision": revision,
                "blocked": blocked,
                "terminal": terminal,
            });
            return Ok(Outcome::new(data, caller.primary).with_code(code));
        }
        let now = Instant::now();
        if now >= deadline {
            return wait_deadline(caller, session_id, until, timeout_secs);
        }
        let remaining = deadline.saturating_duration_since(now);
        tokio::time::sleep(delay.min(remaining)).await;
        delay = (delay * 2).min(WAIT_MAX_DELAY);
    }
}

/// The timeout failure `wait` reports when the deadline passes first.
fn wait_deadline<C: Client>(
    caller: Caller<'_, C>,
    session_id: AgentSessionId,
    until: WaitUntil,
    timeout_secs: u64,
) -> Result<Outcome, Failure> {
    Err(Failure::Timeout {
        request_id: caller.primary,
        message: format!(
            "session {session_id} did not reach {} within {timeout_secs}s",
            until_name(until)
        ),
    })
}

fn until_name(until: WaitUntil) -> &'static str {
    match until {
        WaitUntil::Blocked => "blocked",
        WaitUntil::Terminal => "terminal",
    }
}

/// `status`: the session and its latest run, derived into a declaration.
pub(super) async fn status<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    session_id: AgentSessionId,
    format: StatusFormat,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let (_, projection) = session_projection(&mut caller, session_id).await?;
    let run = projection.active_run.as_ref().map(|active| &active.run);
    let declaration = declaration(run.map(|run| run.state));
    let state = run.map(|run| run_state_name(run.state)).unwrap_or("idle");
    if !invocation.globals.json {
        match format {
            StatusFormat::Verb => emit(
                out,
                &format!(
                    "{declaration}: {state} {}\n",
                    run.map(|run| run.id.to_string())
                        .unwrap_or_else(|| "-".to_owned())
                ),
            )?,
            StatusFormat::Text => {
                emit(
                    out,
                    &format!(
                        "session {} {} {}\n",
                        projection.session.id,
                        session_state_name(projection.session.state),
                        escape(&projection.session.name)
                    ),
                )?;
                if let Some(active) = &projection.active_run {
                    emit(
                        out,
                        &format!(
                            "run {} {}\n",
                            active.run.id,
                            run_state_name(active.run.state)
                        ),
                    )?;
                    if let Some(pending) = &active.pending_approval {
                        emit(out, &format!("pending {}\n", pending.id))?;
                    }
                }
            }
        }
    }
    let data = json!({
        "session": projection.session,
        "run": projection.active_run.as_ref().map(|active| run_value(&active.run)),
        "state": state,
        "verdict": declaration,
        "declaration": format!(
            "{declaration}: {state} {}",
            run.map(|run| run.id.to_string())
                .unwrap_or_else(|| "-".to_owned())
        ),
        "pending_tool_call": projection
            .active_run
            .as_ref()
            .and_then(|active| active.pending_approval.as_ref().map(|call| call.id.to_string())),
    });
    Ok(Outcome::new(data, caller.primary))
}

/// The declaration word derived from a run state. No worker writes this; the
/// client derives it, and `status --format verb` prints it.
fn declaration(state: Option<loom_protocol::AgentRunState>) -> &'static str {
    use loom_protocol::AgentRunState;
    match state {
        None | Some(AgentRunState::Completed) => "done",
        Some(AgentRunState::Planning)
        | Some(AgentRunState::Executing)
        | Some(AgentRunState::Evaluating) => "working",
        Some(AgentRunState::AwaitingApproval) | Some(AgentRunState::NeedsInput) => "blocked",
        Some(AgentRunState::Paused) => "paused",
        Some(AgentRunState::Failed) | Some(AgentRunState::Cancelled) => "failed",
    }
}

/// `liveness`: whether the session is alive, dead, gone, or unknown.
pub(super) async fn liveness<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    session_id: AgentSessionId,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let response = session_projection(&mut caller, session_id).await;
    let (verdict, state, code) = match response {
        Ok((_, projection)) => {
            let state = projection.session.state;
            let verdict = if matches!(
                state,
                AgentSessionState::Archived
                    | AgentSessionState::Completed
                    | AgentSessionState::Failed
                    | AgentSessionState::Cancelled
            ) {
                "dead"
            } else {
                "alive"
            };
            (verdict, Some(state), exit::OK)
        }
        // A transport failure never becomes `gone`: the worker was not reached,
        // so its answer is unknown rather than a missing session.
        Err(Failure::Transport { .. }) => ("unknown", None, exit::UNREACHABLE),
        Err(Failure::Server { error, .. }) if error.code == ErrorCode::NotFound => {
            ("gone", None, exit::GONE)
        }
        Err(other) => return Err(other),
    };
    if !invocation.globals.json {
        emit(out, &format!("liveness {verdict} {session_id}\n"))?;
    }
    let data = json!({
        "session": session_id.to_string(),
        "verdict": verdict,
        "state": state.map(session_state_name),
    });
    Ok(Outcome::new(data, caller.primary).with_code(code))
}

/// `result`: branch, head, diffstat, changed paths, and task evidence.
pub(super) async fn result<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    session_id: AgentSessionId,
    task: Option<loom_core::TaskId>,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let (request_id, response) = caller
        .call(ClientRequest::Repository(
            RepositoryRequest::ListSessionRepositories { session_id },
        ))
        .await?;
    let repositories = match response {
        ServerResponse::Repository(RepositoryResponse::SessionRepositories { repositories }) => {
            repositories
        }
        _ => return Err(Failure::unexpected(request_id, "session repositories")),
    };
    let mut reported = Vec::new();
    for repository in &repositories {
        reported.push(repository_result(&mut caller, session_id, repository).await?);
    }
    let evidence = match task {
        Some(task_id) => {
            let (request_id, response) = caller
                .call(ClientRequest::Task(TaskRequest::GetSessionTaskEvidence {
                    session_id,
                    task_id,
                }))
                .await?;
            match response {
                ServerResponse::Task(TaskResponse::TaskEvidence { evidence }) => evidence,
                _ => return Err(Failure::unexpected(request_id, "task evidence")),
            }
        }
        None => Vec::new(),
    };
    let empty = reported.is_empty() && evidence.is_empty();
    if !invocation.globals.json {
        emit(out, &format!("session {session_id}\n"))?;
        if empty {
            emit(out, "nothing to report\n")?;
        }
        for repository in &reported {
            emit(
                out,
                &format!(
                    "repository {} {}\n",
                    repository["id"].as_str().unwrap_or("-"),
                    escape(repository["path"].as_str().unwrap_or("-"))
                ),
            )?;
            emit(
                out,
                &format!("branch {}\n", repository["branch"].as_str().unwrap_or("-")),
            )?;
            emit(
                out,
                &format!("head {}\n", repository["head"].as_str().unwrap_or("-")),
            )?;
            emit(
                out,
                &format!(
                    "diffstat {} {} {}\n",
                    repository["diffstat"]["files"],
                    repository["diffstat"]["added"],
                    repository["diffstat"]["removed"]
                ),
            )?;
            for changed in repository["changed_paths"].as_array().into_iter().flatten() {
                emit(
                    out,
                    &format!(
                        "changed {} {}\n",
                        changed["status"].as_str().unwrap_or("unknown"),
                        escape(changed["path"].as_str().unwrap_or("-"))
                    ),
                )?;
            }
        }
        if task.is_some() && evidence.is_empty() {
            emit(out, "evidence none\n")?;
        }
        for link in &evidence {
            emit(
                out,
                &format!("evidence {} {}\n", escape(&link.label), escape(&link.uri)),
            )?;
        }
    }
    let data = json!({
        "session": session_id.to_string(),
        "repositories": reported,
        "evidence": evidence,
        "empty": empty,
    });
    Ok(Outcome::new(data, caller.primary))
}

/// The status and diff of one attached repository, as `result` reports it.
async fn repository_result<C: Client>(
    caller: &mut Caller<'_, C>,
    session_id: AgentSessionId,
    repository: &SessionRepository,
) -> Result<Value, Failure> {
    let (request_id, response) = caller
        .call(ClientRequest::Repository(
            RepositoryRequest::GetSessionVcsStatus {
                session_id,
                repository_id: repository.id,
            },
        ))
        .await?;
    let status = match response {
        ServerResponse::Repository(RepositoryResponse::VcsStatus(status)) => status,
        _ => return Err(Failure::unexpected(request_id, "repository status")),
    };
    let (request_id, response) = caller
        .call(ClientRequest::Repository(
            RepositoryRequest::GetSessionVcsDiff {
                session_id,
                repository_id: repository.id,
                path: None,
                staged: false,
            },
        ))
        .await?;
    let diff = match response {
        ServerResponse::Repository(RepositoryResponse::VcsDiff(diff)) => diff,
        _ => return Err(Failure::unexpected(request_id, "repository diff")),
    };
    let added = diff
        .hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .filter(|line| line.kind == loom_core::GitDiffLineKind::Added)
        .count();
    let removed = diff
        .hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .filter(|line| line.kind == loom_core::GitDiffLineKind::Removed)
        .count();
    let changed_paths = status
        .files
        .iter()
        .map(|file| {
            json!({
                "path": file.path,
                "status": file_status_name(file.worktree),
                "conflicted": file.conflicted,
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "id": repository.id.to_string(),
        "path": repository.path,
        "source": repository.source,
        "branch": status.branch,
        "head": status.head,
        "clean": status.clean,
        "changed_paths": changed_paths,
        "diffstat": {
            "files": status.files.len(),
            "added": added,
            "removed": removed,
        },
        "diff_truncated": diff.truncated,
    }))
}

fn file_status_name(kind: GitFileStatusKind) -> &'static str {
    match kind {
        GitFileStatusKind::Added => "added",
        GitFileStatusKind::Modified => "modified",
        GitFileStatusKind::Deleted => "deleted",
        GitFileStatusKind::Renamed => "renamed",
        GitFileStatusKind::Copied => "copied",
        GitFileStatusKind::Untracked => "untracked",
        GitFileStatusKind::Ignored => "ignored",
        GitFileStatusKind::Conflicted => "conflicted",
        GitFileStatusKind::Unknown => "unknown",
    }
}

/// The metadata projection of one session.
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
