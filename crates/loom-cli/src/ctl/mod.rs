//! `loom-ctl`: the read-only command-line client contract.
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
//! | `whoami` | `Access/GetCurrentParticipant` |
//! | `ls` | `Workspace/ListWorkspaces`, or `Workspace/ListWorkspaceSessions` with `--workspace` |
//! | `get` | `Session/GetAgentSession`, plus `Session/GetAgentSessionSnapshotMetadata` with `--metadata` |
//! | `events` | `Events/GetSessionEvents` |
//! | `peek` | `Events/GetRecentSessionEvents` |
//!
//! Every verb first sends `Control/Negotiate` with the version this build
//! speaks and with only the capabilities the client uses, because a worker
//! refuses an unnegotiated connection. Nothing else is sent, so `ls`, `get`,
//! `events`, and `peek` issue no mutation and leave the worker's journal and
//! session snapshots untouched.
//!
//! `run`, and every other mutating verb, is out of scope here. The parser is
//! structured so a verb with side effects can be added without touching the
//! read-only ones.
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
//! its handshake), or `null` when no request was sent.
//!
//! `events --format jsonl` and `peek --format jsonl` are a different shape:
//! without `--json` they print one object per event,
//! `{"seq":...,"stream_epoch":...,"session_id":...,"kind":...,"text":...,"event":...}`,
//! where `kind` and `text` are exactly the text projection of that event.
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
//! is never printed; a `SessionEventsSnapshot` response prints only events
//! above it and then persists `latest_sequence`; and a changed `stream_epoch`
//! discards the stored cursor first, because a new epoch means the sequences no
//! longer name the same events.
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

/// Entry point of the `loom-ctl` binary: parses the arguments, connects, runs
/// the verb, and maps every outcome to an exit code. A usage error never
/// panics on user input.
pub fn main(arguments: Vec<String>) -> i32 {
    let json = args::wants_json(&arguments);
    let invocation = match args::parse(&arguments) {
        Ok(Parsed::Help) => return print_help(),
        Ok(Parsed::Run(invocation)) => invocation,
        Err(message) => return report_usage_stdio(&message, json, args::named_verb(&arguments)),
    };
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
        let mut transport = loom_server::WebSocketTransport::new(url, token)
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
                // there is one execution path for both.
                run_command(&mut client, &arguments, &mut out, &mut err).await
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
/// in-process backend; the binary itself calls the same runner after it
/// connects.
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
        Ok(Parsed::Run(invocation)) => run_invocation(client, &invocation, out, err).await,
        Err(message) => report_usage(&message, json, args::named_verb(arguments), out, err),
    }
}

async fn run_invocation<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let json = invocation.globals.json;
    let verb = args::verb_name(&invocation.verb);
    match verbs::run(client, invocation, out).await {
        Ok((data, request_id)) => {
            if json {
                let envelope = serde_json::json!({
                    "ok": true,
                    "verb": verb,
                    "request_id": request_id.map(|id| id.to_string()),
                    "data": data,
                });
                write_json(out, &envelope);
            }
            exit::OK
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
