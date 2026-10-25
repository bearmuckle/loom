//! `events` and `peek`: the session event stream in text or JSON lines.
//!
//! `events` follows the worker's own paging cursor (`next_after_sequence`) until
//! a batch reaches the journal head, and `--follow` then keeps asking, backing
//! off from 250 ms to 2 s while nothing new arrives, until a `run_completed`
//! event is printed. Every printed event advances a floor sequence that is
//! persisted to the `--cursor-file`, so a resumed run prints exactly the suffix
//! it has not printed before.
//!
//! A snapshot cannot page: the worker bounds it to its newest events, so a
//! cursor older than that window loses the events between the two. The verb
//! does not pretend otherwise - it prints the window, reports the missing range
//! as a `gap` on stderr and in the `--json` envelope, and resumes at the head,
//! because a later snapshot would carry the same window again. Only a fresh
//! start without a cursor has no gap: starting from the current window is the
//! documented behaviour of a first read.

use std::{io::Write, path::Path, time::Duration};

use loom_core::{EventSequence, RequestId};
use loom_protocol::{
    ActivityDetail, ClientRequest, EventsRequest, EventsResponse, ServerEvent, ServerEventEnvelope,
    ServerResponse,
};
use serde_json::{Value, json};

use crate::cli::args::{EventsArgs, Format, Invocation, PeekArgs};
use crate::cli::client::Client;
use crate::cli::cursor::{self, Cursor};
use crate::cli::text::{self, Rendered};

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
    err: &mut dyn Write,
) -> Result<(Value, Option<RequestId>), Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
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
    let mut gap: Option<Gap> = None;
    let mut followed_run_completed = false;
    let mut delay = FOLLOW_MIN_DELAY;
    loop {
        let requested_after = after;
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
                oldest: None,
            },
            ServerResponse::Events(EventsResponse::SessionEventsSnapshot {
                session: _,
                events,
                oldest_sequence,
                latest_sequence,
                stream_epoch,
            }) => Page {
                events,
                stream_epoch,
                next: None,
                latest: Some(latest_sequence),
                oldest: Some(oldest_sequence),
            },
            _ => return Err(Failure::unexpected(request_id, "session events")),
        };

        // A new stream epoch means the sequence numbers no longer name the same
        // events, so the stored position is discarded before anything prints.
        let epoch_changed = match (epoch.as_deref(), page.stream_epoch.as_deref()) {
            (Some(previous), Some(current)) => previous != current,
            _ => false,
        };
        if epoch_changed {
            floor = None;
        }
        epoch = page.stream_epoch.clone();

        // A snapshot is bounded to its newest events. When its window starts
        // above the cursor, the events in between are not in the response and a
        // later snapshot cannot page them back, so say so instead of resuming
        // silently past them. A fresh start has no cursor to compare against,
        // and a new epoch makes the old sequence meaningless.
        if let (Some(oldest), Some(cursor), false) = (
            page.oldest.map(EventSequence::value),
            requested_after.map(EventSequence::value),
            epoch_changed,
        ) && oldest > cursor.saturating_add(1)
        {
            let found = Gap {
                after: cursor,
                before: oldest,
            };
            gap = Some(match gap {
                Some(previous) => previous.merge(found),
                None => found,
            });
            writeln!(
                err,
                "warning gap {}..{}: {} event(s) between the cursor and the snapshot window are \
                 no longer available; the cursor resumes at the head of the window",
                cursor.saturating_add(1),
                oldest.saturating_sub(1),
                found.missing()
            )
            .map_err(|error| Failure::Internal(format!("could not write a warning: {error}")))?;
        }

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

    let gap = gap.map(|gap| {
        json!({
            "after": gap.after,
            "before": gap.before,
            "missing": gap.missing(),
        })
    });
    Ok((json!({ "events": rendered, "gap": gap }), caller.primary))
}

/// `peek`: the newest events of a session, bounded by `--lines`.
pub(super) async fn peek<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    args: &PeekArgs,
    out: &mut dyn Write,
    _err: &mut dyn Write,
) -> Result<(Value, Option<RequestId>), Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
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
    /// The head a snapshot resumes at; absent for a pageable batch.
    latest: Option<EventSequence>,
    /// The first sequence a snapshot actually carried; absent for a batch,
    /// whose oldest event was requested explicitly.
    oldest: Option<EventSequence>,
}

/// The range a truncated snapshot could not carry: `after` is the cursor the
/// request resumed from and `before` is the first sequence the window carried,
/// so the missing sequences are `after + 1 .. before`. Merging keeps the widest
/// range when a follow loop resynchronises more than once.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Gap {
    after: u64,
    before: u64,
}

impl Gap {
    fn missing(self) -> u64 {
        self.before.saturating_sub(self.after).saturating_sub(1)
    }

    fn merge(self, other: Self) -> Self {
        Self {
            after: self.after.min(other.after),
            before: self.before.max(other.before),
        }
    }
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
    if json_output {
        rendered.push(text::json_line(
            envelope.sequence,
            stream_epoch,
            &envelope.session_id.to_string(),
            projected,
            &envelope.event,
        ));
        return Ok(());
    }
    match format {
        Format::Text => emit(out, &text::line(envelope.sequence, projected)),
        Format::Jsonl => emit(
            out,
            &format!(
                "{}\n",
                text::json_line(
                    envelope.sequence,
                    stream_epoch,
                    &envelope.session_id.to_string(),
                    projected,
                    &envelope.event,
                )
            ),
        ),
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
