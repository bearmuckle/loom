//! In-process tests: GitHub identity sign-in, pre-login connections, sign-out.

#[allow(unused_imports)]
use super::support::*;
use super::*;
use loom_core::{GitHubSignInStatus, ParticipantIdentity, PresenceStatus, github_participant_id};
use loom_protocol::{AccessRequest, AccessResponse};

/// A local stand-in for github.com's device flow and account API.
///
/// Every round of one sign-in makes three requests: the device authorization,
/// the token exchange, and the account read. Nothing in these tests reaches the
/// network.
struct GitHubFixture {
    base: String,
    requests: std::sync::mpsc::Receiver<String>,
    server: thread::JoinHandle<()>,
}

impl GitHubFixture {
    fn start(rounds: usize, user_id: u64, login: &'static str) -> Self {
        use std::io::{Read, Write};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, requests) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            for _ in 0..rounds * 3 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut byte = [0_u8; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    if stream.read_exact(&mut byte).is_err() {
                        return;
                    }
                    request.push(byte[0]);
                }
                let request = String::from_utf8_lossy(&request).into_owned();
                let path = request
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_owned();
                let body = match path.as_str() {
                    "/login/device/code" => concat!(
                        r#"{"device_code":"device-code","user_code":"ABCD-1234","#,
                        r#""verification_uri":"https://github.com/login/device","#,
                        r#""expires_in":900,"interval":1}"#
                    )
                    .to_owned(),
                    "/login/oauth/access_token" => {
                        r#"{"access_token":"identity-token","token_type":"bearer"}"#.to_owned()
                    }
                    "/user" => format!(r#"{{"id":{user_id},"login":"{login}"}}"#),
                    other => panic!("unexpected fixture request: {other}"),
                };
                let _ = sender.send(path);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        Self {
            base: format!("http://{address}"),
            requests,
            server,
        }
    }

    fn config(&self, allowlist: &[&str]) -> GitHubSignInConfig {
        GitHubSignInConfig::new(allowlist.iter().map(|login| (*login).to_owned()))
            .with_device_code_url(format!("{}/login/device/code", self.base))
            .with_access_token_url(format!("{}/login/oauth/access_token", self.base))
            .with_user_api_url(format!("{}/user", self.base))
    }

    /// The paths this fixture served, in order.
    fn served(&self, expected: usize) -> Vec<String> {
        (0..expected)
            .map(|_| {
                self.requests
                    .recv_timeout(Duration::from_secs(10))
                    .expect("the sign-in did not reach the fixture")
            })
            .collect()
    }

    fn join(self) -> Vec<String> {
        let served = self.requests.try_iter().collect::<Vec<_>>();
        self.server.join().expect("fixture server panicked");
        served
    }
}

/// Serves one GitHub account read and returns its endpoint plus a handle that
/// yields the request line.
fn serve_github_user(body: &'static str) -> (String, thread::JoinHandle<String>) {
    use std::io::{Read, Write};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            if stream.read_exact(&mut byte).is_err() {
                break;
            }
            request.push(byte[0]);
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
        String::from_utf8_lossy(&request)
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned()
    });
    (format!("http://{address}/user"), server)
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

fn start_sign_in(connection: &InProcessConnection) -> String {
    match access(connection, AccessRequest::StartGitHubSignIn).unwrap() {
        AccessResponse::GitHubSignInStarted {
            login_id,
            user_code,
            verification_uri,
            expires_in,
            interval,
        } => {
            assert_eq!(user_code, "ABCD-1234");
            assert_eq!(verification_uri, "https://github.com/login/device");
            assert_eq!((expires_in, interval), (900, 1));
            login_id
        }
        response => panic!("unexpected sign-in response: {response:?}"),
    }
}

/// Polls a sign-in once, returning its status and the credential the worker
/// hands out beside it.
fn poll_sign_in(
    connection: &InProcessConnection,
    login_id: &str,
) -> (GitHubSignInStatus, Option<String>) {
    match access(
        connection,
        AccessRequest::GetGitHubSignInStatus {
            login_id: login_id.to_owned(),
        },
    )
    .expect("the sign-in is readable by its initiator")
    {
        AccessResponse::GitHubSignInStatus {
            status,
            resume_token,
        } => (status, resume_token),
        response => panic!("unexpected status response: {response:?}"),
    }
}

/// Polls a sign-in until it stops being pending.
fn await_sign_in(connection: &InProcessConnection, login_id: &str) -> GitHubSignInStatus {
    await_sign_in_with_credential(connection, login_id).0
}

/// Polls a sign-in until it stops being pending, keeping the credential its
/// first `signed_in` poll carries.
fn await_sign_in_with_credential(
    connection: &InProcessConnection,
    login_id: &str,
) -> (GitHubSignInStatus, Option<String>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (status, credential) = poll_sign_in(connection, login_id);
        if !matches!(status, GitHubSignInStatus::Pending) {
            return (status, credential);
        }
        assert!(Instant::now() < deadline, "the sign-in did not settle");
        thread::sleep(Duration::from_millis(5));
    }
}

fn participant_of(connection: &InProcessConnection) -> Option<ParticipantIdentity> {
    match access(connection, AccessRequest::GetCurrentParticipant).unwrap() {
        AccessResponse::CurrentParticipant { participant } => participant,
        response => panic!("unexpected participant response: {response:?}"),
    }
}

fn pre_login_backend(config: GitHubSignInConfig) -> Arc<InProcessBackend> {
    let backend = InProcessBackend::new();
    backend.set_github_sign_in_config(config);
    backend
}

/// A pre-login backend that attached `store` as a remote listener does, so a
/// sign-in can hand out a credential for a later connection.
fn pre_login_backend_with_store(
    config: GitHubSignInConfig,
    store: &Arc<AuthTokenStore>,
) -> Arc<InProcessBackend> {
    let backend = pre_login_backend(config);
    backend.attach_person_token_store(Arc::clone(store));
    backend
}

/// Creates a workspace with one session in it, owned by nobody.
fn create_session(connection: &InProcessConnection, name: &str) -> AgentSessionId {
    let workspace = match send(
        connection,
        ClientRequest::Workspace(WorkspaceRequest::CreateWorkspace {
            name: name.to_owned(),
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
            name: name.to_owned(),
        }),
    )
    .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    }
}

#[test]
fn a_pre_login_connection_is_refused_everything_but_sign_in() {
    let backend = pre_login_backend(GitHubSignInConfig::new(["octocat".to_owned()]));
    let connection = backend.connect_pre_login();
    assert!(connection.awaits_sign_in());
    negotiate(&connection);

    // Reading what the connection is and what the worker can do is allowed.
    assert!(matches!(
        send(
            &connection,
            ClientRequest::Control(ControlRequest::DiscoverCapabilities)
        )
        .unwrap(),
        ServerResponse::Control(ControlResponse::Capabilities(_))
    ));
    assert!(matches!(
        send(
            &connection,
            ClientRequest::Control(ControlRequest::GetWorkerNodeStatus)
        )
        .unwrap(),
        ServerResponse::Control(ControlResponse::WorkerNodeStatus(_))
    ));
    assert_eq!(participant_of(&connection), None);

    // Everything else needs a person, and says so.
    for request in [
        ClientRequest::Workspace(WorkspaceRequest::ListWorkspaces),
        ClientRequest::Access(AccessRequest::ListParticipants),
        ClientRequest::Access(AccessRequest::ListProviderCredentials),
        ClientRequest::Repository(RepositoryRequest::ListClonedRepositories),
    ] {
        let error = send(&connection, request.clone()).unwrap_err();
        assert_eq!(error.code, ErrorCode::AuthenticationRequired, "{request:?}");
        assert!(error.message.contains("not signed in"), "{error}");
    }

    // Signing out of a connection that never signed in is a request error, not
    // a sign-out.
    assert_eq!(
        access(&connection, AccessRequest::SignOutGitHub)
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
}

#[test]
fn a_person_signs_in_with_github_and_polls_the_sign_in() {
    let fixture = GitHubFixture::start(1, 141030, "octocat");
    let backend = pre_login_backend(fixture.config(&["octocat"]));
    let connection = backend.connect_pre_login();
    negotiate(&connection);

    let login_id = start_sign_in(&connection);
    // A sign-in is readable only by the connection that started it: another
    // person polling the same id is told it does not exist.
    let tokens = AuthTokenStore::new();
    let stranger = ParticipantIdentity {
        id: loom_core::ParticipantId::new(),
        display_name: "stranger".to_owned(),
    };
    let issued = tokens
        .issue_for_participant(AuthorizationScope::all(), stranger)
        .unwrap();
    let other = backend.connect_authenticated(tokens.authenticate(&issued.token).unwrap());
    negotiate(&other);
    assert_eq!(
        access(
            &other,
            AccessRequest::GetGitHubSignInStatus {
                login_id: login_id.clone(),
            },
        )
        .unwrap_err()
        .code,
        ErrorCode::NotFound
    );

    let (status, credential) = await_sign_in_with_credential(&connection, &login_id);
    let expected = ParticipantIdentity {
        id: github_participant_id(141030),
        display_name: "octocat".to_owned(),
    };
    assert_eq!(
        status,
        GitHubSignInStatus::SignedIn {
            participant: expected.clone()
        }
    );
    assert_eq!(participant_of(&connection), Some(expected.clone()));
    assert!(!connection.awaits_sign_in());
    // An in-process backend has no listener, so it issues no credential for a
    // later connection: the person is signed in on this one only.
    assert_eq!(credential, None);
    // The person is in the directory, so an owner can grant them access.
    let AccessResponse::Participants { participants } =
        access(&connection, AccessRequest::ListParticipants).unwrap()
    else {
        panic!("expected the participant directory");
    };
    assert!(participants.contains(&expected));

    // The device flow asked only for the identity scope, and the GitHub token
    // was never stored as a provider credential.
    assert_eq!(
        fixture.served(3),
        vec!["/login/device/code", "/login/oauth/access_token", "/user"]
    );
    assert!(
        backend
            .provider_credentials
            .list(Some(expected.id))
            .is_empty(),
        "a sign-in must not store a provider credential"
    );
    fixture.join();
}

#[test]
fn the_same_github_account_gets_the_same_participant_id_on_a_second_sign_in() {
    let fixture = GitHubFixture::start(2, 141030, "octocat");
    let config = fixture.config(&["octocat"]);
    let backend = pre_login_backend(config);

    let mut ids = Vec::new();
    for _ in 0..2 {
        let connection = backend.connect_pre_login();
        negotiate(&connection);
        let login_id = start_sign_in(&connection);
        let GitHubSignInStatus::SignedIn { participant } = await_sign_in(&connection, &login_id)
        else {
            panic!("the second sign-in should bind the person");
        };
        assert_eq!(participant.id, github_participant_id(141030));
        ids.push(participant.id);
        // Signing out returns the connection to its pre-login state, so the
        // next connection starts from the same place.
        assert!(matches!(
            access(&connection, AccessRequest::SignOutGitHub).unwrap(),
            AccessResponse::SignedOutGitHub
        ));
    }
    assert_eq!(ids[0], ids[1]);
    fixture.join();
}

#[test]
fn a_login_outside_the_allowlist_is_refused() {
    let fixture = GitHubFixture::start(1, 999, "mallory");
    let backend = pre_login_backend(fixture.config(&["octocat"]));
    let connection = backend.connect_pre_login();
    negotiate(&connection);

    let login_id = start_sign_in(&connection);
    let GitHubSignInStatus::Failed { message } = await_sign_in(&connection, &login_id) else {
        panic!("a login outside the allowlist must fail");
    };
    assert!(message.contains("mallory"), "{message}");
    assert!(message.contains("not allowed"), "{message}");
    assert_eq!(participant_of(&connection), None);
    assert!(connection.awaits_sign_in());
    // The allowlist is case-insensitive, as GitHub logins are.
    assert!(backend.github_sign_in_config().allows_login("OctoCat"));
    assert!(!backend.github_sign_in_config().allows_login("mallory"));
    fixture.join();
}

#[test]
fn sign_in_pushes_the_roster_and_sign_out_removes_the_person() {
    let fixture = GitHubFixture::start(1, 141030, "octocat");
    let backend = InProcessBackend::new();
    let person = ParticipantIdentity {
        id: github_participant_id(141030),
        display_name: "octocat".to_owned(),
    };
    backend
        .register_participants(std::slice::from_ref(&person))
        .unwrap();
    backend.set_github_sign_in_config(fixture.config(&["octocat"]));

    // An operator owns a session and grants the person access to it.
    let operator = backend.connect();
    negotiate(&operator);
    let session_id = match send(
        &operator,
        ClientRequest::Workspace(WorkspaceRequest::CreateWorkspace {
            name: "Sign-in".to_owned(),
        }),
    )
    .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => {
            match send(
                &operator,
                ClientRequest::Workspace(WorkspaceRequest::CreateAgentSessionInWorkspace {
                    workspace_id: workspace.id,
                    name: "Pairing".to_owned(),
                }),
            )
            .unwrap()
            {
                ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => {
                    session.id
                }
                response => panic!("unexpected session response: {response:?}"),
            }
        }
        response => panic!("unexpected workspace response: {response:?}"),
    };
    access(
        &operator,
        AccessRequest::GrantSessionAccess {
            session_id,
            participant_id: person.id,
        },
    )
    .unwrap();

    let connection = backend.connect_pre_login();
    negotiate(&connection);
    let login_id = start_sign_in(&connection);
    assert!(matches!(
        await_sign_in(&connection, &login_id),
        GitHubSignInStatus::SignedIn { .. }
    ));

    // The sign-in pushed the roster of every session the person holds access
    // to, with the person counted as one live connection. The connection has
    // not sent a request since it was bound, so it shows as away rather than
    // online until it does.
    let roster = backend.journal().unwrap().rosters.get(&session_id).cloned();
    let roster = roster.expect("signing in must push the session roster");
    let ServerEvent::SessionRosterChanged { entries } = roster.event else {
        panic!("unexpected roster event");
    };
    let entry = entries
        .iter()
        .find(|entry| entry.participant == person)
        .expect("the signed-in person is in the roster");
    assert_eq!(entry.presence, PresenceStatus::Away);
    assert_eq!(entry.connections, 1);

    assert!(matches!(
        access(&connection, AccessRequest::SignOutGitHub).unwrap(),
        AccessResponse::SignedOutGitHub
    ));
    assert_eq!(participant_of(&connection), None);
    let roster = backend.journal().unwrap().rosters.get(&session_id).cloned();
    let Some(roster) = roster else {
        panic!("the roster event should be kept")
    };
    let ServerEvent::SessionRosterChanged { entries } = roster.event else {
        panic!("unexpected roster event");
    };
    let entry = entries
        .iter()
        .find(|entry| entry.participant == person)
        .expect("signing out keeps the grant");
    assert_eq!(entry.presence, PresenceStatus::Offline);
    assert_eq!(entry.connections, 0);

    // Signing out of a pre-login connection puts it back behind the gate.
    assert!(connection.awaits_sign_in());
    assert_eq!(
        send(
            &connection,
            ClientRequest::Workspace(WorkspaceRequest::ListWorkspaces)
        )
        .unwrap_err()
        .code,
        ErrorCode::AuthenticationRequired
    );
    fixture.join();
}

#[test]
fn an_operator_token_connection_stays_unbound_until_it_signs_in() {
    let fixture = GitHubFixture::start(1, 141030, "octocat");
    let backend = InProcessBackend::new();
    backend.set_github_sign_in_config(fixture.config(&["octocat"]));
    let tokens = Arc::new(AuthTokenStore::new());
    backend.attach_person_token_store(Arc::clone(&tokens));
    let issued = tokens.issue(AuthorizationScope::all()).unwrap();
    let connection = backend.connect_authenticated(tokens.authenticate(&issued.token).unwrap());
    negotiate(&connection);

    // An operator token is unbound: it is not a person and is not held to
    // session roles.
    assert_eq!(participant_of(&connection), None);
    assert!(!connection.awaits_sign_in());
    assert!(
        send(
            &connection,
            ClientRequest::Workspace(WorkspaceRequest::ListWorkspaces)
        )
        .is_ok()
    );

    let login_id = start_sign_in(&connection);
    assert!(matches!(
        await_sign_in(&connection, &login_id),
        GitHubSignInStatus::SignedIn { .. }
    ));
    assert_eq!(
        participant_of(&connection).map(|participant| participant.id),
        Some(github_participant_id(141030))
    );

    // Signing out releases the person but keeps the operator token, which a
    // sign-out never revokes.
    access(&connection, AccessRequest::SignOutGitHub).unwrap();
    assert_eq!(participant_of(&connection), None);
    assert!(tokens.authenticate(&issued.token).is_ok());
    assert!(
        send(
            &connection,
            ClientRequest::Workspace(WorkspaceRequest::ListWorkspaces)
        )
        .is_ok()
    );
    fixture.join();
}

#[test]
fn an_operator_token_resumes_a_remembered_identity() {
    let backend = InProcessBackend::new();
    let octocat = ParticipantIdentity {
        id: github_participant_id(141030),
        display_name: "octocat".to_owned(),
    };
    backend
        .register_participants(std::slice::from_ref(&octocat))
        .unwrap();
    let tokens = AuthTokenStore::new();
    let operator = tokens.issue(AuthorizationScope::all()).unwrap();
    let connection = backend.connect_authenticated(tokens.authenticate(&operator.token).unwrap());
    negotiate(&connection);

    // An id the worker has never seen is not resumed.
    let unknown = ParticipantIdentity {
        id: github_participant_id(7),
        display_name: "mallory".to_owned(),
    };
    assert_eq!(
        access(
            &connection,
            AccessRequest::ResumeGitHubIdentity {
                participant_id: unknown.id,
            },
        )
        .unwrap_err()
        .code,
        ErrorCode::NotFound
    );

    let resumed = access(
        &connection,
        AccessRequest::ResumeGitHubIdentity {
            participant_id: octocat.id,
        },
    )
    .unwrap();
    assert!(matches!(
        resumed,
        AccessResponse::GitHubIdentityResumed { participant } if participant == octocat
    ));
    assert_eq!(participant_of(&connection), Some(octocat.clone()));

    // A connection with no operator token cannot claim a remembered person.
    let pre_login = backend.connect_pre_login();
    negotiate(&pre_login);
    assert_eq!(
        access(
            &pre_login,
            AccessRequest::ResumeGitHubIdentity {
                participant_id: octocat.id,
            },
        )
        .unwrap_err()
        .code,
        ErrorCode::AuthenticationRequired
    );

    // A token already bound to a person is not an operator connection.
    let bound = tokens
        .issue_for_participant(AuthorizationScope::all(), octocat.clone())
        .unwrap();
    let bound_connection =
        backend.connect_authenticated(tokens.authenticate(&bound.token).unwrap());
    negotiate(&bound_connection);
    assert_eq!(
        access(
            &bound_connection,
            AccessRequest::ResumeGitHubIdentity {
                participant_id: octocat.id,
            },
        )
        .unwrap_err()
        .code,
        ErrorCode::Conflict
    );
}

#[test]
fn a_provider_login_binds_the_account_it_authorized() {
    let (endpoint, server) = serve_github_user(r#"{"id":141030,"login":"octocat"}"#);
    let config = GitHubSignInConfig::new(["octocat".to_owned()]).with_user_api_url(endpoint);
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate(&connection);

    let identity = connection
        .bind_provider_identity(&config, "provider-token")
        .expect("the allowed account binds the connection");

    assert_eq!(participant_of(&connection), Some(identity.clone()));
    assert_eq!(identity.id, github_participant_id(141030));
    assert_eq!(identity.display_name, "octocat");
    assert!(
        server.join().unwrap().starts_with("GET /user "),
        "the provider login reads the account over the configured endpoint"
    );
}

#[test]
fn a_provider_login_does_not_bind_a_disallowed_account() {
    let (endpoint, server) = serve_github_user(r#"{"id":141030,"login":"mallory"}"#);
    let config = GitHubSignInConfig::new(["octocat".to_owned()]).with_user_api_url(endpoint);
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate(&connection);

    assert!(
        connection
            .bind_provider_identity(&config, "provider-token")
            .is_none()
    );
    assert_eq!(participant_of(&connection), None);
    server.join().unwrap();
}

#[test]
fn only_an_unrestricted_operator_token_adopts_ownerless_sessions() {
    let backend = InProcessBackend::new();
    let tokens = AuthTokenStore::new();
    let connect = |scope: AuthorizationScope| {
        let issued = tokens.issue(scope).unwrap();
        backend.connect_authenticated(tokens.authenticate(&issued.token).unwrap())
    };

    // An operator token reaches every session and workspace, so a sign-in on it
    // adopts the sessions that have no owner.
    assert!(connect(AuthorizationScope::all()).adopts_ownerless_sessions());

    // A token narrowed to named workspaces or sessions reaches only those, so
    // its sign-in adopts nothing.
    let capability = CapabilitySet::new([Capability::GitHubSignIn]);
    assert!(
        !connect(AuthorizationScope::for_workspaces(
            [WorkspaceId::new()],
            capability.clone()
        ))
        .adopts_ownerless_sessions()
    );
    assert!(
        !connect(AuthorizationScope::for_sessions(
            [AgentSessionId::new()],
            capability
        ))
        .adopts_ownerless_sessions()
    );

    // A token already bound to a person cannot sign in again and must never be
    // treated as an operator, even when its scope reaches everything.
    let bound = tokens
        .issue_for_participant(
            AuthorizationScope::all(),
            ParticipantIdentity {
                id: github_participant_id(141030),
                display_name: "octocat".to_owned(),
            },
        )
        .unwrap();
    assert!(
        !backend
            .connect_authenticated(tokens.authenticate(&bound.token).unwrap())
            .adopts_ownerless_sessions()
    );

    // A pre-login connection carries no credential at all.
    assert!(!backend.connect_pre_login().adopts_ownerless_sessions());
}

#[test]
fn sign_in_is_refused_when_the_worker_does_not_allow_it() {
    let backend = pre_login_backend(GitHubSignInConfig::disabled());
    assert!(!backend.github_sign_in_enabled());
    let connection = backend.connect_pre_login();
    negotiate(&connection);
    let error = access(&connection, AccessRequest::StartGitHubSignIn).unwrap_err();
    assert_eq!(error.code, ErrorCode::UnsupportedCapability);
    assert!(error.message.contains("--allow-github-login"), "{error}");
}

#[test]
fn a_worker_without_a_configured_allowlist_accepts_any_github_account() {
    // The default policy lets a person sign in and then start with no session
    // grants, so a worker needs no configuration to be usable by people.
    let backend = InProcessBackend::new();
    let config = backend.github_sign_in_config();
    assert!(backend.github_sign_in_enabled());
    assert!(config.allows_login("anyone"));
    assert!(config.allows_login("OctoCat"));
}

#[test]
fn a_completed_sign_in_hands_out_its_credential_once() {
    let fixture = GitHubFixture::start(1, 141030, "octocat");
    let store = Arc::new(AuthTokenStore::new());
    let backend = pre_login_backend_with_store(fixture.config(&["octocat"]), &store);
    let connection = backend.connect_pre_login();
    negotiate(&connection);

    let login_id = start_sign_in(&connection);
    let (status, credential) = await_sign_in_with_credential(&connection, &login_id);
    let GitHubSignInStatus::SignedIn { participant } = status else {
        panic!("the sign-in should bind the person");
    };
    let credential = credential.expect("a served sign-in hands out a credential");
    let signed_in = store
        .authenticate(&credential)
        .expect("the handed-out credential authenticates on this worker");
    assert_eq!(signed_in.participant(), Some(&participant));
    assert_eq!(
        signed_in.scope(),
        &AuthorizationScope::all(),
        "the credential reaches what the person is granted, not more"
    );

    // The credential is delivered exactly once: a later poll reports the same
    // status and carries none, so it cannot be collected by polling again.
    let (later, repeated) = await_sign_in_with_credential(&connection, &login_id);
    assert_eq!(
        later,
        GitHubSignInStatus::SignedIn {
            participant: participant.clone(),
        }
    );
    assert_eq!(repeated, None);
    fixture.join();
}

#[test]
fn a_sign_in_credential_binds_the_person_and_their_session_roles() {
    let fixture = GitHubFixture::start(1, 141030, "octocat");
    let person = ParticipantIdentity {
        id: github_participant_id(141030),
        display_name: "octocat".to_owned(),
    };
    let backend = InProcessBackend::new();
    backend
        .register_participants(std::slice::from_ref(&person))
        .unwrap();
    backend.set_github_sign_in_config(fixture.config(&["octocat"]));
    let store = Arc::new(AuthTokenStore::new());
    backend.attach_person_token_store(Arc::clone(&store));

    // An operator owns two sessions and grants the person one of them.
    let operator = backend.connect();
    negotiate(&operator);
    let granted = create_session(&operator, "Pairing");
    let another = create_session(&operator, "Unrelated");
    access(
        &operator,
        AccessRequest::GrantSessionAccess {
            session_id: granted,
            participant_id: person.id,
        },
    )
    .unwrap();

    let connection = backend.connect_pre_login();
    negotiate(&connection);
    let login_id = start_sign_in(&connection);
    let (status, credential) = await_sign_in_with_credential(&connection, &login_id);
    assert!(matches!(status, GitHubSignInStatus::SignedIn { .. }));
    let credential = credential.expect("a served sign-in hands out a credential");
    fixture.join();

    // A later connection that presents the credential is the same person.
    let person_connection = backend.connect_authenticated(store.authenticate(&credential).unwrap());
    negotiate(&person_connection);
    assert_eq!(participant_of(&person_connection), Some(person.clone()));

    // It is not an operator: it adopts no ownerless session, it is held to the
    // person's roles, and it cannot claim a remembered identity it already is.
    assert!(!person_connection.adopts_ownerless_sessions());
    assert!(!person_connection.retains_operator_authority());
    assert_eq!(
        access(
            &person_connection,
            AccessRequest::ResumeGitHubIdentity {
                participant_id: person.id,
            },
        )
        .unwrap_err()
        .code,
        ErrorCode::Conflict
    );

    // It reads the session the person was granted and nothing else.
    assert!(
        send(
            &person_connection,
            ClientRequest::Session(SessionRequest::GetAgentSession {
                session_id: granted
            }),
        )
        .is_ok()
    );
    assert_eq!(
        send(
            &person_connection,
            ClientRequest::Session(SessionRequest::GetAgentSession {
                session_id: another
            }),
        )
        .unwrap_err()
        .code,
        ErrorCode::AuthorizationDenied
    );
}

#[test]
fn signing_out_revokes_the_credential_the_connection_signed_in_with() {
    let fixture = GitHubFixture::start(1, 141030, "octocat");
    let store = Arc::new(AuthTokenStore::new());
    let backend = pre_login_backend_with_store(fixture.config(&["octocat"]), &store);
    let connection = backend.connect_pre_login();
    negotiate(&connection);
    let login_id = start_sign_in(&connection);
    let (_, credential) = await_sign_in_with_credential(&connection, &login_id);
    let credential = credential.expect("a served sign-in hands out a credential");
    fixture.join();

    let person_connection = backend.connect_authenticated(store.authenticate(&credential).unwrap());
    negotiate(&person_connection);
    assert!(matches!(
        access(&person_connection, AccessRequest::SignOutGitHub).unwrap(),
        AccessResponse::SignedOutGitHub
    ));
    // The connection is no longer bound, and the credential it authenticated
    // with stops working, so signing out on one device ends it everywhere.
    assert_eq!(person_connection.participant(), None);
    assert_eq!(
        store.authenticate(&credential).unwrap_err().code,
        ErrorCode::AuthenticationFailed
    );
}

#[test]
fn signing_out_revokes_the_credential_a_pre_login_sign_in_issued() {
    let fixture = GitHubFixture::start(1, 141030, "octocat");
    let store = Arc::new(AuthTokenStore::new());
    let backend = pre_login_backend_with_store(fixture.config(&["octocat"]), &store);
    let connection = backend.connect_pre_login();
    negotiate(&connection);
    let login_id = start_sign_in(&connection);
    let (_, credential) = await_sign_in_with_credential(&connection, &login_id);
    let credential = credential.expect("a served sign-in hands out a credential");
    fixture.join();
    assert!(store.authenticate(&credential).is_ok());

    // This connection holds no credential of its own, so the one its sign-in
    // handed to the client is the only handle on that sign-in. Signing out
    // revokes it, which is what ends the kept sign-in on the other device too.
    assert!(matches!(
        access(&connection, AccessRequest::SignOutGitHub).unwrap(),
        AccessResponse::SignedOutGitHub
    ));
    assert_eq!(
        store.authenticate(&credential).unwrap_err().code,
        ErrorCode::AuthenticationFailed
    );
}

#[test]
fn a_sign_in_credential_survives_a_restart() {
    let directory = std::env::temp_dir().join(format!(
        "loom-server-sign-in-credential-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join("state.sqlite");
    let fixture = GitHubFixture::start(1, 141030, "octocat");

    let (credential, person) = {
        let backend = InProcessBackend::new_persistent(&path).unwrap();
        backend.set_github_sign_in_config(fixture.config(&["octocat"]));
        let store = Arc::new(AuthTokenStore::new());
        backend.attach_person_token_store(Arc::clone(&store));
        let connection = backend.connect_pre_login();
        negotiate(&connection);
        let login_id = start_sign_in(&connection);
        let (status, credential) = await_sign_in_with_credential(&connection, &login_id);
        let GitHubSignInStatus::SignedIn { participant } = status else {
            panic!("the sign-in should bind the person");
        };
        fixture.join();
        let credential = credential.expect("a served sign-in hands out a credential");
        backend.shutdown().unwrap();
        (credential, participant)
    };

    // A reopened worker restores the credential into the store its listener
    // attaches, with the person the directory kept.
    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let stored = backend
        .persistence
        .as_ref()
        .unwrap()
        .load_person_tokens()
        .unwrap();
    assert_eq!(stored.len(), 1, "the credential row is stored");
    let store = Arc::new(AuthTokenStore::new());
    backend.attach_person_token_store(Arc::clone(&store));
    let restored = store
        .authenticate(&credential)
        .expect("the stored credential authenticates after a restart");
    assert_eq!(restored.participant(), Some(&person));

    let connection = backend.connect_authenticated(restored);
    negotiate(&connection);
    assert_eq!(participant_of(&connection), Some(person));
    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn the_oldest_credential_is_revoked_when_a_person_signs_in_too_often() {
    let sign_ins = MAX_PERSON_TOKENS + 1;
    let fixture = GitHubFixture::start(sign_ins, 141030, "octocat");
    let store = Arc::new(AuthTokenStore::new());
    let backend = pre_login_backend_with_store(fixture.config(&["octocat"]), &store);
    let mut credentials = Vec::new();
    for _ in 0..sign_ins {
        let connection = backend.connect_pre_login();
        negotiate(&connection);
        let login_id = start_sign_in(&connection);
        let (status, credential) = await_sign_in_with_credential(&connection, &login_id);
        assert!(matches!(status, GitHubSignInStatus::SignedIn { .. }));
        credentials.push(credential.expect("each sign-in hands out a credential"));
    }
    fixture.join();

    // The worker keeps the most recent credentials and revokes the oldest, so
    // a client that never signs out cannot grow the store without bound.
    assert_eq!(
        store.authenticate(&credentials[0]).unwrap_err().code,
        ErrorCode::AuthenticationFailed
    );
    for credential in &credentials[1..] {
        assert!(
            store.authenticate(credential).is_ok(),
            "a recent credential still authenticates"
        );
    }
    assert_eq!(
        store
            .participant_token_ids(github_participant_id(141030))
            .unwrap()
            .len(),
        MAX_PERSON_TOKENS
    );
}
