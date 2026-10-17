//! In-process tests: attributed peer chat, agent direction, ordering, and
//! roster pushes.

#[allow(unused_imports)]
use super::support::*;
use super::*;
use loom_core::{
    Author, CollaborationEntry, CollaborationEntryKind, ParticipantId, ParticipantIdentity,
    Recipient, RunControlAction, RunRevision, SessionRole,
};
use loom_protocol::{AccessRequest, CollaborationRequest, CollaborationResponse, RunActionOutcome};

fn person(name: &str) -> ParticipantIdentity {
    ParticipantIdentity {
        id: ParticipantId::new(),
        display_name: name.to_owned(),
    }
}

fn connect(
    backend: &Arc<InProcessBackend>,
    tokens: &AuthTokenStore,
    identity: &ParticipantIdentity,
) -> InProcessConnection {
    let issued = tokens
        .issue_for_participant(AuthorizationScope::all(), identity.clone())
        .unwrap();
    let connection = backend.connect_authenticated(tokens.authenticate(&issued.token).unwrap());
    negotiate(&connection);
    connection
}

fn send(connection: &InProcessConnection, request: ClientRequest) -> Result<ServerResponse> {
    connection.request(RequestEnvelope::new(request)).result
}

fn collaborate(
    connection: &InProcessConnection,
    request: CollaborationRequest,
) -> Result<CollaborationResponse> {
    match send(connection, ClientRequest::Collaboration(request))? {
        ServerResponse::Collaboration(response) => Ok(response),
        response => panic!("unexpected collaboration response: {response:?}"),
    }
}

fn create_session(connection: &InProcessConnection) -> AgentSessionId {
    let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) = send(
        connection,
        ClientRequest::Workspace(WorkspaceRequest::CreateWorkspace {
            name: "Shared".to_owned(),
        }),
    )
    .unwrap() else {
        panic!("expected a workspace");
    };
    let ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) = send(
        connection,
        ClientRequest::Workspace(WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "Pairing".to_owned(),
        }),
    )
    .unwrap() else {
        panic!("expected a session");
    };
    session.id
}

fn start_settled_run(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
) -> loom_core::RunId {
    let ServerResponse::Run(RunResponse::AgentRunStarted(run)) = send(
        connection,
        ClientRequest::Run(RunRequest::StartSessionAgentRun {
            session_id,
            task: "hello".to_owned(),
            model: deterministic_descriptor().id,
            system_instructions: None,
            repository_instructions: None,
        }),
    )
    .unwrap() else {
        panic!("expected a run");
    };
    await_settled_run(connection, run.id);
    run.id
}

fn revision(connection: &InProcessConnection, run_id: loom_core::RunId) -> RunRevision {
    match collaborate(connection, CollaborationRequest::GetRunRevision { run_id }).unwrap() {
        CollaborationResponse::RunRevision { revision, .. } => revision,
        response => panic!("unexpected revision response: {response:?}"),
    }
}

/// Every collaboration entry in the session's feed, oldest first.
fn history(connection: &InProcessConnection, session_id: AgentSessionId) -> Vec<ServerEvent> {
    let events = match send(
        connection,
        ClientRequest::Events(EventsRequest::GetSessionEvents {
            session_id: Some(session_id),
            workspace_id: None,
            after_sequence: None,
            stream_epoch: None,
        }),
    )
    .unwrap()
    {
        ServerResponse::Events(EventsResponse::SessionEvents { events, .. })
        | ServerResponse::Events(EventsResponse::SessionEventsSnapshot { events, .. }) => events,
        response => panic!("unexpected events response: {response:?}"),
    };
    events.into_iter().map(|event| event.event).collect()
}

fn entries(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
) -> Vec<CollaborationEntry> {
    history(connection, session_id)
        .into_iter()
        .filter_map(|event| match event {
            ServerEvent::Collaboration { entry } => Some(entry),
            _ => None,
        })
        .collect()
}

fn promote(
    owner: &InProcessConnection,
    session_id: AgentSessionId,
    participant: &ParticipantIdentity,
    role: SessionRole,
) {
    send(
        owner,
        ClientRequest::Access(AccessRequest::GrantSessionAccess {
            session_id,
            participant_id: participant.id,
        }),
    )
    .unwrap();
    send(
        owner,
        ClientRequest::Access(AccessRequest::SetSessionRole {
            session_id,
            participant_id: participant.id,
            role,
        }),
    )
    .unwrap();
}

#[test]
fn peer_chat_is_attributed_history_and_never_reaches_the_model() {
    let backend = InProcessBackend::new();
    let tokens = AuthTokenStore::new();
    let owner = person("Owner");
    let owner_connection = connect(&backend, &tokens, &owner);
    let session_id = create_session(&owner_connection);
    let run_id = start_settled_run(&owner_connection, session_id);
    let before = owner_connection
        .backend
        .runs()
        .unwrap()
        .get(&run_id)
        .unwrap()
        .state();

    let CollaborationResponse::PeerMessageSent { entry } = collaborate(
        &owner_connection,
        CollaborationRequest::SendPeerMessage {
            session_id,
            text: "peer-only secret".to_owned(),
        },
    )
    .unwrap() else {
        panic!("expected a peer message");
    };
    assert_eq!(entry.recipient, Recipient::People);
    assert_eq!(
        entry.author,
        Author::Participant {
            participant: owner.clone(),
            role: SessionRole::Owner,
        }
    );

    // The run is untouched: same state, revision, and messages.
    let handle = owner_connection
        .backend
        .runs()
        .unwrap()
        .get(&run_id)
        .cloned()
        .unwrap();
    let after = handle.state();
    assert_eq!(after.run.state, before.run.state);
    assert_eq!(after.run.control_revision, before.run.control_revision);
    assert_eq!(after.messages.len(), before.messages.len());

    // A later direction continues the run, and the model context it assembles
    // carries the direction but never the peer chat.
    let expected = revision(&owner_connection, run_id);
    let CollaborationResponse::RunAction(outcome) = collaborate(
        &owner_connection,
        CollaborationRequest::DirectAgent {
            run_id,
            expected,
            text: "agent direction".to_owned(),
        },
    )
    .unwrap() else {
        panic!("expected a run action");
    };
    let RunActionOutcome::Accepted { entry, .. } = *outcome else {
        panic!("expected an accepted direction");
    };
    assert_eq!(entry.recipient, Recipient::Agent);
    await_settled_run(&owner_connection, run_id);
    let messages = handle.state().messages;
    assert!(
        messages
            .iter()
            .any(|message| message.content.contains("agent direction"))
    );
    assert!(
        !messages
            .iter()
            .any(|message| message.content.contains("peer-only secret"))
    );

    let kinds = entries(&owner_connection, session_id)
        .into_iter()
        .map(|entry| (entry.recipient, entry.kind))
        .collect::<Vec<_>>();
    assert!(kinds.contains(&(
        Recipient::People,
        CollaborationEntryKind::PeerMessage {
            text: "peer-only secret".to_owned()
        }
    )));
    assert!(kinds.contains(&(
        Recipient::Agent,
        CollaborationEntryKind::AgentDirection {
            text: "agent direction".to_owned()
        }
    )));
}

#[test]
fn observers_cannot_chat_direct_or_control() {
    let backend = InProcessBackend::new();
    let tokens = AuthTokenStore::new();
    let owner = person("Owner");
    let guest = person("Guest");
    let owner_connection = connect(&backend, &tokens, &owner);
    let guest_connection = connect(&backend, &tokens, &guest);
    let session_id = create_session(&owner_connection);
    let run_id = start_settled_run(&owner_connection, session_id);
    promote(&owner_connection, session_id, &guest, SessionRole::Observer);

    // An observer may read the revision but not act.
    let expected = revision(&guest_connection, run_id);
    for request in [
        CollaborationRequest::SendPeerMessage {
            session_id,
            text: "hi".to_owned(),
        },
        CollaborationRequest::DirectAgent {
            run_id,
            expected,
            text: "go".to_owned(),
        },
        CollaborationRequest::ControlRun {
            run_id,
            expected,
            action: RunControlAction::Pause,
        },
    ] {
        assert_eq!(
            collaborate(&guest_connection, request).unwrap_err().code,
            ErrorCode::AuthorizationDenied
        );
    }
    assert_eq!(revision(&owner_connection, run_id), expected);

    // Promoted to participant, the same person can chat.
    promote(
        &owner_connection,
        session_id,
        &guest,
        SessionRole::Participant,
    );
    assert!(
        collaborate(
            &guest_connection,
            CollaborationRequest::SendPeerMessage {
                session_id,
                text: "hi".to_owned(),
            },
        )
        .is_ok()
    );
}

#[test]
fn concurrent_directions_yield_one_acceptance_and_one_conflict() {
    let backend = InProcessBackend::new();
    let tokens = AuthTokenStore::new();
    let owner = person("Owner");
    let guest = person("Guest");
    let owner_connection = connect(&backend, &tokens, &owner);
    let guest_connection = connect(&backend, &tokens, &guest);
    let session_id = create_session(&owner_connection);
    let run_id = start_settled_run(&owner_connection, session_id);
    promote(
        &owner_connection,
        session_id,
        &guest,
        SessionRole::Participant,
    );
    let expected = revision(&owner_connection, run_id);

    let outcomes = thread::scope(|scope| {
        let handles = [
            (&owner_connection, "owner direction"),
            (&guest_connection, "guest direction"),
        ]
        .map(|(connection, text)| {
            scope.spawn(move || {
                match collaborate(
                    connection,
                    CollaborationRequest::DirectAgent {
                        run_id,
                        expected,
                        text: text.to_owned(),
                    },
                )
                .unwrap()
                {
                    CollaborationResponse::RunAction(outcome) => *outcome,
                    response => panic!("unexpected response: {response:?}"),
                }
            })
        });
        handles.map(|handle| handle.join().unwrap())
    });
    let accepted = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, RunActionOutcome::Accepted { .. }))
        .count();
    assert_eq!(accepted, 1, "{outcomes:?}");
    let conflict = outcomes
        .iter()
        .find_map(|outcome| match outcome {
            RunActionOutcome::Conflict {
                expected: sent,
                current,
                ..
            } => Some((*sent, *current)),
            RunActionOutcome::Accepted { .. } => None,
        })
        .expect("one direction conflicts");
    assert_eq!(conflict.0, expected);
    assert_eq!(conflict.1.action_sequence, expected.action_sequence + 1);
    await_settled_run(&owner_connection, run_id);

    // Exactly one direction reached the history.
    let directions = entries(&owner_connection, session_id)
        .into_iter()
        .filter(|entry| matches!(entry.kind, CollaborationEntryKind::AgentDirection { .. }))
        .count();
    assert_eq!(directions, 1);

    // A stale control request conflicts too, and changes nothing.
    let stale = collaborate(
        &guest_connection,
        CollaborationRequest::ControlRun {
            run_id,
            expected,
            action: RunControlAction::Interrupt,
        },
    )
    .unwrap();
    assert!(matches!(
        stale,
        CollaborationResponse::RunAction(outcome)
            if matches!(*outcome, RunActionOutcome::Conflict { .. })
    ));
}

#[test]
fn operator_actions_are_attributed_to_the_operator() {
    let backend = InProcessBackend::new();
    let operator = backend.connect();
    negotiate(&operator);
    let session_id = create_session(&operator);
    let run_id = start_settled_run(&operator, session_id);
    let CollaborationResponse::PeerMessageSent { entry } = collaborate(
        &operator,
        CollaborationRequest::SendPeerMessage {
            session_id,
            text: "note".to_owned(),
        },
    )
    .unwrap() else {
        panic!("expected a peer message");
    };
    assert_eq!(entry.author, Author::Operator);

    // Legacy run requests keep working and are recorded too.
    let run = await_settled_run(&operator, run_id);
    send(
        &operator,
        ClientRequest::Run(RunRequest::SendAgentMessage {
            run_id,
            attempt_id: run.attempt_id,
            expected_control_revision: run.control_revision,
            message: "operator direction".to_owned(),
        }),
    )
    .unwrap();
    let direction = entries(&operator, session_id)
        .into_iter()
        .find(|entry| matches!(entry.kind, CollaborationEntryKind::AgentDirection { .. }))
        .expect("the direction is in the history");
    assert_eq!(direction.author, Author::Operator);
    assert_eq!(direction.recipient, Recipient::Agent);
    assert_eq!(
        direction.kind,
        CollaborationEntryKind::AgentDirection {
            text: "operator direction".to_owned()
        }
    );
    assert_eq!(revision(&operator, run_id).action_sequence, 1);
    await_settled_run(&operator, run_id);
}

#[test]
fn roster_and_role_changes_are_pushed_and_revoked_people_stop_reading() {
    let backend = InProcessBackend::new();
    let tokens = AuthTokenStore::new();
    let owner = person("Owner");
    let guest = person("Guest");
    let owner_connection = connect(&backend, &tokens, &owner);
    let session_id = create_session(&owner_connection);
    backend
        .register_participants(std::slice::from_ref(&guest))
        .unwrap();
    promote(
        &owner_connection,
        session_id,
        &guest,
        SessionRole::Participant,
    );
    let guest_connection = connect(&backend, &tokens, &guest);

    let rosters = || {
        history(&owner_connection, session_id)
            .into_iter()
            .filter_map(|event| match event {
                ServerEvent::SessionRosterChanged { entries } => Some(entries),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    // Presence churn is coalesced: the feed carries only the latest roster,
    // which reflects the promotion and the guest's connection.
    let latest = rosters();
    assert_eq!(latest.len(), 1, "{latest:?}");
    let guest_entry = latest[0]
        .iter()
        .find(|entry| entry.participant.id == guest.id)
        .unwrap();
    assert_eq!(guest_entry.role, SessionRole::Participant);
    assert_eq!(guest_entry.connections, 1);

    // A second connection pushes a newer roster; its disconnect another.
    let second = connect(&backend, &tokens, &guest);
    let with_second = rosters();
    assert_eq!(with_second.len(), 1);
    let guest_count = |roster: &[loom_core::RosterEntry]| {
        roster
            .iter()
            .find(|entry| entry.participant.id == guest.id)
            .unwrap()
            .connections
    };
    assert_eq!(guest_count(&with_second[0]), 2);
    drop(second);
    let after_disconnect = rosters();
    assert_eq!(after_disconnect.len(), 1);
    assert_eq!(guest_count(&after_disconnect[0]), 1);

    let changes = entries(&owner_connection, session_id)
        .into_iter()
        .filter_map(|entry| match entry.kind {
            CollaborationEntryKind::AccessChanged { participant, role } => {
                Some((entry.author, participant.id, role))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let by_owner = Author::Participant {
        participant: owner.clone(),
        role: SessionRole::Owner,
    };
    assert_eq!(
        changes,
        vec![
            (by_owner.clone(), guest.id, Some(SessionRole::Observer)),
            (by_owner.clone(), guest.id, Some(SessionRole::Participant)),
        ]
    );

    // The guest reads the feed until revoked, then no longer.
    assert!(!history(&guest_connection, session_id).is_empty());
    send(
        &owner_connection,
        ClientRequest::Access(AccessRequest::RevokeSessionAccess {
            session_id,
            participant_id: guest.id,
        }),
    )
    .unwrap();
    assert_eq!(
        send(
            &guest_connection,
            ClientRequest::Events(EventsRequest::GetSessionEvents {
                session_id: Some(session_id),
                workspace_id: None,
                after_sequence: None,
                stream_epoch: None,
            }),
        )
        .unwrap_err()
        .code,
        ErrorCode::AuthorizationDenied
    );
    let last = entries(&owner_connection, session_id).pop().unwrap();
    assert_eq!(
        last.kind,
        CollaborationEntryKind::AccessChanged {
            participant: guest.clone(),
            role: None
        }
    );
}

#[test]
fn session_collaboration_history_is_readable_in_pages() {
    let backend = InProcessBackend::new();
    let tokens = AuthTokenStore::new();
    let owner = person("Owner");
    let owner_connection = connect(&backend, &tokens, &owner);
    let session_id = create_session(&owner_connection);
    for text in ["one", "two", "three"] {
        collaborate(
            &owner_connection,
            CollaborationRequest::SendPeerMessage {
                session_id,
                text: text.to_owned(),
            },
        )
        .unwrap();
    }
    let page = |after_sequence| match collaborate(
        &owner_connection,
        CollaborationRequest::ListSessionCollaboration {
            session_id,
            after_sequence,
            limit: 2,
        },
    )
    .unwrap()
    {
        CollaborationResponse::SessionCollaboration {
            entries,
            next_after_sequence,
            ..
        } => (entries, next_after_sequence),
        response => panic!("unexpected history response: {response:?}"),
    };
    let text = |entry: &CollaborationEntry| match &entry.kind {
        CollaborationEntryKind::PeerMessage { text } => text.clone(),
        kind => panic!("unexpected entry kind: {kind:?}"),
    };
    let (first, next) = page(None);
    assert_eq!(
        first
            .iter()
            .map(|(_, entry)| text(entry))
            .collect::<Vec<_>>(),
        ["one", "two"]
    );
    assert_eq!(next, Some(first[1].0));
    let (second, next) = page(next);
    assert_eq!(
        second
            .iter()
            .map(|(_, entry)| text(entry))
            .collect::<Vec<_>>(),
        ["three"]
    );
    assert_eq!(next, None);

    // An observer may read the history too.
    let reader = person("Reader");
    let reader_connection = connect(&backend, &tokens, &reader);
    promote(
        &owner_connection,
        session_id,
        &reader,
        SessionRole::Observer,
    );
    assert!(
        collaborate(
            &reader_connection,
            CollaborationRequest::ListSessionCollaboration {
                session_id,
                after_sequence: None,
                limit: 10,
            },
        )
        .is_ok()
    );
}

#[test]
fn presence_churn_does_not_consume_history_retention() {
    let backend = InProcessBackend::new();
    let tokens = AuthTokenStore::new();
    let owner = person("Owner");
    let guest = person("Guest");
    let owner_connection = connect(&backend, &tokens, &owner);
    let session_id = create_session(&owner_connection);
    backend
        .register_participants(std::slice::from_ref(&guest))
        .unwrap();
    promote(
        &owner_connection,
        session_id,
        &guest,
        SessionRole::Participant,
    );
    collaborate(
        &owner_connection,
        CollaborationRequest::SendPeerMessage {
            session_id,
            text: "keep me".to_owned(),
        },
    )
    .unwrap();
    backend.set_event_retention(8).unwrap();
    // Far more connects and disconnects than the retention window holds.
    for _ in 0..20 {
        drop(connect(&backend, &tokens, &guest));
    }
    let events = history(&owner_connection, session_id);
    let rosters = events
        .iter()
        .filter(|event| matches!(event, ServerEvent::SessionRosterChanged { .. }))
        .count();
    assert_eq!(rosters, 1, "only the latest roster is kept");
    let kept = entries(&owner_connection, session_id);
    assert!(
        kept.iter().any(|entry| entry.kind
            == CollaborationEntryKind::PeerMessage {
                text: "keep me".to_owned(),
            }),
        "presence churn evicted history: {events:?}"
    );
    // Access changes are still attributed history entries.
    assert!(
        kept.iter()
            .any(|entry| matches!(entry.kind, CollaborationEntryKind::AccessChanged { .. }))
    );
}

#[test]
fn run_revisions_survive_a_backend_restart() {
    let directory = std::env::temp_dir().join(format!("loom-revision-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join("state.db");
    let owner = person("Owner");
    let (session_id, run_id, before_restart) = {
        let backend = InProcessBackend::new_persistent(&path).unwrap();
        let tokens = AuthTokenStore::new();
        let connection = connect(&backend, &tokens, &owner);
        let session_id = create_session(&connection);
        let run_id = start_settled_run(&connection, session_id);
        for text in ["first", "second"] {
            let expected = revision(&connection, run_id);
            let CollaborationResponse::RunAction(outcome) = collaborate(
                &connection,
                CollaborationRequest::DirectAgent {
                    run_id,
                    expected,
                    text: text.to_owned(),
                },
            )
            .unwrap() else {
                panic!("expected a run action");
            };
            assert!(
                matches!(*outcome, RunActionOutcome::Accepted { .. }),
                "{outcome:?}"
            );
            await_settled_run(&connection, run_id);
        }
        let current = revision(&connection, run_id);
        assert_eq!(current.action_sequence, 2);
        backend.shutdown().unwrap();
        (session_id, run_id, current)
    };

    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let tokens = AuthTokenStore::new();
    let connection = connect(&backend, &tokens, &owner);
    let after_restart = revision(&connection, run_id);
    assert_eq!(
        after_restart, before_restart,
        "the action sequence is durable"
    );
    // A revision that a counter reset to zero would produce (for example one
    // issued right after the run started) must not match after the restart.
    let stale = RunRevision {
        action_sequence: 0,
        ..after_restart
    };
    let CollaborationResponse::RunAction(outcome) = collaborate(
        &connection,
        CollaborationRequest::DirectAgent {
            run_id,
            expected: stale,
            text: "late".to_owned(),
        },
    )
    .unwrap() else {
        panic!("expected a run action");
    };
    assert!(
        matches!(*outcome, RunActionOutcome::Conflict { .. }),
        "{outcome:?}"
    );
    assert!(!entries(&connection, session_id).iter().any(|entry| {
        entry.kind
            == CollaborationEntryKind::AgentDirection {
                text: "late".to_owned(),
            }
    }));
    backend.shutdown().unwrap();
    let _ = fs::remove_dir_all(directory);
}
