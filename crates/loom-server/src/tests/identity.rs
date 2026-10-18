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

/// Polls a sign-in until it stops being pending.
fn await_sign_in(connection: &InProcessConnection, login_id: &str) -> GitHubSignInStatus {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = match access(
            connection,
            AccessRequest::GetGitHubSignInStatus {
                login_id: login_id.to_owned(),
            },
        )
        .expect("the sign-in is readable by its initiator")
        {
            AccessResponse::GitHubSignInStatus { status } => status,
            response => panic!("unexpected status response: {response:?}"),
        };
        if !matches!(status, GitHubSignInStatus::Pending) {
            return status;
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

    let status = await_sign_in(&connection, &login_id);
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
    let tokens = AuthTokenStore::new();
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

    // Signing out releases the person but keeps the operator token.
    access(&connection, AccessRequest::SignOutGitHub).unwrap();
    assert_eq!(participant_of(&connection), None);
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
