//! In-process tests: idempotency slots.

#[allow(unused_imports)]
use super::support::*;
use super::*;

#[test]
fn request_slot_is_released_when_authorization_is_rejected() {
    let backend = InProcessBackend::new();
    let tokens = AuthTokenStore::new();
    let scoped = tokens
        .issue(AuthorizationScope::for_sessions(
            [AgentSessionId::new()],
            backend.supported_capabilities.clone(),
        ))
        .unwrap();
    let connection = backend.connect_authenticated(tokens.authenticate(&scoped.token).unwrap());
    negotiate(&connection);

    let response = connection.request(RequestEnvelope::with_request_id(
        RequestId::new(),
        ClientRequest::Workspace(WorkspaceRequest::CreateWorkspace {
            name: "rejected".to_owned(),
        }),
    ));
    assert_eq!(
        response.result.unwrap_err().code,
        ErrorCode::AuthorizationDenied
    );
    assert!(
        backend.idempotency_store.in_flight_request_ids().is_empty(),
        "a rejected request must release its idempotency slot"
    );
}

#[test]
fn request_slot_is_released_after_a_cached_response() {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate(&connection);

    let request_id = RequestId::new();
    let request = ClientRequest::Workspace(WorkspaceRequest::CreateWorkspace {
        name: "cached".to_owned(),
    });
    let first = connection.request(RequestEnvelope::with_request_id(
        request_id,
        request.clone(),
    ));

    let replay = connection.request(RequestEnvelope::with_request_id(request_id, request));
    assert_eq!(first.result, replay.result);
    assert!(!first.replayed && replay.replayed);
    assert!(matches!(
        first.result,
        Ok(ServerResponse::Workspace(
            WorkspaceResponse::WorkspaceCreated(_)
        ))
    ));
    assert!(
        backend.idempotency_store.in_flight_request_ids().is_empty(),
        "a cached response must release its idempotency slot"
    );
}

#[test]
fn a_cached_answer_is_marked_as_a_replay() {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate(&connection);

    let request_id = RequestId::new();
    let request = ClientRequest::Workspace(WorkspaceRequest::CreateWorkspace {
        name: "replayed".to_owned(),
    });
    let first = connection.request(RequestEnvelope::with_request_id(
        request_id,
        request.clone(),
    ));
    assert!(matches!(
        first.result,
        Ok(ServerResponse::Workspace(
            WorkspaceResponse::WorkspaceCreated(_)
        ))
    ));
    assert!(
        !first.replayed,
        "the answer to the first request must not be marked as a replay"
    );

    let replay = connection.request(RequestEnvelope::with_request_id(
        request_id,
        request.clone(),
    ));
    assert_eq!(replay.result, first.result);
    assert!(
        replay.replayed,
        "a cached answer must be marked as a replay"
    );

    let repeated = connection.request(RequestEnvelope::with_request_id(request_id, request));
    assert_eq!(repeated.result, first.result);
    assert!(
        repeated.replayed,
        "marking the returned copy must not contaminate the cached record"
    );
}

#[test]
fn request_slot_is_released_after_an_idempotency_conflict() {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate(&connection);

    let request_id = RequestId::new();
    let created = connection.request(RequestEnvelope::with_request_id(
        request_id,
        ClientRequest::Workspace(WorkspaceRequest::CreateWorkspace {
            name: "first".to_owned(),
        }),
    ));
    assert!(matches!(
        created.result,
        Ok(ServerResponse::Workspace(
            WorkspaceResponse::WorkspaceCreated(_)
        ))
    ));

    let conflict = connection.request(RequestEnvelope::with_request_id(
        request_id,
        ClientRequest::Workspace(WorkspaceRequest::CreateWorkspace {
            name: "second".to_owned(),
        }),
    ));
    assert_eq!(conflict.result.unwrap_err().code, ErrorCode::Conflict);
    assert!(
        backend.idempotency_store.in_flight_request_ids().is_empty(),
        "a conflicted request must release its idempotency slot"
    );
}
