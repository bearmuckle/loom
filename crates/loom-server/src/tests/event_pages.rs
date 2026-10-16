//! In-process tests: bounded session-event pages.
//!
//! One events response is bounded to a single frame, so a backlog larger than
//! that arrives as pages a caller follows. These tests drive the bound through the
//! request path, with the connection's response byte budget set the same way the
//! remote listener sets it from its configured frame limit.

use super::*;
use loom_core::RunId;
use loom_protocol::{encode_event, encode_response};

/// One bounded page as the backend answered it.
struct Page {
    events: Vec<loom_protocol::ServerEventEnvelope>,
    next_after_sequence: Option<EventSequence>,
    /// Bytes the response occupied as a WebSocket frame, measured from the
    /// encoded response the backend produced.
    frame_bytes: usize,
}

/// A backend with one workspace and one session, because an event stream is
/// scoped to a session.
fn workspace_and_session(label: &str) -> (Arc<InProcessBackend>, AgentSessionId) {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace_id = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: format!("{label} workspace"),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace.id,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let session_id = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id,
                name: format!("{label} session"),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };
    (backend, session_id)
}

fn session_events_request(
    session_id: AgentSessionId,
    after_sequence: Option<EventSequence>,
) -> RequestEnvelope {
    RequestEnvelope::new(ClientRequest::Events(EventsRequest::GetSessionEvents {
        session_id: Some(session_id),
        workspace_id: None,
        after_sequence,
        stream_epoch: None,
    }))
}

/// Requests session events after `after_sequence` and returns the page.
fn request_page(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
    after_sequence: Option<EventSequence>,
) -> Page {
    let response = connection.request(session_events_request(session_id, after_sequence));
    let frame_bytes = encode_response(&response)
        .expect("an events response should encode")
        .len();
    match response.result {
        Ok(ServerResponse::Events(EventsResponse::SessionEvents {
            events,
            next_after_sequence,
            ..
        })) => Page {
            events,
            next_after_sequence,
            frame_bytes,
        },
        response => panic!("expected a session events page, got {response:?}"),
    }
}

/// The failure one session events request is answered with.
fn request_page_error(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
    after_sequence: Option<EventSequence>,
) -> LoomError {
    connection
        .request(session_events_request(session_id, after_sequence))
        .result
        .expect_err("the events request should fail")
}

/// Appends one assistant delta event and returns its sequence.
fn append_delta(
    backend: &InProcessBackend,
    session_id: AgentSessionId,
    text: String,
) -> EventSequence {
    let mut journal = backend.journal().unwrap();
    journal.append_agent(
        session_id,
        AgentEvent::AssistantMessageDelta {
            run_id: RunId::new(),
            message_id: 1,
            text,
        },
    );
    journal.next_sequence
}

/// Every session event sequence the journal holds, oldest first.
fn journaled_sequences(
    backend: &InProcessBackend,
    session_id: AgentSessionId,
) -> Vec<EventSequence> {
    backend
        .journal()
        .unwrap()
        .events_since(Some(session_id), None)
        .into_iter()
        .map(|event| event.sequence)
        .collect()
}

#[test]
fn a_batch_that_fits_one_response_is_returned_unchanged() {
    let (backend, session_id) = workspace_and_session("fits");
    for index in 0..3 {
        append_delta(&backend, session_id, format!("delta {index}"));
    }
    let connection = backend.connect();
    negotiate(&connection);

    let page = request_page(&connection, session_id, None);
    assert_eq!(
        page.events.len(),
        4,
        "the session creation and three deltas"
    );
    assert_eq!(
        page.next_after_sequence, None,
        "a batch that reached the latest event names no page to follow"
    );
    assert_eq!(
        journaled_sequences(&backend, session_id),
        page.events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>()
    );
}

#[test]
fn a_resync_above_the_response_bound_is_delivered_in_followable_pages() {
    const LIMIT: usize = 2048;
    let (backend, session_id) = workspace_and_session("bounded");
    for index in 0..40 {
        append_delta(
            &backend,
            session_id,
            "d".repeat(160) + &format!("{index:03}"),
        );
    }
    let connection = backend.connect().with_max_response_bytes(LIMIT);
    negotiate(&connection);

    let mut cursor = None;
    let mut collected = Vec::new();
    let mut pages = 0;
    loop {
        let page = request_page(&connection, session_id, cursor);
        assert!(
            !page.events.is_empty(),
            "every page must make progress towards the latest event"
        );
        assert!(
            page.frame_bytes <= LIMIT,
            "a page of {} bytes is above the configured {LIMIT}-byte frame limit",
            page.frame_bytes
        );
        for event in &page.events {
            assert!(
                cursor.is_none_or(|cursor| event.sequence > cursor),
                "page repeats sequence {}",
                event.sequence
            );
            collected.push(event.sequence);
        }
        pages += 1;
        match page.next_after_sequence {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }

    // The bound changes how the batch travels, not what it contains: the pages
    // followed to the latest journaled event carry the whole backlog.
    assert!(pages > 1, "the backlog should not fit one page");
    assert_eq!(collected, journaled_sequences(&backend, session_id));
}

#[test]
fn a_batch_above_the_event_count_bound_is_paged_without_a_frame_limit() {
    let (backend, session_id) = workspace_and_session("count");
    for index in 0..MAX_EVENT_BATCH_EVENTS + 10 {
        append_delta(&backend, session_id, format!("delta {index}"));
    }
    // An in-process connection has no frame to fit, so only the event count
    // bounds the batch; a caller follows the pages either way.
    let connection = backend.connect();
    negotiate(&connection);

    let mut cursor = None;
    let mut collected = Vec::new();
    let mut pages = 0;
    loop {
        let page = request_page(&connection, session_id, cursor);
        assert!(
            page.events.len() <= MAX_EVENT_BATCH_EVENTS,
            "a page carries at most {MAX_EVENT_BATCH_EVENTS} events"
        );
        collected.extend(page.events.iter().map(|event| event.sequence));
        pages += 1;
        match page.next_after_sequence {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(pages, 2);
    assert_eq!(collected, journaled_sequences(&backend, session_id));
}

#[test]
fn a_recent_events_batch_keeps_its_caller_sized_tail() {
    let (backend, session_id) = workspace_and_session("recent");
    for index in 0..10 {
        append_delta(&backend, session_id, format!("delta {index}"));
    }
    let connection = backend.connect();
    negotiate(&connection);

    // A recent batch is a tail the caller sized with `limit`, not a batch that
    // grows with the journal, so it is answered with the newest events and names
    // no page to follow.
    let response = connection.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetRecentSessionEvents {
            session_id,
            limit: 3,
        },
    )));
    let ServerResponse::Events(EventsResponse::SessionEvents {
        events,
        next_after_sequence,
        ..
    }) = response.result.unwrap()
    else {
        panic!("expected a recent events batch");
    };
    assert_eq!(events.len(), 3);
    assert_eq!(next_after_sequence, None);
    assert_eq!(
        events.last().map(|event| event.sequence),
        journaled_sequences(&backend, session_id).last().copied()
    );
}

#[test]
fn an_event_above_the_response_bound_is_reported_instead_of_dropped() {
    const LIMIT: usize = 2048;
    let (backend, session_id) = workspace_and_session("unfit");
    let created = journaled_sequences(&backend, session_id)[0];
    let before = append_delta(&backend, session_id, "small".to_owned());
    let unfit = append_delta(&backend, session_id, "h".repeat(4096));
    let after = append_delta(&backend, session_id, "small again".to_owned());
    let connection = backend.connect().with_max_response_bytes(LIMIT);
    negotiate(&connection);

    // A page stops before the event it cannot fit, so the caller reaches that
    // event as the first event of the next page instead of losing it silently.
    let page = request_page(&connection, session_id, None);
    assert_eq!(
        page.events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![created, before]
    );
    assert_eq!(page.next_after_sequence, Some(before));

    // The page that would start with the unfit event answers with a diagnostic
    // naming that event and its size.
    let error = request_page_error(&connection, session_id, Some(before));
    let unfit_envelope = backend
        .journal()
        .unwrap()
        .events_since(Some(session_id), Some(before))
        .into_iter()
        .next()
        .expect("the unfit event should be journaled");
    assert_eq!(unfit_envelope.sequence, unfit);
    let encoded_size = encode_event(&unfit_envelope)
        .expect("the unfit event should encode")
        .len();
    assert_eq!(error.code, ErrorCode::PayloadTooLarge);
    assert!(
        !error.retryable,
        "raising the limit is the only fix: {error}"
    );
    assert!(
        error.message.contains(&unfit.to_string()),
        "the diagnostic must name the event: {error}"
    );
    assert!(
        error.message.contains(&encoded_size.to_string()),
        "the diagnostic must name the event size {encoded_size}: {error}"
    );
    assert!(
        error.message.contains(&LIMIT.to_string()),
        "the diagnostic must name the configured limit: {error}"
    );

    // The event after it still arrives, so one unfit event does not hide the
    // rest of the journal.
    let recovered = request_page(&connection, session_id, Some(unfit));
    assert_eq!(
        recovered
            .events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![after]
    );
}

/// A session events request whose stream epoch the backend never had.
///
/// One way a cursor is treated as stale is an epoch the connection no longer
/// belongs to, which is what makes the backend answer with the snapshot fallback
/// — the response a client takes when the incremental path cannot be served.
fn stale_epoch_request(
    session_id: AgentSessionId,
    after_sequence: Option<EventSequence>,
) -> RequestEnvelope {
    RequestEnvelope::new(ClientRequest::Events(EventsRequest::GetSessionEvents {
        session_id: Some(session_id),
        workspace_id: None,
        after_sequence,
        stream_epoch: Some("an-epoch-this-backend-never-had".to_owned()),
    }))
}

#[test]
fn a_snapshot_above_the_response_bound_keeps_its_newest_events() {
    const LIMIT: usize = 2048;
    let (backend, session_id) = workspace_and_session("snapshot-bound");
    for index in 0..40 {
        append_delta(
            &backend,
            session_id,
            "d".repeat(160) + &format!("{index:03}"),
        );
    }
    let connection = backend.connect().with_max_response_bytes(LIMIT);
    negotiate(&connection);

    let response = connection.request(stale_epoch_request(session_id, None));
    let frame_bytes = encode_response(&response)
        .expect("the snapshot response should encode")
        .len();
    let ServerResponse::Events(EventsResponse::SessionEventsSnapshot {
        events,
        oldest_sequence,
        latest_sequence,
        ..
    }) = response.result.unwrap()
    else {
        panic!("the stale epoch should be answered with the snapshot fallback");
    };

    let journaled = journaled_sequences(&backend, session_id);
    assert_eq!(latest_sequence, *journaled.last().unwrap());
    assert!(
        !events.is_empty(),
        "a bounded snapshot still carries its newest events"
    );
    // The window keeps the newest events, so what it carries is the tail of the
    // journal and `oldest_sequence` names the first one carried rather than the
    // oldest one the journal retains: the caller rebuilds the older part from
    // durable state, which is why dropping that side is safe.
    let carried = events
        .iter()
        .map(|event| event.sequence)
        .collect::<Vec<_>>();
    assert_eq!(
        carried,
        journaled[journaled.len() - carried.len()..].to_vec()
    );
    assert_eq!(oldest_sequence, carried[0]);
    assert!(
        oldest_sequence > journaled[0],
        "the whole retained window must not be what was sent: {oldest_sequence} vs {}",
        journaled[0]
    );
    assert!(
        frame_bytes <= LIMIT,
        "a snapshot of {frame_bytes} bytes is above the configured {LIMIT}-byte frame limit"
    );
}

#[test]
fn an_event_that_cannot_fit_a_snapshot_is_reported_by_name() {
    const LIMIT: usize = 2048;
    let (backend, session_id) = workspace_and_session("snapshot-unfit");
    append_delta(&backend, session_id, "small".to_owned());
    let unfit = append_delta(&backend, session_id, "h".repeat(4096));
    let connection = backend.connect().with_max_response_bytes(LIMIT);
    negotiate(&connection);

    // The newest event is the one a snapshot cannot leave out, so this is the
    // one case where the diagnostic is the only honest answer: sending the
    // response would let the reader drop the event without saying which one.
    let error = connection
        .request(stale_epoch_request(session_id, None))
        .result
        .expect_err("a snapshot whose newest event cannot fit must be reported");
    let encoded_size = encode_event(
        &backend
            .journal()
            .unwrap()
            .events_since(Some(session_id), None)
            .into_iter()
            .find(|event| event.sequence == unfit)
            .expect("the unfit event should be journaled"),
    )
    .expect("the unfit event should encode")
    .len();
    assert_eq!(error.code, ErrorCode::PayloadTooLarge);
    assert!(
        !error.retryable,
        "raising the limit is the only fix: {error}"
    );
    assert!(
        error.message.contains(&unfit.to_string()),
        "the diagnostic must name the event: {error}"
    );
    assert!(
        error.message.contains(&encoded_size.to_string()),
        "the diagnostic must name the event size {encoded_size}: {error}"
    );
    assert!(
        error.message.contains(&LIMIT.to_string()),
        "the diagnostic must name the configured limit: {error}"
    );
}

#[test]
fn a_workspace_feed_above_the_response_bound_is_delivered_in_followable_pages() {
    const LIMIT: usize = 2048;
    let backend = InProcessBackend::new();
    let setup = backend.connect();
    negotiate(&setup);
    let workspace_id = match setup
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "feed bound workspace".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace.id,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let session_id = match setup
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id,
                name: "feed bound session".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };
    // The feed belongs to a workspace with a session in it, which is what makes
    // the session events part of it.
    assert!(
        backend
            .sessions()
            .unwrap()
            .list_in_workspace(Some(workspace_id), true)
            .iter()
            .any(|session| session.id == session_id)
    );

    // Long names, so the feed crosses the bound with a handful of events.
    let mut appended = Vec::new();
    for index in 0..40 {
        appended.push(backend.journal().unwrap().append_workspace(
            workspace_id,
            loom_protocol::WorkspaceEvent::Renamed {
                name: format!("{}{index:03}", "n".repeat(200)),
            },
        ));
    }

    let connection = backend.connect().with_max_response_bytes(LIMIT);
    negotiate(&connection);
    let mut cursor = Some(EventSequence::default());
    let mut collected: Vec<EventSequence> = Vec::new();
    let mut pages = 0;
    loop {
        let response = connection.request(RequestEnvelope::new(ClientRequest::Events(
            EventsRequest::GetSessionEvents {
                session_id: None,
                workspace_id: Some(workspace_id),
                after_sequence: cursor,
                stream_epoch: None,
            },
        )));
        let frame_bytes = encode_response(&response)
            .expect("a feed page should encode")
            .len();
        let ServerResponse::Events(EventsResponse::WorkspaceEvents {
            events,
            next_after_sequence,
            ..
        }) = response.result.unwrap()
        else {
            panic!("the feed should be answered with bounded pages");
        };
        assert!(
            !events.is_empty(),
            "every page must make progress towards the latest feed event"
        );
        assert!(
            frame_bytes <= LIMIT,
            "a feed page of {frame_bytes} bytes is above the configured {LIMIT}-byte frame limit"
        );
        for event in &events {
            let sequence = match event {
                loom_protocol::WorkspaceFeedEvent::Session(event) => event.sequence,
                loom_protocol::WorkspaceFeedEvent::Workspace(event) => event.sequence,
            };
            assert!(
                cursor.is_none_or(|cursor| sequence > cursor),
                "page repeats sequence {sequence}"
            );
            collected.push(sequence);
        }
        pages += 1;
        match next_after_sequence {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }

    assert!(pages > 1, "the feed should not fit one page");
    assert!(
        collected.windows(2).all(|pair| pair[1] > pair[0]),
        "the pages must carry the feed in sequence order without repeating"
    );
    for sequence in &appended {
        assert!(
            collected.contains(sequence),
            "the followed pages must carry every event, missing {sequence}"
        );
    }
}
