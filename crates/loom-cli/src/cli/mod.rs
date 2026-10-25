//! `loom-cli`: the command-line client contract.
//!
//! # Surface
//!
//! Global flags are accepted anywhere on the command line and in both the
//! `--flag value` and `--flag=value` forms. The first non-flag token names the
//! verb.
//!
//! | verb | requests |
//! | --- | --- |
//! | `doctor` | `Control/DiscoverCapabilities`, `Control/Negotiate`, `Access/GetCurrentParticipant`, `Control/GetWorkerNodeStatus` |
//! | `node` | `Control/GetWorkerNodeStatus` |
//! | `whoami` | `Access/GetCurrentParticipant` |
//! | `ls` | `Workspace/ListWorkspaces`, or `Workspace/ListWorkspaceSessions` with `--workspace` |
//! | `get` | `Session/GetAgentSession`, plus `Session/GetAgentSessionSnapshotMetadata` with `--metadata` |
//! | `events` | `Events/GetSessionEvents` |
//! | `peek` | `Events/GetRecentSessionEvents` |
//! | `run` | `Control/Negotiate`, `Workspace/ListWorkspaceSessions`, `Workspace/CreateAgentSessionInWorkspace`, `Repository/AttachSessionRepository`, `Filesystem/AttachSessionDirectory`, `Session/SetSessionApprovalPolicy`, `Run/StartSessionAgentRunWithOptions` |
//! | `send` | `Collaboration/GetRunRevision`, `Collaboration/DirectAgent` |
//! | `answer` | `Collaboration/GetRunRevision`, `Collaboration/AnswerAgentQuestion` |
//! | `approve`, `deny` | `Collaboration/GetRunRevision`, `Collaboration/ControlRun` with `Approve`/`Reject` |
//! | `stop` | `Collaboration/GetRunRevision`, `Collaboration/ControlRun` with `Interrupt`, plus `Session/ArchiveAgentSession` with `--archive` |
//! | `wait` | `Collaboration/GetRunRevision`, `Session/GetAgentSessionSnapshotMetadata` polling |
//! | `status` | `Session/GetAgentSessionSnapshotMetadata`, plus `Project/GetProjectSnapshotForSession` and one `Session/GetAgentSessionSnapshotMetadata` per blocked agent with `--tree` |
//! | `liveness` | `Session/GetAgentSessionSnapshotMetadata` |
//! | `result` | `Repository/ListSessionRepositories`, `Repository/GetSessionVcsStatus`, `Repository/GetSessionVcsDiff`, `Task/GetSessionTaskEvidence` |
//! | `retention get` | `Control/GetArchiveRetentionPolicy` |
//! | `retention set` | `Control/GetArchiveRetentionPolicy`, `Control/SetArchiveRetentionPolicy` |
//! | `delete` | `Project/GetProjectSnapshotForSession`, `Session/DeleteAgentSession` |
//! | `session archive` | `Project/GetProjectSnapshotForSession`, `Session/ArchiveAgentSession` |
//! | `session delete` | `Project/GetProjectSnapshotForSession`, `Session/DeleteAgentSession` |
//! | `project archive` | `Project/GetProjectSnapshotForSession`, `Session/ArchiveAgentSession` |
//! | `project delete` | `Project/GetProjectSnapshotForSession`, `Session/DeleteAgentSession` |
//! | `project worktree cleanup` | `Project/GetProjectSnapshotForSession`, `Project/CleanupProjectChildWorktree` |
//!
//! Every verb first sends `Control/Negotiate` with the version this build
//! speaks and with only the capabilities it uses, because a worker refuses an
//! unnegotiated connection. `node`, `whoami`, `ls`, `get`, `events`, `peek`,
//! and `retention get` are the read-only verbs: they send nothing that mutates
//! and negotiate only the capabilities their reads need, so a read-only-scoped
//! token can run them. `retention set` and `delete` are operator-level
//! mutations: a token without `configure_archive_retention` or
//! `delete_agent_session` is refused at negotiation with exit `3`.
//!
//! `session archive`, `session delete` (and its `delete` alias), `project
//! archive`, `project delete`, and `project worktree cleanup` are the
//! operator-level lifecycle verbs, and they are separate operations: archiving
//! moves a session to the archived state and keeps its transcript, checkpoints,
//! and every checkout, deletion removes the durable state, and worktree cleanup
//! decides the fate of one delegated task's checkout. Each one reads
//! `Project/GetProjectSnapshotForSession` before it mutates anything, because an
//! id alone does not say whether it names a project root whose operation reaches
//! other sessions.
//!
//! `session archive` accepts any session and requires `--yes` when the id is a
//! project root whose archive the worker cascades to its live members, so the
//! blast radius is one explicit decision. `project archive` and `project delete`
//! require the id to name such a root and spell the scope themselves; `project
//! delete` keeps the `--yes` requirement because deletion is irreversible and
//! `--force` still has to be confirmed. The worker stays the authority on which
//! sessions may change state: archiving a project root while a child task is
//! unfinished or a child session is active is refused as `invalid_state` (exit
//! `5`), and a deletion of a live child is cancelled by the worker rather than
//! by this client.
//!
//! `project worktree cleanup` resolves the project, the manager session, and the
//! worktree record from the snapshot by `--task`, so the caller never supplies
//! an id the worker did not report. The disposition is explicit: `--keep`
//! retains the checkout, `--remove` removes it and is refused while it holds
//! changes, and `--discard` removes it with those changes and therefore also
//! needs `--yes`. A checkout is never removed without one of those flags.
//!
//! `node` prints one line per field of `WorkerNodeStatus`, with absent health
//! samples as `unknown`. `retention get` prints `window`, `force-discard`, and
//! `sub-agent`, with a disabled window as `off`. `retention set` merges the
//! flags it was given into the policy the worker already stores (a flag that is
//! absent keeps its stored value), and reports the stored result followed by
//! `stored for this worker; the next sweep applies it`: changing the policy
//! never starts a sweep, and a session already archived is deleted by that next
//! round rather than by the command.
//!
//! `delete` reads `Project/GetProjectSnapshotForSession` before it sends the
//! mutation, because the id alone does not say whether it names a project root
//! that cascades. A root with another member or a delegated task removes its
//! whole tree, so `--yes` is required when that tree is larger than the target
//! itself; `--force` requires `--yes` as well, because it discards dirty or
//! locked worktrees. Both checks happen before `Session/DeleteAgentSession` is
//! sent. The worker remains the authority on which session states may be
//! deleted: a non-archived session that is not a project root is refused with
//! `invalid_state` (exit `5`) and nothing changes.
//!
//! `run` starts a run, and it is described here because its requests are the
//! longest sequence the client sends:
//!
//! 1. `Workspace/ListWorkspaceSessions { include_archived: false }` resolves the
//!    workspace's live sessions. The first session whose name equals the
//!    resolved name (`--name`, or one derived from the task) is reused;
//!    otherwise the session is created with
//!    `Workspace/CreateAgentSessionInWorkspace`. `--new` skips the listing
//!    entirely and always creates a session, so a name is a label rather than
//!    an identity.
//! 2. The inputs are attached **only when this invocation created the session**:
//!    `--repo` sends `Repository/AttachSessionRepository` and `--as-directory`
//!    sends `Filesystem/AttachSessionDirectory`. A replay of the same command
//!    therefore neither duplicates an attachment nor fails on one that already
//!    exists.
//! 3. `Session/SetSessionApprovalPolicy` pins `--approval-policy` before the run
//!    starts, because a run resolves its policy once, when it is created. A
//!    session this invocation creates is pinned to the requested preset, or the
//!    default one when the flag is absent; a reused session is only changed when
//!    the flag was given, so a plain run never overwrites a person's choice.
//! 4. `Run/StartSessionAgentRunWithOptions` starts the run under the caller's
//!    `--request-id` (or a freshly generated id), so repeating the same id is
//!    answered from the worker's durable idempotency cache with the same run
//!    instead of starting a second one. The idempotency key is the run start
//!    alone; the session creation, the attachments, and the policy change use
//!    fresh ids.
//!
//! The verb returns as soon as `AgentRunStarted` arrives: it never polls and
//! never waits for the run, and `events --follow` is the verb that watches one.
//! Its text output is exactly two lines, `session <session-id> <session-name>`
//! and `run <run-id> <run-state>`, and it prints neither when the run did not
//! start.
//!
//! # Control verbs
//!
//! `send`, `answer`, `approve`, and `deny` read `Collaboration/GetRunRevision`,
//! send their collaboration request with that revision, and on `Conflict`
//! re-read once and retry, because their intent is idempotent. `stop` sends
//! `Interrupt` and refuses the same conflict unless `--retry-on-conflict` is
//! given, so a stale revision cannot interrupt a run the caller has not seen.
//! A conflict that survives the allowed retry is reported with the protocol's
//! `conflict` code, which maps to exit `1` because it is neither a usage error
//! nor a missing resource.
//!
//! `wait` polls `Session/GetAgentSessionSnapshotMetadata`, and reads
//! `Collaboration/GetRunRevision` once it has something to report. It is always
//! time-bounded: `--timeout` (60 seconds by default) ends the poll with
//! `deadline_exceeded` and exit `4`. A run parked on an approval or a question
//! is reported at once with exit `3`, and the `--json` envelope carries the
//! attempt id and, for an approval, the tool call id. The timeout therefore
//! covers only a run that is still working, never a person's decision.
//!
//! `status` derives its declaration from the run state: `working` for planning,
//! executing, and evaluating, `blocked` for awaiting approval and needs input,
//! `paused`, `done` for completed, and `failed` for failed and cancelled. The
//! line is derived by this client from the run state; no worker writes it.
//!
//! `status --tree` additionally projects the agent session's project: the
//! project verdict, its agents with a verdict each, its delegated tasks, its
//! worktrees with their conflict paths, and one decision per agent waiting on an
//! approval or a question. The records come from
//! `Project/GetProjectSnapshotForSession` unchanged; this client derives the
//! verdict words, which are the `status` word set plus `archived`, and reads the
//! metadata of every agent whose project record is `awaiting_approval` or
//! `needs_input` to learn which decision it waits on. The requested session
//! reuses its own projection, so its pending decision appears once as a
//! `decision` line instead of the plain `pending` line.
//!
//! `liveness` never guesses: an archived or terminal session is `dead`, a
//! session the worker does not know is `gone` with exit `5`, a transport
//! failure is `unknown` with exit `7`, and anything else is `alive`.
//!
//! # `loom-text/1`
//!
//! Text output is a byte-stable projection: one physical line per event,
//! `<seq> <kind> <body>\n`. `<seq>` is the decimal `EventSequence`, `<kind>` is
//! a fixed lowercase word, and `<body>` is escaped so it can never contain a
//! raw newline: `\\`, `\n`, `\r`, and `\t` become their two-character
//! escapes, other C0 controls and `0x7f` become `\xNN`, and every other UTF-8
//! byte is passed through unchanged. An empty body leaves no trailing space, so
//! the line is `<seq> <kind>\n`.
//!
//! Fields of a body that are empty are omitted, which is how
//! `run_completed` reports a run without a summary. `tool_done` carries the
//! outcome word and, when the tool produced one, its bounded output.
//!
//! Text bounding: a string longer than the byte budget keeps its first and last
//! half, rounded to UTF-8 character boundaries, with the exact marker
//! `[... N bytes omitted ...]` between them, where N is the number of omitted
//! bytes. The budget of a verb's `--max-bytes` applies to tool output, tool
//! arguments, and tool results, so one line never grows unbounded with a tool
//! payload. Bounding runs on the raw text before escaping, so the omitted count
//! is a count of the original bytes. Replaying the same events with the same
//! budget produces identical bytes.
//!
//! Nothing in the projection uses Rust's `Debug` formatting: every variant
//! name is an explicit match arm spelled exactly like the protocol's snake_case
//! serde name, so a wire change cannot silently change the text contract.
//!
//! # `--json`
//!
//! `--json` prints exactly one envelope on stdout. Success is
//! `{"ok":true,"verb":...,"request_id":...,"data":...}` with the requested
//! payload in `data`; failure is
//! `{"ok":false,"verb":...,"request_id":...,"error":{"code":...,"message":...,"retryable":...,"exit_code":...}}`.
//! `request_id` is the id of the verb's primary request (its first request, or
//! its handshake), or `null` when no request was sent. `run` reports its run
//! start instead, because that is the id its caller chose and the key a replay
//! reuses. `events` adds `gap` (`after`, `before`, `missing`) when a snapshot
//! could not carry every event after the cursor, and `null` otherwise.
//!
//! `events --format jsonl` and `peek --format jsonl` are a different shape:
//! without `--json` they print one object per event,
//! `{"seq":...,"stream_epoch":...,"session_id":...,"kind":...,"text":...,"event":...}`,
//! where `kind` and `text` are exactly the text projection of that event.
//!
//! A verb whose documented exit code reports something other than success is
//! still a successful determination: a blocked `wait` is `ok:true` with exit
//! `3`, and a `liveness` verdict of `gone` or `unknown` is `ok:true` with exit
//! `5` or `7`. The `data` payload carries the verdict, state, attempt id, and
//! tool call id, so a caller never has to parse the message.
//!
//! # Exit codes
//!
//! `0` ok, `1` internal, `2` usage, `3` blocked (`approval_required`,
//! `capability_denied`, `authorization_denied`, `workspace_access_denied`), `4`
//! timeout (`deadline_exceeded`, `backpressure`), `5` not found or gone
//! (`not_found`, `invalid_state`), `6` protocol or authentication mismatch
//! (`unsupported_protocol`, `authentication_required`,
//! `authentication_failed`), and `7` transport unreachable, meaning a
//! connection or transport failure before or during a request. Every other
//! error maps to `1`.
//!
//! # Cursor file
//!
//! `events --cursor-file <path>` keeps
//! `{"version":1,"url":...,"session_id":...,"after":...,"stream_epoch":...}`
//! so a later run resumes without reprinting history. A cursor written for a
//! different URL or session is ignored. The file is replaced atomically (a
//! temporary file in the same directory, then a rename) after every fully
//! rendered batch. An event whose sequence is at or below the persisted cursor
//! is never printed; a `SessionEventsSnapshot` response prints the window it
//! carries, reports any range between the cursor and that window as `gap` (on
//! stderr and in the `--json` envelope) because a snapshot cannot page it back,
//! and then persists `latest_sequence` so a later run resumes at the head; and a
//! changed `stream_epoch` discards the stored cursor first, because a new epoch
//! means the sequences no longer name the same events.
//!
//! # Token handling
//!
//! The bearer token may only come from a file: `--token-file <path>` or
//! `LOOM_TOKEN_FILE`. `--token` is rejected as a usage error that says so,
//! because a token on a command line is visible to every process on the node.
//! No diagnostic, envelope, or log line ever carries the token.

mod args;
mod client;
mod cursor;
mod exit;
mod text;
mod verbs;

#[cfg(test)]
mod tests;

use std::io::Write;

pub use client::{Client, RemoteClient};

use args::{Invocation, Parsed};

/// Entry point of the `loom-cli` binary: parses the arguments, connects, runs
/// the verb, and maps every outcome to an exit code. A usage error never
/// panics on user input.
pub fn main(arguments: Vec<String>) -> i32 {
    let json = args::wants_json(&arguments);
    let mut invocation = match args::parse(&arguments) {
        Ok(Parsed::Help) => return print_help(),
        Ok(Parsed::Run(invocation)) => invocation,
        Err(message) => return report_usage_stdio(&message, json, args::named_verb(&arguments)),
    };
    // The environment is consulted exactly once, here, beside the token file
    // fallback, and the result is stored on the invocation so the URL a cursor
    // file is scoped to is the one the connection uses. Parsing itself stays a
    // pure function of its arguments.
    invocation.globals.connect = args::resolve_connect(
        invocation.globals.connect.take(),
        std::env::var("LOOM_URL").ok(),
    );
    let settings = &invocation.globals;
    let Some(url) = settings.connect.clone() else {
        return report_usage_stdio(
            "a worker URL is required: pass --connect <url> or set LOOM_URL",
            settings.json,
            args::verb_name(&invocation.verb),
        );
    };
    let token_path = settings
        .token_file
        .clone()
        .or_else(|| std::env::var_os("LOOM_TOKEN_FILE").map(Into::into));
    let Some(token_path) = token_path else {
        return report_usage_stdio(
            "a bearer token is required: pass --token-file <path> or set LOOM_TOKEN_FILE",
            settings.json,
            args::verb_name(&invocation.verb),
        );
    };
    let token = match std::fs::read_to_string(&token_path) {
        Ok(token) => token.trim().to_owned(),
        Err(error) => {
            let message = format!(
                "could not read the token file '{}': {error}",
                token_path.display()
            );
            return report_usage_stdio(&message, settings.json, args::verb_name(&invocation.verb));
        }
    };
    if token.is_empty() {
        return report_usage_stdio(
            "the token file is empty",
            settings.json,
            args::verb_name(&invocation.verb),
        );
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let mut out = std::io::stdout();
            let mut err = std::io::stderr();
            let failure =
                verbs::Failure::Internal(format!("could not start the async runtime: {error}"));
            return report_failure(
                &failure,
                settings.json,
                args::verb_name(&invocation.verb),
                &mut out,
                &mut err,
            );
        }
    };
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let code = runtime.block_on(async {
        let mut transport = loom_local::WebSocketTransport::new(url, token)
            .with_insecure_remote_allowed(settings.allow_insecure_remote);
        if let Some(max_frame_bytes) = settings.max_frame_bytes {
            transport = match transport.with_max_frame_bytes(max_frame_bytes) {
                Ok(transport) => transport,
                Err(error) => {
                    return report_failure(
                        &verbs::Failure::Usage(error.message),
                        settings.json,
                        args::verb_name(&invocation.verb),
                        &mut out,
                        &mut err,
                    );
                }
            };
        }
        if let Some(ca_file) = &settings.ca_file {
            transport = match transport.with_ca_certificate_file(ca_file) {
                Ok(transport) => transport,
                Err(error) => {
                    return report_failure(
                        &verbs::Failure::Usage(error.message),
                        settings.json,
                        args::verb_name(&invocation.verb),
                        &mut out,
                        &mut err,
                    );
                }
            };
        }
        match transport.connect().await {
            Ok(connection) => {
                let mut client = RemoteClient::new(connection);
                // The verb runs through the same entry point the tests use, so
                // there is one execution path for both, and the arguments were
                // parsed once, so a task file is read once.
                run_parsed(&mut client, &invocation, &mut out, &mut err).await
            }
            // The worker was never reached: an authentication refusal is a
            // protocol or auth mismatch, and anything else is unreachable.
            Err(error) => {
                let code = exit::for_connect_error(&error);
                report_transport(
                    &error,
                    code,
                    settings.json,
                    args::verb_name(&invocation.verb),
                    &mut out,
                    &mut err,
                )
            }
        }
    });
    let _ = out.flush();
    let _ = err.flush();
    code
}

/// Runs one verb against an already connected client, returning its exit code
/// without ever exiting the process. This is the seam the tests drive with an
/// in-process backend; the binary parses once and calls [`run_parsed`].
#[cfg(test)]
pub async fn run_command<C: Client>(
    client: &mut C,
    arguments: &[String],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let json = args::wants_json(arguments);
    match args::parse(arguments) {
        Ok(Parsed::Help) => {
            let _ = out.write_all(args::HELP.as_bytes());
            exit::OK
        }
        Ok(Parsed::Run(invocation)) => run_parsed(client, &invocation, out, err).await,
        Err(message) => report_usage(&message, json, args::named_verb(arguments), out, err),
    }
}

/// Runs an already parsed invocation, so a caller that needed the parsed flags
/// before connecting does not parse (and read a task file) twice.
pub async fn run_parsed<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    run_invocation(client, invocation, out, err).await
}

async fn run_invocation<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let json = invocation.globals.json;
    let verb = args::verb_name(&invocation.verb);
    match verbs::run(client, invocation, out, err).await {
        Ok(outcome) => {
            if json {
                let envelope = serde_json::json!({
                    "ok": true,
                    "verb": verb,
                    "request_id": outcome.request_id.map(|id| id.to_string()),
                    "data": outcome.data,
                });
                write_json(out, &envelope);
            }
            outcome.exit_code
        }
        Err(failure) => report_failure(&failure, json, verb, out, err),
    }
}

fn print_help() -> i32 {
    let mut out = std::io::stdout();
    let _ = out.write_all(args::HELP.as_bytes());
    let _ = out.flush();
    exit::OK
}

/// Reports a usage error, in the JSON envelope when `--json` was asked for and
/// on stderr otherwise.
fn report_usage(
    message: &str,
    json: bool,
    verb: &str,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    report_failure(
        &verbs::Failure::Usage(message.to_owned()),
        json,
        verb,
        out,
        err,
    )
}

/// Reports a usage error found before any client exists, on the process streams.
fn report_usage_stdio(message: &str, json: bool, verb: &str) -> i32 {
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let code = report_usage(message, json, verb, &mut out, &mut err);
    let _ = out.flush();
    let _ = err.flush();
    code
}

fn report_failure(
    failure: &verbs::Failure,
    json: bool,
    verb: &str,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    // A reader that closed the pipe is not a failure and has nowhere to read a
    // report, so stop quietly with the successful code.
    if matches!(failure, verbs::Failure::BrokenPipe) {
        return exit::OK;
    }
    let code = failure.exit_code();
    if json {
        write_json(out, &failure.envelope(verb));
    } else {
        let _ = writeln!(err, "error {} {}", failure.code(), failure.message());
    }
    code
}

fn report_transport(
    error: &loom_core::LoomError,
    code: i32,
    json: bool,
    verb: &str,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    if json {
        write_json(
            out,
            &verbs::Failure::Transport {
                request_id: None,
                error: error.clone(),
            }
            .envelope_with_code(verb, code),
        );
    } else {
        let _ = writeln!(err, "error {} {}", error.code, error.message);
    }
    code
}

pub(crate) fn write_json(out: &mut dyn Write, value: &serde_json::Value) {
    let _ = writeln!(out, "{value}");
}
