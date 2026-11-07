//! The `jev` verbs: supervision of a decision model this client drives itself.
//!
//! `jev status` and `jev ask` are local: they read the key file and call the
//! model, so they run without a worker, a token, or a negotiated capability.
//! `jev triage` reads one session's pending approval from the worker, asks the
//! model for a verdict, and prints it; `--apply` approves the call only when
//! every guard in [`crate::cli::jev::guard`] passes, so a suggestion alone never
//! changes anything.
//!
//! The key is read here, after parsing, and is sent only in the `Authorization`
//! header. No text line, envelope, or failure message carries it.

use std::io::Write;

use loom_core::{AgentSessionId, ErrorCode, RunControlAction};
use loom_model::ToolCall;
use loom_protocol::{
    AgentSessionSnapshotProjection, ClientRequest, CollaborationRequest, FilesystemRequest,
    FilesystemResponse, RunActionOutcome, ServerResponse,
};
use serde_json::{Value, json};

use crate::cli::args::{Invocation, JevAskArgs, JevFlags, JevTriageArgs};
use crate::cli::client::Client;
use crate::cli::jev::{self, Answer, GuardRefusal, Settings, USER_AGENT};
use crate::cli::text::{bound, escape, run_state_name};

use super::{Caller, Failure, Outcome, emit, handshake, supervise};

/// The most of one tool call's arguments the triage state carries. The model is
/// asked for a judgement about the shape of the call, not to read a whole file.
const MAX_ARGUMENTS_BYTES: usize = 2000;
/// The most of a session name the triage state carries.
const MAX_NAME_BYTES: usize = 200;
/// The instruction of the triage question, which states the two labels.
const TRIAGE_INSTRUCTIONS: &str = "The agent is parked on the tool call described by the state, and a person must \
     decide whether it may run. Approve it only when the call is plainly a read of the \
     workspace the state names and nothing suggests it touches anything else; otherwise a \
     person should look first.";

/// `jev status`: what a call would use, without making one.
pub(super) async fn status<C: Client>(
    _client: &mut C,
    invocation: &Invocation,
    flags: &JevFlags,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let settings = settings(flags)?;
    let configured = settings.key_configured();
    if !invocation.globals.json {
        emit(out, &format!("source {}\n", settings.source.name()))?;
        emit(out, &format!("endpoint {}\n", escape(&settings.endpoint)))?;
        emit(out, &format!("model {}\n", escape(&settings.model)))?;
        emit(
            out,
            &format!(
                "key {} {}\n",
                if configured { "configured" } else { "missing" },
                escape(&settings.key_file.to_string_lossy())
            ),
        )?;
    }
    Ok(Outcome::new(
        json!({
            "source": settings.source.name(),
            "source_explicit": settings.source_explicit,
            "endpoint": settings.endpoint,
            "model": settings.model,
            "key_file": settings.key_file.to_string_lossy(),
            "key_configured": configured,
            "user_agent": USER_AGENT,
        }),
        None,
    ))
}

/// `jev ask`: one raw choice question, with its confidence and probabilities.
pub(super) async fn ask<C: Client>(
    _client: &mut C,
    invocation: &Invocation,
    args: &JevAskArgs,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let settings = settings(&args.flags)?;
    let key = settings.key().map_err(Failure::Usage)?;
    let answer = call(&settings, &key, &args.state, &args.question, &args.options).await?;
    if !invocation.globals.json {
        emit(
            out,
            &format!(
                "choice {} {}\n",
                escape(&answer.choice),
                number(answer.confidence)
            ),
        )?;
        for (label, probability) in &answer.probabilities {
            emit(
                out,
                &format!("probability {} {probability}\n", escape(label)),
            )?;
        }
        emit(out, &format!("model {}\n", escape(&answer.model)))?;
    }
    Ok(Outcome::new(
        json!({
            "choice": answer.choice,
            "confidence": answer.confidence,
            "probabilities": answer
                .probabilities
                .iter()
                .map(|(label, probability)| json!({"label": label, "probability": probability}))
                .collect::<Vec<_>>(),
            "model": answer.model,
            "source": settings.source.name(),
            "endpoint": settings.endpoint,
        }),
        None,
    ))
}

/// `jev triage`: a suggested verdict for the approval one session waits on.
///
/// The suggestion is always printed. `--apply` is the only way it acts, and it
/// acts only on an `approve` verdict that clears the threshold and the read-only
/// guard, through the same revision-checked control request `approve` sends.
pub(super) async fn triage<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    args: &JevTriageArgs,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    // The key is read before the worker is asked anything, so a missing key is
    // reported as the local usage error it is.
    let settings = settings(&args.flags)?;
    let key = settings.key().map_err(Failure::Usage)?;

    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let (request_id, projection) = supervise::session_projection(&mut caller, args.session).await?;
    let run = projection
        .active_run
        .as_ref()
        .ok_or_else(|| Failure::Server {
            request_id,
            error: loom_core::LoomError::invalid_state(format!(
                "session {} has no run, so it waits on no approval",
                args.session
            )),
        })?;
    let call = run
        .pending_approval
        .as_ref()
        .ok_or_else(|| Failure::Server {
            request_id,
            error: loom_core::LoomError::invalid_state(format!(
                "session {} has no pending approval to triage",
                args.session
            )),
        })?;
    let run_id = run.run.id;
    let tool_call_id = call.id;
    let root = session_root(&mut caller, args.session).await?;
    let refusal = jev::guard(&call.name, &call.arguments, &root).err();

    let state = triage_state(&projection, call, &root, refusal.is_none());
    let answer = call_decision(&settings, &key, &state).await?;
    let verdict = verdict_word(&answer)?;
    let skipped = skip_reason(verdict, &answer, refusal, args);
    let text = !invocation.globals.json;
    if text {
        emit(
            out,
            &format!(
                "session {} tool {} call {}\n",
                args.session,
                escape(&call.name),
                tool_call_id
            ),
        )?;
        emit(
            out,
            &format!("verdict {verdict} {}\n", number(answer.confidence)),
        )?;
    }
    let mut applied = false;
    let mut run_after = Value::Null;
    // `--apply` is the caller's explicit permission and a skip is the guard's
    // refusal: only with the first and without the second is anything approved.
    if args.apply && skipped.is_none() {
        let outcome = supervise::act(
            &mut caller,
            run_id,
            true,
            args.request_id,
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
        let RunActionOutcome::Accepted { run: accepted, .. } = outcome else {
            unreachable!("a conflict is reported by `act`")
        };
        applied = true;
        run_after = json!({
            "id": accepted.id.to_string(),
            "state": run_state_name(accepted.state),
        });
        if text {
            emit(
                out,
                &format!(
                    "apply approved {} {}\n",
                    accepted.id,
                    run_state_name(accepted.state)
                ),
            )?;
        }
    } else if text {
        match skipped {
            Some(reason) => emit(out, &format!("apply skipped {reason}\n"))?,
            None => emit(out, "apply not-requested\n")?,
        }
    }
    Ok(Outcome::new(
        json!({
            "session": args.session.to_string(),
            "run": run_id.to_string(),
            "run_state": run_state_name(run.run.state),
            "tool_call": tool_call_id.to_string(),
            "tool_name": call.name,
            "arguments": call.arguments,
            "session_root": root,
            "verdict": verdict,
            "confidence": answer.confidence,
            "probabilities": answer
                .probabilities
                .iter()
                .map(|(label, probability)| json!({"label": label, "probability": probability}))
                .collect::<Vec<_>>(),
            "model": answer.model,
            "source": settings.source.name(),
            "endpoint": settings.endpoint,
            "threshold": args.threshold,
            "read_tool": refusal != Some(GuardRefusal::NotAReadTool),
            "paths_inside_root": refusal != Some(GuardRefusal::PathOutsideRoot),
            "applied": applied,
            "skipped": skipped,
            "run_after": run_after,
        }),
        caller.primary,
    ))
}

/// The verdict word of one answer: `approve` only for that exact label, so a
/// label this client does not know can never be mistaken for permission.
fn verdict_word(answer: &Answer) -> Result<&'static str, Failure> {
    match answer.choice.as_str() {
        "approve" => Ok("approve"),
        "ask_user" => Ok("ask_user"),
        other => Err(Failure::Provider(loom_core::LoomError::new(
            ErrorCode::ProviderInvalidResponse,
            format!(
                "the decision model answered '{other}', not approve or ask_user; refusing the \
                 answer instead of reading it as a verdict"
            ),
            false,
        ))),
    }
}

/// Why `--apply` may not approve the call, or `None` when it may.
fn skip_reason(
    verdict: &str,
    answer: &Answer,
    refusal: Option<GuardRefusal>,
    args: &JevTriageArgs,
) -> Option<&'static str> {
    if !args.apply {
        return None;
    }
    if verdict != "approve" {
        return Some("verdict-ask-user");
    }
    if !answer.clears(args.threshold) {
        return Some("below-threshold");
    }
    refusal.map(GuardRefusal::word)
}

/// The session root the worker reports for one session.
async fn session_root<C: Client>(
    caller: &mut Caller<'_, C>,
    session_id: AgentSessionId,
) -> Result<String, Failure> {
    let (request_id, response) = caller
        .call(ClientRequest::Filesystem(
            FilesystemRequest::GetSessionFilesystemSnapshot { session_id },
        ))
        .await?;
    match response {
        ServerResponse::Filesystem(FilesystemResponse::SessionFilesystemSnapshot(snapshot)) => {
            Ok(snapshot.root)
        }
        _ => Err(Failure::unexpected(
            request_id,
            "session filesystem snapshot",
        )),
    }
}

/// The bounded state one triage question sends: the call, its arguments, and the
/// workspace it would run in. The guard's verdict is stated as one fact rather
/// than as a conclusion, so the model still answers the question it is asked.
fn triage_state(
    projection: &AgentSessionSnapshotProjection,
    call: &ToolCall,
    root: &str,
    inside_root: bool,
) -> String {
    let run_state = projection
        .active_run
        .as_ref()
        .map(|run| run_state_name(run.run.state))
        .unwrap_or("blocked");
    format!(
        "A Loom agent run is parked on a tool call and waits for a person to approve or deny it.\n\
         Session: {name}\n\
         Run state: {run_state}\n\
         Tool: {tool}\n\
         Everything the call names stays inside the session workspace: {inside}\n\
         Session root: {root}\n\
         Tool arguments: {arguments}\n",
        name = bound(&projection.session.name, MAX_NAME_BYTES),
        tool = escape(&call.name),
        inside = if inside_root { "yes" } else { "no" },
        root = escape(root),
        arguments = bound(&call.arguments.to_string(), MAX_ARGUMENTS_BYTES),
    )
}

/// The triage question: one choice between approving and asking a person.
async fn call_decision(settings: &Settings, key: &str, state: &str) -> Result<Answer, Failure> {
    let options = [
        (
            "approve".to_owned(),
            "the call plainly reads the session workspace and nothing suggests an irreversible or \
             external effect"
                .to_owned(),
        ),
        (
            "ask_user".to_owned(),
            "the call writes, runs a command, reaches the network, touches anything outside the \
             session workspace, or the state is too thin to tell"
                .to_owned(),
        ),
    ];
    call(settings, key, state, TRIAGE_INSTRUCTIONS, &options).await
}

/// One decision call, with its failures mapped onto this client's failures: a
/// request the service rejects is the caller's own input, and everything else is
/// a provider failure that keeps the service's code.
async fn call(
    settings: &Settings,
    key: &str,
    state: &str,
    instructions: &str,
    options: &[(String, String)],
) -> Result<Answer, Failure> {
    jev::ask(settings, key, state, instructions, options)
        .await
        .map_err(|error| match error.code {
            ErrorCode::InvalidRequest => Failure::Usage(error.message),
            _ => Failure::Provider(error),
        })
}

/// The settings of one `jev` command, resolved once from the flags and `HOME`.
fn settings(flags: &JevFlags) -> Result<Settings, Failure> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    jev::select(flags, home.as_deref()).map_err(Failure::Usage)
}

/// One number as text, with an absent value as `-`.
fn number(value: Option<f64>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| value.to_string())
}
