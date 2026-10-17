//! In-process tests: participant identity, session roles, revocation, roster.

#[allow(unused_imports)]
use super::support::*;
use super::*;
use loom_core::{ParticipantId, ParticipantIdentity, PresenceStatus, SessionRole};
use loom_protocol::{AccessRequest, AccessResponse};

struct People {
    backend: Arc<InProcessBackend>,
    tokens: AuthTokenStore,
}

impl People {
    fn new(backend: Arc<InProcessBackend>) -> Self {
        Self {
            backend,
            tokens: AuthTokenStore::new(),
        }
    }

    /// A connection authenticated as `identity`.
    fn connect(&self, identity: &ParticipantIdentity) -> InProcessConnection {
        let issued = self
            .tokens
            .issue_for_participant(AuthorizationScope::all(), identity.clone())
            .unwrap();
        let connection = self
            .backend
            .connect_authenticated(self.tokens.authenticate(&issued.token).unwrap());
        negotiate(&connection);
        connection
    }
}

fn person(name: &str) -> ParticipantIdentity {
    ParticipantIdentity {
        id: ParticipantId::new(),
        display_name: name.to_owned(),
    }
}

fn send(connection: &InProcessConnection, request: ClientRequest) -> Result<ServerResponse> {
    connection.request(RequestEnvelope::new(request)).result
}

fn access(connection: &InProcessConnection, request: AccessRequest) -> Result<AccessResponse> {
    match send(connection, ClientRequest::Access(request))? {
        ServerResponse::Access(response) => Ok(response),
        response => panic!("unexpected access response: {response:?}"),
    }
}

fn create_session(connection: &InProcessConnection) -> AgentSessionId {
    let workspace = match send(
        connection,
        ClientRequest::Workspace(WorkspaceRequest::CreateWorkspace {
            name: "Shared".to_owned(),
        }),
    )
    .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    match send(
        connection,
        ClientRequest::Workspace(WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "Pairing".to_owned(),
        }),
    )
    .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    }
}

fn rename(connection: &InProcessConnection, session_id: AgentSessionId) -> Result<ServerResponse> {
    send(
        connection,
        ClientRequest::Session(SessionRequest::RenameAgentSession {
            session_id,
            name: "Renamed".to_owned(),
        }),
    )
}

fn start_run(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
) -> Result<ServerResponse> {
    send(
        connection,
        ClientRequest::Run(RunRequest::StartSessionAgentRun {
            session_id,
            task: "hello".to_owned(),
            model: deterministic_descriptor().id,
            system_instructions: None,
            repository_instructions: None,
        }),
    )
}

fn read(connection: &InProcessConnection, session_id: AgentSessionId) -> Result<ServerResponse> {
    send(
        connection,
        ClientRequest::Session(SessionRequest::GetAgentSession { session_id }),
    )
}

fn denied(result: Result<impl std::fmt::Debug>) {
    assert_eq!(result.unwrap_err().code, ErrorCode::AuthorizationDenied);
}

#[test]
fn identity_is_bound_by_the_server_and_creators_own_their_sessions() {
    let backend = InProcessBackend::new();
    let people = People::new(backend.clone());
    let owner = person("Björn");
    let connection = people.connect(&owner);
    assert_eq!(
        access(&connection, AccessRequest::GetCurrentParticipant).unwrap(),
        AccessResponse::CurrentParticipant {
            participant: Some(owner.clone())
        }
    );
    let operator = backend.connect();
    negotiate(&operator);
    assert_eq!(
        access(&operator, AccessRequest::GetCurrentParticipant).unwrap(),
        AccessResponse::CurrentParticipant { participant: None }
    );

    let session_id = create_session(&connection);
    let AccessResponse::SessionRoster { entries, .. } =
        access(&connection, AccessRequest::GetSessionRoster { session_id }).unwrap()
    else {
        panic!("expected a roster");
    };
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].participant, owner);
    assert_eq!(entries[0].role, SessionRole::Owner);
    assert_eq!(entries[0].presence, PresenceStatus::Online);
}

#[test]
fn roles_gate_reads_actions_and_access_management() {
    let backend = InProcessBackend::new();
    let people = People::new(backend.clone());
    let owner = person("Owner");
    let guest = person("Guest");
    let outsider = person("Outsider");
    let owner_connection = people.connect(&owner);
    let guest_connection = people.connect(&guest);
    let outsider_connection = people.connect(&outsider);
    let session_id = create_session(&owner_connection);

    // Without a grant, neither the session nor its roster is readable, and
    // the session is absent from listings.
    denied(read(&guest_connection, session_id));
    denied(access(
        &guest_connection,
        AccessRequest::GetSessionRoster { session_id },
    ));
    let workspace_id = match read(&owner_connection, session_id).unwrap() {
        ServerResponse::Session(SessionResponse::AgentSession(session)) => session.workspace_id,
        response => panic!("unexpected session response: {response:?}"),
    };
    let listing = send(
        &guest_connection,
        ClientRequest::Workspace(WorkspaceRequest::ListWorkspaceSessions {
            workspace_id,
            include_archived: true,
        }),
    )
    .unwrap();
    assert!(matches!(
        listing,
        ServerResponse::Session(SessionResponse::AgentSessions { sessions }) if sessions.is_empty()
    ));
    let events = send(
        &guest_connection,
        ClientRequest::Events(EventsRequest::GetSessionEvents {
            session_id: None,
            workspace_id: None,
            after_sequence: None,
            stream_epoch: None,
        }),
    )
    .unwrap();
    assert!(matches!(
        events,
        ServerResponse::Events(EventsResponse::SessionEvents { events, .. })
            if events.iter().all(|event| event.session_id != session_id)
    ));

    // Only an owner manages access, and new grants start as observers.
    denied(access(
        &guest_connection,
        AccessRequest::GrantSessionAccess {
            session_id,
            participant_id: guest.id,
        },
    ));
    let AccessResponse::SessionGrant(grant) = access(
        &owner_connection,
        AccessRequest::GrantSessionAccess {
            session_id,
            participant_id: guest.id,
        },
    )
    .unwrap() else {
        panic!("expected a grant");
    };
    assert_eq!(grant.role, SessionRole::Observer);
    assert_eq!(grant.granted_by, Some(owner.id));

    // An observer reads but cannot act.
    assert!(read(&guest_connection, session_id).is_ok());
    assert!(
        access(
            &guest_connection,
            AccessRequest::GetSessionRoster { session_id }
        )
        .is_ok()
    );
    denied(start_run(&guest_connection, session_id));
    denied(rename(&guest_connection, session_id));
    denied(access(
        &guest_connection,
        AccessRequest::SetSessionRole {
            session_id,
            participant_id: guest.id,
            role: SessionRole::Owner,
        },
    ));

    // A participant acts inside runs without owner approval, but cannot
    // manage the session.
    access(
        &owner_connection,
        AccessRequest::SetSessionRole {
            session_id,
            participant_id: guest.id,
            role: SessionRole::Participant,
        },
    )
    .unwrap();
    let run_id = match start_run(&guest_connection, session_id).unwrap() {
        ServerResponse::Run(RunResponse::AgentRunStarted(run)) => run.id,
        response => panic!("unexpected run response: {response:?}"),
    };
    assert!(
        send(
            &guest_connection,
            ClientRequest::Run(RunRequest::GetAgentRun { run_id })
        )
        .is_ok()
    );
    denied(rename(&guest_connection, session_id));
    denied(access(
        &guest_connection,
        AccessRequest::RevokeSessionAccess {
            session_id,
            participant_id: owner.id,
        },
    ));
    // The outsider still reads nothing, including the run.
    denied(send(
        &outsider_connection,
        ClientRequest::Run(RunRequest::GetAgentRun { run_id }),
    ));

    // The last owner can be neither demoted nor revoked.
    assert_eq!(
        access(
            &owner_connection,
            AccessRequest::SetSessionRole {
                session_id,
                participant_id: owner.id,
                role: SessionRole::Participant,
            },
        )
        .unwrap_err()
        .code,
        ErrorCode::Conflict
    );
    assert!(rename(&owner_connection, session_id).is_ok());
    await_settled_run(&owner_connection, run_id);
}

#[test]
fn revocation_stops_requests_and_signals_only_the_revoked_person() {
    let backend = InProcessBackend::new();
    let people = People::new(backend.clone());
    let owner = person("Owner");
    let guest = person("Guest");
    let owner_connection = people.connect(&owner);
    let guest_connection = people.connect(&guest);
    let session_id = create_session(&owner_connection);
    access(
        &owner_connection,
        AccessRequest::GrantSessionAccess {
            session_id,
            participant_id: guest.id,
        },
    )
    .unwrap();
    assert!(read(&guest_connection, session_id).is_ok());

    let guest_revoked = guest_connection.presence().unwrap().revoked();
    let owner_revoked = owner_connection.presence().unwrap().revoked();
    let AccessResponse::SessionAccessRevoked { participant_id, .. } = access(
        &owner_connection,
        AccessRequest::RevokeSessionAccess {
            session_id,
            participant_id: guest.id,
        },
    )
    .unwrap() else {
        panic!("expected a revocation");
    };
    assert_eq!(participant_id, guest.id);
    denied(read(&guest_connection, session_id));
    assert!(read(&owner_connection, session_id).is_ok());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(1), guest_revoked.notified())
            .await
            .expect("the revoked person's connections are signalled");
        assert!(
            tokio::time::timeout(Duration::from_millis(20), owner_revoked.notified())
                .await
                .is_err()
        );
    });
}

#[test]
fn roster_merges_connections_and_tracks_disconnects() {
    let backend = InProcessBackend::new();
    let people = People::new(backend.clone());
    let owner = person("Owner");
    let guest = person("Guest");
    let owner_connection = people.connect(&owner);
    let session_id = create_session(&owner_connection);
    // Granting needs the person to be known; a token file registers people
    // before their first connection.
    backend
        .register_participants(std::slice::from_ref(&guest))
        .unwrap();
    access(
        &owner_connection,
        AccessRequest::GrantSessionAccess {
            session_id,
            participant_id: guest.id,
        },
    )
    .unwrap();
    let roster = |connection: &InProcessConnection| match access(
        connection,
        AccessRequest::GetSessionRoster { session_id },
    )
    .unwrap()
    {
        AccessResponse::SessionRoster { entries, .. } => entries,
        response => panic!("unexpected roster response: {response:?}"),
    };
    let entries = roster(&owner_connection);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1].participant, guest);
    assert_eq!(entries[1].presence, PresenceStatus::Offline);

    let first = people.connect(&guest);
    let second = people.connect(&guest);
    let entries = roster(&owner_connection);
    assert_eq!(entries.len(), 2, "one entry per person");
    assert_eq!(entries[1].connections, 2);
    assert_eq!(entries[1].presence, PresenceStatus::Online);

    drop(first);
    // A reconnect does not add an entry.
    let reconnect = people.connect(&guest);
    let entries = roster(&owner_connection);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1].connections, 2);
    drop((second, reconnect));
    let entries = roster(&owner_connection);
    assert_eq!(entries[1].connections, 0);
    assert_eq!(entries[1].presence, PresenceStatus::Offline);
    assert!(entries[1].last_active_at.is_some());
}

#[test]
fn grants_survive_a_backend_restart() {
    let directory = std::env::temp_dir().join(format!("loom-access-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join("state.db");
    let owner = person("Owner");
    let guest = person("Guest");
    let session_id = {
        let backend = InProcessBackend::new_persistent(&path).unwrap();
        let people = People::new(backend.clone());
        let owner_connection = people.connect(&owner);
        let _guest_connection = people.connect(&guest);
        let session_id = create_session(&owner_connection);
        access(
            &owner_connection,
            AccessRequest::GrantSessionAccess {
                session_id,
                participant_id: guest.id,
            },
        )
        .unwrap();
        access(
            &owner_connection,
            AccessRequest::SetSessionRole {
                session_id,
                participant_id: guest.id,
                role: SessionRole::Participant,
            },
        )
        .unwrap();
        backend.shutdown().unwrap();
        session_id
    };

    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let people = People::new(backend.clone());
    let guest_connection = people.connect(&guest);
    let AccessResponse::SessionRoster { entries, .. } = access(
        &guest_connection,
        AccessRequest::GetSessionRoster { session_id },
    )
    .unwrap() else {
        panic!("expected a roster");
    };
    let roles = entries
        .iter()
        .map(|entry| (entry.participant.display_name.as_str(), entry.role))
        .collect::<Vec<_>>();
    assert_eq!(
        roles,
        vec![
            ("Owner", SessionRole::Owner),
            ("Guest", SessionRole::Participant)
        ]
    );
    denied(rename(&guest_connection, session_id));
    let outsider = people.connect(&person("Outsider"));
    denied(read(&outsider, session_id));
    backend.shutdown().unwrap();
    let _ = fs::remove_dir_all(directory);
}
