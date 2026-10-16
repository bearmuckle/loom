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
