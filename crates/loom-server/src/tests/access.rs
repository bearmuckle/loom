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
            activity_detail: loom_protocol::ActivityDetail::Full,
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

/// Simulates a completed device sign-in of `who` on the worker.
fn sign_in(
    backend: &InProcessBackend,
    who: &ParticipantIdentity,
    secret: &str,
) -> loom_core::ProviderCredentialMetadata {
    backend
        .provider_credentials
        .sign_in(
            backend.providers.instance_credentials().unwrap().as_ref(),
            "deterministic",
            who,
            secret.to_owned(),
        )
        .unwrap()
}

fn run_pin(
    connection: &InProcessConnection,
    run_id: loom_core::RunId,
) -> loom_core::RunCredentialPin {
    match access(connection, AccessRequest::GetRunCredential { run_id }).unwrap() {
        AccessResponse::RunCredential { pin: Some(pin), .. } => pin,
        response => panic!("unexpected run credential response: {response:?}"),
    }
}

#[test]
fn active_runs_keep_their_pinned_credential_and_secrets_stay_on_the_worker() {
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
    access(
        &owner_connection,
        AccessRequest::SetSessionRole {
            session_id,
            participant_id: guest.id,
            role: SessionRole::Participant,
        },
    )
    .unwrap();

    let credential = sign_in(&backend, &owner, "owner-secret-v1");
    let guest_credential = sign_in(&backend, &guest, "guest-secret-v1");
    // Only the owner selects, and only a credential they hold.
    denied(access(
        &guest_connection,
        AccessRequest::SelectSessionCredential {
            session_id,
            credential_id: guest_credential.id,
        },
    ));
    denied(access(
        &owner_connection,
        AccessRequest::SelectSessionCredential {
            session_id,
            credential_id: guest_credential.id,
        },
    ));
    access(
        &owner_connection,
        AccessRequest::SelectSessionCredential {
            session_id,
            credential_id: credential.id,
        },
    )
    .unwrap();
    // Anyone with access sees whose account runs will use, before a run.
    let AccessResponse::SessionCredential {
        credential: Some(selected),
        ..
    } = access(
        &guest_connection,
        AccessRequest::GetSessionCredential { session_id },
    )
    .unwrap()
    else {
        panic!("expected a selected credential");
    };
    assert_eq!(
        (selected.owner_label.as_str(), selected.version),
        ("Owner", 1)
    );
    // Listings show only the caller's own credentials.
    let AccessResponse::ProviderCredentials { credentials } =
        access(&guest_connection, AccessRequest::ListProviderCredentials).unwrap()
    else {
        panic!("expected credentials");
    };
    assert_eq!(credentials.len(), 1);
    assert_eq!(credentials[0].id, guest_credential.id);

    // A participant starts the run; it pins the owner's selected version.
    let run_id = match start_run(&guest_connection, session_id).unwrap() {
        ServerResponse::Run(RunResponse::AgentRunStarted(run)) => run.id,
        response => panic!("unexpected run response: {response:?}"),
    };
    // Later sign-ins by anyone, and a new owner sign-in, do not move the pin.
    sign_in(&backend, &guest, "guest-secret-v2");
    let newer = sign_in(&backend, &owner, "owner-secret-v2");
    assert_eq!((newer.id, newer.version), (credential.id, 2));
    drop(owner_connection);
    let pin = run_pin(&guest_connection, run_id);
    assert_eq!(pin.credential_id, Some(credential.id));
    assert_eq!(pin.version, Some(1));
    assert_eq!(pin.owner, Some(owner.id));
    assert_eq!(pin.owner_label, "Owner");
    await_settled_run(&guest_connection, run_id);
    assert_eq!(run_pin(&guest_connection, run_id).version, Some(1));

    // No secret appears in responses or session events.
    let events = send(
        &guest_connection,
        ClientRequest::Events(EventsRequest::GetSessionEvents {
            activity_detail: loom_protocol::ActivityDetail::Full,
            session_id: Some(session_id),
            workspace_id: None,
            after_sequence: None,
            stream_epoch: None,
        }),
    )
    .unwrap();
    let serialized = format!(
        "{}{}{}",
        serde_json::to_string(&events).unwrap(),
        serde_json::to_string(&pin).unwrap(),
        serde_json::to_string(&selected).unwrap()
    );
    for secret in [
        "owner-secret-v1",
        "owner-secret-v2",
        "guest-secret-v1",
        "guest-secret-v2",
    ] {
        assert!(!serialized.contains(secret), "{secret} leaked");
    }
}

#[test]
fn a_revoked_or_missing_credential_is_an_explicit_authentication_error() {
    let backend = InProcessBackend::new();
    let people = People::new(backend.clone());
    let owner = person("Owner");
    let owner_connection = people.connect(&owner);
    let session_id = create_session(&owner_connection);
    let credential = sign_in(&backend, &owner, "owner-secret");
    access(
        &owner_connection,
        AccessRequest::SelectSessionCredential {
            session_id,
            credential_id: credential.id,
        },
    )
    .unwrap();
    let other = person("Other");
    denied(access(
        &people.connect(&other),
        AccessRequest::RevokeProviderCredential {
            credential_id: credential.id,
        },
    ));
    access(
        &owner_connection,
        AccessRequest::RevokeProviderCredential {
            credential_id: credential.id,
        },
    )
    .unwrap();
    // The run does not fall back to the worker credential.
    let error = start_run(&owner_connection, session_id).unwrap_err();
    assert_eq!(error.code, ErrorCode::ProviderAuthentication);
    assert!(error.message.contains("revoked"), "{}", error.message);
    // A revoked credential cannot be selected again.
    assert_eq!(
        access(
            &owner_connection,
            AccessRequest::SelectSessionCredential {
                session_id,
                credential_id: credential.id,
            },
        )
        .unwrap_err()
        .code,
        ErrorCode::ProviderAuthentication
    );
}

#[test]
fn only_owners_browse_repositories_and_only_operators_configure_providers() {
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
    access(
        &owner_connection,
        AccessRequest::SetSessionRole {
            session_id,
            participant_id: guest.id,
            role: SessionRole::Participant,
        },
    )
    .unwrap();

    for request in [
        RepositoryRequest::SearchGitHubRepositories {
            query: "loom".to_owned(),
            session_id: Some(session_id),
        },
        RepositoryRequest::SearchGitHubRepositories {
            query: "loom".to_owned(),
            session_id: None,
        },
        RepositoryRequest::ListClonedRepositories,
        RepositoryRequest::AttachSessionRepository {
            session_id,
            source: "https://github.com/bearmuckle/loom.git".to_owned(),
            path: "loom".to_owned(),
            revision: None,
            reuse_local: false,
        },
    ] {
        denied(send(&guest_connection, ClientRequest::Repository(request)));
    }
    // A participant still reads the session's attached checkouts.
    assert!(
        send(
            &guest_connection,
            ClientRequest::Repository(RepositoryRequest::ListSessionRepositories { session_id }),
        )
        .is_ok()
    );
    // The owner passes the role check; the search then fails only because no
    // GitHub credential is configured on this worker.
    let owner_search = send(
        &owner_connection,
        ClientRequest::Repository(RepositoryRequest::SearchGitHubRepositories {
            query: "loom".to_owned(),
            session_id: Some(session_id),
        }),
    );
    assert_ne!(
        owner_search.unwrap_err().code,
        ErrorCode::AuthorizationDenied
    );

    // Worker-wide provider configuration is for operators only.
    for request in [
        ProviderRequest::ConfigureGitHubCopilot {
            access_token: "token".to_owned(),
        },
        ProviderRequest::ConfigureGitHubRepository {
            access_token: "token".to_owned(),
        },
        ProviderRequest::ConfigureGitHubWriteAccess { enabled: true },
    ] {
        denied(send(&owner_connection, ClientRequest::Provider(request)));
    }

    // A device sign-in's status is visible to its initiator and operators.
    let now = std::time::Instant::now();
    backend
        .credentials
        .begin_pending_for(
            "owner-login".to_owned(),
            now,
            Duration::from_secs(300),
            8,
            now + Duration::from_secs(600),
            Some(owner.id),
        )
        .unwrap();
    let status = |connection: &InProcessConnection| {
        send(
            connection,
            ClientRequest::Provider(ProviderRequest::GetGitHubCopilotLoginStatus {
                login_id: "owner-login".to_owned(),
            }),
        )
    };
    assert!(status(&owner_connection).is_ok());
    assert_eq!(
        status(&guest_connection).unwrap_err().code,
        ErrorCode::NotFound
    );
    let operator = backend.connect();
    negotiate(&operator);
    assert!(status(&operator).is_ok());
}

fn request_access(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
    role: SessionRole,
    note: Option<&str>,
) -> Result<loom_core::SessionAccessRequest> {
    match access(
        connection,
        AccessRequest::RequestSessionAccess {
            session_id,
            requested_role: role,
            note: note.map(str::to_owned),
        },
    )? {
        AccessResponse::SessionAccessRequestPending(request) => Ok(request),
        response => panic!("unexpected request response: {response:?}"),
    }
}

fn pending(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
) -> Result<Vec<loom_core::SessionAccessRequest>> {
    match access(
        connection,
        AccessRequest::ListSessionAccessRequests { session_id },
    )? {
        AccessResponse::SessionAccessRequests { requests, .. } => Ok(requests),
        response => panic!("unexpected list response: {response:?}"),
    }
}

/// Access-request events a connection sees on the cross-session stream. A
/// requester without a grant cannot read the session feed, so the request
/// events filed by them are delivered there.
fn request_events(connection: &InProcessConnection) -> Vec<ServerEvent> {
    match send(
        connection,
        ClientRequest::Events(EventsRequest::GetSessionEvents {
            activity_detail: loom_protocol::ActivityDetail::Full,
            session_id: None,
            workspace_id: None,
            after_sequence: None,
            stream_epoch: None,
        }),
    )
    .unwrap()
    {
        ServerResponse::Events(EventsResponse::SessionEvents { events, .. }) => events
            .into_iter()
            .map(|event| event.event)
            .filter(|event| {
                matches!(
                    event,
                    ServerEvent::SessionAccessRequested { .. }
                        | ServerEvent::SessionAccessRequestResolved { .. }
                )
            })
            .collect(),
        response => panic!("unexpected events response: {response:?}"),
    }
}

#[test]
fn access_requests_are_filed_listed_and_decided_by_owners() {
    use loom_core::{AccessRequestDecision, Author, CollaborationEntryKind};

    let backend = InProcessBackend::new();
    let people = People::new(backend.clone());
    let owner = person("Owner");
    let guest = person("Guest");
    let other = person("Other");
    let owner_connection = people.connect(&owner);
    let guest_connection = people.connect(&guest);
    let other_connection = people.connect(&other);
    let session_id = create_session(&owner_connection);

    // An operator connection has no identity to request with.
    let operator = backend.connect();
    negotiate(&operator);
    assert_eq!(
        request_access(&operator, session_id, SessionRole::Observer, None)
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    // An owner already holds every role.
    assert!(request_access(&owner_connection, session_id, SessionRole::Owner, None).is_err());

    // A person without a grant may ask; asking again replaces the request.
    let first = request_access(
        &guest_connection,
        session_id,
        SessionRole::Observer,
        Some("hi"),
    )
    .unwrap();
    let request = request_access(
        &guest_connection,
        session_id,
        SessionRole::Participant,
        Some("  pairing on #25  "),
    )
    .unwrap();
    assert_eq!(request.id, first.id);
    assert_eq!(request.participant, guest);
    assert_eq!(request.note.as_deref(), Some("pairing on #25"));
    assert_eq!(
        request_access(
            &guest_connection,
            session_id,
            SessionRole::Observer,
            Some(&"x".repeat(loom_core::MAX_ACCESS_REQUEST_NOTE_BYTES + 1)),
        )
        .unwrap_err()
        .code,
        ErrorCode::InvalidRequest
    );

    // Only owners list and decide.
    denied(pending(&guest_connection, session_id));
    assert_eq!(
        pending(&owner_connection, session_id).unwrap(),
        vec![request.clone()]
    );
    denied(access(
        &guest_connection,
        AccessRequest::ApproveSessionAccessRequest {
            session_id,
            request_id: request.id,
            role: None,
        },
    ));
    // Someone else cannot withdraw it.
    assert_eq!(
        access(
            &other_connection,
            AccessRequest::WithdrawSessionAccessRequest {
                session_id,
                request_id: request.id,
            },
        )
        .unwrap_err()
        .code,
        ErrorCode::NotFound
    );

    // Owners and the requester see the request event; others do not.
    assert!(matches!(
        request_events(&owner_connection).last(),
        Some(ServerEvent::SessionAccessRequested { request: seen }) if *seen == request
    ));
    assert!(!request_events(&guest_connection).is_empty());
    assert!(request_events(&other_connection).is_empty());

    // Approval applies the requested role and is attributed history.
    let AccessResponse::SessionAccessRequestResolved {
        decision, grant, ..
    } = access(
        &owner_connection,
        AccessRequest::ApproveSessionAccessRequest {
            session_id,
            request_id: request.id,
            role: None,
        },
    )
    .unwrap()
    else {
        panic!("expected a resolution");
    };
    assert_eq!(
        decision,
        AccessRequestDecision::Approved {
            role: SessionRole::Participant
        }
    );
    assert_eq!(grant.unwrap().role, SessionRole::Participant);
    assert!(pending(&owner_connection, session_id).unwrap().is_empty());
    assert!(matches!(
        request_events(&guest_connection).last(),
        Some(ServerEvent::SessionAccessRequestResolved { decision, .. })
            if *decision == AccessRequestDecision::Approved { role: SessionRole::Participant }
    ));
    let history = match send(
        &owner_connection,
        ClientRequest::Events(EventsRequest::GetSessionEvents {
            activity_detail: loom_protocol::ActivityDetail::Full,
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
    let decided = history
        .iter()
        .find_map(|event| match &event.event {
            ServerEvent::Collaboration { entry } => match &entry.kind {
                CollaborationEntryKind::AccessRequestDecided { participant, .. } => {
                    Some((entry.author.clone(), participant.clone()))
                }
                _ => None,
            },
            _ => None,
        })
        .expect("the approval is recorded");
    assert_eq!(
        decided,
        (
            Author::Participant {
                participant: owner.clone(),
                role: SessionRole::Owner
            },
            guest.clone()
        )
    );

    // A denial leaves access unchanged; a withdrawal removes the request.
    let promotion =
        request_access(&guest_connection, session_id, SessionRole::Owner, None).unwrap();
    let AccessResponse::SessionAccessRequestResolved {
        decision, grant, ..
    } = access(
        &owner_connection,
        AccessRequest::DenySessionAccessRequest {
            session_id,
            request_id: promotion.id,
            reason: Some("not yet".to_owned()),
        },
    )
    .unwrap()
    else {
        panic!("expected a resolution");
    };
    assert_eq!(
        decision,
        AccessRequestDecision::Denied {
            reason: Some("not yet".to_owned())
        }
    );
    assert!(grant.is_none());
    denied(rename(&guest_connection, session_id));
    let again = request_access(&other_connection, session_id, SessionRole::Observer, None).unwrap();
    assert!(matches!(
        access(
            &other_connection,
            AccessRequest::WithdrawSessionAccessRequest {
                session_id,
                request_id: again.id,
            },
        )
        .unwrap(),
        AccessResponse::SessionAccessRequestResolved {
            decision: AccessRequestDecision::Withdrawn,
            ..
        }
    ));
    assert!(pending(&owner_connection, session_id).unwrap().is_empty());
    assert_eq!(
        access(
            &owner_connection,
            AccessRequest::DenySessionAccessRequest {
                session_id,
                request_id: again.id,
                reason: None,
            },
        )
        .unwrap_err()
        .code,
        ErrorCode::NotFound
    );
}

#[test]
fn pending_access_requests_survive_a_backend_restart() {
    let directory =
        std::env::temp_dir().join(format!("loom-access-request-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join("state.db");
    let owner = person("Owner");
    let guest = person("Guest");
    let (session_id, request) = {
        let backend = InProcessBackend::new_persistent(&path).unwrap();
        let people = People::new(backend.clone());
        let owner_connection = people.connect(&owner);
        let guest_connection = people.connect(&guest);
        let session_id = create_session(&owner_connection);
        let request = request_access(
            &guest_connection,
            session_id,
            SessionRole::Participant,
            Some("note"),
        )
        .unwrap();
        backend.shutdown().unwrap();
        (session_id, request)
    };
    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let people = People::new(backend.clone());
    let owner_connection = people.connect(&owner);
    assert_eq!(
        pending(&owner_connection, session_id).unwrap(),
        vec![request.clone()]
    );
    access(
        &owner_connection,
        AccessRequest::ApproveSessionAccessRequest {
            session_id,
            request_id: request.id,
            role: Some(SessionRole::Observer),
        },
    )
    .unwrap();
    let guest_connection = people.connect(&guest);
    assert!(read(&guest_connection, session_id).is_ok());
    denied(start_run(&guest_connection, session_id));
    backend.shutdown().unwrap();
    let _ = fs::remove_dir_all(directory);
}
