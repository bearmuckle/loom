//! In-process backend integration tests built on the shared `support` fixture.
//!
//! These exercise the negotiated session/event/run surface through the public
//! protocol rather than reaching into backend internals.

mod support;

use loom_core::{ErrorCode, RunId};
use loom_protocol::{
    ClientRequest, EventsRequest, EventsResponse, RequestEnvelope, RunRequest, ServerEvent,
    ServerResponse, SessionRequest, SessionResponse, WorkspaceRequest,
};
use loom_server::InProcessBackend;

#[test]
fn creates_session_and_reads_event_stream() {
    let (_, connection) = support::fixture();
    let workspace = support::create_workspace(&connection, "In-process workspace");
    let session_id = support::create_session(&connection, workspace.id, "In-process demo");

    let events = connection.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetSessionEvents {
            session_id: Some(session_id),
            workspace_id: None,
            after_sequence: None,
            stream_epoch: None,
        },
    )));
    let ServerResponse::Events(EventsResponse::SessionEvents { events, .. }) =
        events.result.unwrap()
    else {
        panic!("unexpected response");
    };
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].session_id, session_id);

    let initial = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::GetAgentSessionInitialState { session_id },
    )));
    let ServerResponse::Session(SessionResponse::AgentSessionInitialState(initial)) =
        initial.result.unwrap()
    else {
        panic!("unexpected initial state response");
    };
    assert_eq!(initial.cursor, events[0].sequence);

    let renamed = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::RenameAgentSession {
            session_id,
            name: "Renamed after snapshot".to_owned(),
        },
    )));
    assert!(matches!(
        renamed.result,
        Ok(ServerResponse::Session(
            SessionResponse::AgentSessionRenamed(_)
        ))
    ));

    let resumed = connection.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetSessionEvents {
            session_id: Some(session_id),
            workspace_id: None,
            after_sequence: Some(initial.cursor),
            stream_epoch: None,
        },
    )));
    let ServerResponse::Events(EventsResponse::SessionEvents { events, .. }) =
        resumed.result.unwrap()
    else {
        panic!("unexpected incremental event response");
    };
    assert_eq!(events.len(), 1);
    assert!(matches!(
        events[0].event,
        ServerEvent::AgentSessionRenamed { .. }
    ));
}

#[test]
fn requires_negotiation_before_session_requests() {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    let response = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::ListWorkspaces,
    )));

    support::assert_error_code(&response, ErrorCode::InvalidRequest);
}

#[test]
fn unknown_run_is_structured_not_found() {
    let (_, connection) = support::fixture();

    let response = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRun {
            run_id: RunId::new(),
        },
    )));

    support::assert_error_code(&response, ErrorCode::NotFound);
}
