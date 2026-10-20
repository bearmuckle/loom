//! `events` and `peek`: the session event stream in text or JSON lines.
//!
//! `events` follows the worker's own paging cursor (`next_after_sequence`) until
//! a batch reaches the journal head, and `--follow` then keeps asking, backing
//! off from 250 ms to 2 s while nothing new arrives, until a `run_completed`
//! event is printed. Every printed event advances a floor sequence that is
//! persisted to the `--cursor-file`, so a resumed run prints exactly the suffix
//! it has not printed before.

use std::{io::Write, path::Path, time::Duration};

use loom_core::{EventSequence, RequestId};
use loom_protocol::{
    ActivityDetail, ClientRequest, EventsRequest, EventsResponse, ServerEvent, ServerEventEnvelope,
    ServerResponse,
};
use serde_json::{Value, json};

use crate::ctl::args::{EventsArgs, Format, Invocation, PeekArgs};
use crate::ctl::client::Client;
use crate::ctl::cursor::{self, Cursor};
use crate::ctl::text::{self, Rendered};

use super::{Caller, Failure, emit, handshake};

/// How long `--follow` waits before asking again when nothing new arrived.
const FOLLOW_MIN_DELAY: Duration = Duration::from_millis(250);
/// The longest `--follow` waits between polls.
const FOLLOW_MAX_DELAY: Duration = Duration::from_secs(2);

/// `events`: a session's journal, from the `--after` sequence or the cursor.
pub(super) async fn events<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    args: &EventsArgs,
    out: &mut dyn Write,
) -> Result<(Value, Option<RequestId>), Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller).await?;
    let url = invocation.globals.connect.clone().unwrap_or_default();
    let json_output = invocation.globals.json;

    // A stored cursor is adopted only when it names this worker and session;
    // an explicit `--after` wins over it.
    let stored = match &args.cursor_file {
        Some(path) => cursor::read(path, &url, args.session).map_err(Failure::Internal)?,
        None => None,
    };
    let stored_after = stored.as_ref().and_then(|cursor| cursor.after);
    let mut epoch = stored
        .as_ref()
        .and_then(|cursor| cursor.stream_epoch.clone());
    let mut floor = args.after.or(stored_after);
    let mut after = floor.map(EventSequence::new);

    let mut rendered = Vec::new();
    let mut followed_run_completed = false;
    let mut delay = FOLLOW_MIN_DELAY;
    loop {
        let (request_id, response) = caller
            .call(ClientRequest::Events(EventsRequest::GetSessionEvents {
                activity_detail: ActivityDetail::Full,
                session_id: Some(args.session),
                workspace_id: None,
                after_sequence: after,
                stream_epoch: epoch.clone(),
            }))
            .await?;
        let page = match response {
            ServerResponse::Events(EventsResponse::SessionEvents {
                events,
                stream_epoch,
                next_after_sequence,
            }) => Page {
                events,
                stream_epoch,
                next: next_after_sequence,
                latest: None,
            },
            ServerResponse::Events(EventsResponse::SessionEventsSnapshot {
                events,
                latest_sequence,
                stream_epoch,
                ..
            }) => Page {
                events,
                stream_epoch,
                next: None,
                latest: Some(latest_sequence),
            },
            _ => return Err(Failure::unexpected(request_id, "session events")),
        };

        // A new stream epoch means the sequence numbers no longer name the same
        // events, so the stored position is discarded before anything prints.
        if let (Some(previous), Some(current)) = (epoch.as_deref(), page.stream_epoch.as_deref())
            && previous != current
        {
            floor = None;
        }
        epoch = page.stream_epoch.clone();

        let mut printed = 0usize;
        for envelope in &page.events {
            let sequence = envelope.sequence.value();
            if floor.is_some_and(|floor| sequence <= floor) {
                continue;
            }
            let projected = text::render(&envelope.event, args.max_bytes);
            emit_event(
                out,
                &mut rendered,
                json_output,
                args.format,
                envelope,
                page.stream_epoch.as_deref(),
                &projected,
            )?;
            floor = Some(sequence);
            printed += 1;
            if matches!(
                &envelope.event,
                ServerEvent::Agent {
                    event: loom_protocol::AgentEvent::RunCompleted { .. }
                }
            ) {
                followed_run_completed = true;
            }
        }

        if let Some(latest) = page.latest {
            floor = Some(floor.map_or_else(|| latest.value(), |floor| floor.max(latest.value())));
        }
        if let Some(path) = &args.cursor_file {
            write_cursor(path, &url, args.session, floor, epoch.clone())?;
        }

        match page.next {
            Some(next) => {
                after = Some(next);
                delay = FOLLOW_MIN_DELAY;
                continue;
            }
            None => {
                after = floor.map(EventSequence::new);
                if !args.follow || followed_run_completed {
                    break;
                }
                if printed == 0 {
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(FOLLOW_MAX_DELAY);
                } else {
                    delay = FOLLOW_MIN_DELAY;
                }
            }
        }
    }

    Ok((json!({ "events": rendered }), caller.primary))
}

/// `peek`: the newest events of a session, bounded by `--lines`.
pub(super) async fn peek<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    args: &PeekArgs,
    out: &mut dyn Write,
) -> Result<(Value, Option<RequestId>), Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller).await?;
    let (request_id, response) = caller
        .call(ClientRequest::Events(
            EventsRequest::GetRecentSessionEvents {
                session_id: args.session,
                limit: args.lines,
                activity_detail: ActivityDetail::Full,
            },
        ))
        .await?;
    let (events, stream_epoch) = match response {
        ServerResponse::Events(EventsResponse::SessionEvents {
            events,
            stream_epoch,
            ..
        }) => (events, stream_epoch),
        _ => return Err(Failure::unexpected(request_id, "recent session events")),
    };
    let mut rendered = Vec::new();
    for envelope in &events {
        let projected = text::render(&envelope.event, args.max_bytes);
        emit_event(
            out,
            &mut rendered,
            invocation.globals.json,
            args.format,
            envelope,
            stream_epoch.as_deref(),
            &projected,
        )?;
    }
    Ok((json!({ "events": rendered }), caller.primary))
}

/// One response's worth of events, normalised over both batch shapes.
struct Page {
    events: Vec<ServerEventEnvelope>,
    stream_epoch: Option<String>,
    next: Option<EventSequence>,
    latest: Option<EventSequence>,
}

/// Prints one event as its text line or its JSON line, and records the JSON form
/// for a `--json` envelope.
#[allow(clippy::too_many_arguments)]
fn emit_event(
    out: &mut dyn Write,
    rendered: &mut Vec<Value>,
    json_output: bool,
    format: Format,
    envelope: &ServerEventEnvelope,
    stream_epoch: Option<&str>,
    projected: &Rendered,
) -> Result<(), Failure> {
    let session_id = envelope.session_id.to_string();
    let object = text::json_line(
        envelope.sequence,
        stream_epoch,
        &session_id,
        projected,
        &envelope.event,
    );
    if json_output {
        rendered.push(object);
        return Ok(());
    }
    match format {
        Format::Text => emit(out, &text::line(envelope.sequence, projected)),
        Format::Jsonl => emit(out, &format!("{object}\n")),
    }
}

fn write_cursor(
    path: &Path,
    url: &str,
    session: loom_core::AgentSessionId,
    floor: Option<u64>,
    stream_epoch: Option<String>,
) -> Result<(), Failure> {
    cursor::write(
        path,
        url,
        session,
        &Cursor {
            after: floor,
            stream_epoch,
        },
    )
    .map_err(Failure::Internal)
}
