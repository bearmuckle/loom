use std::{
    collections::VecDeque,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::Command,
    thread,
    time::Duration,
};

use loom_context::ContextAssemblyOptions;
use loom_core::{
    AgentSessionId, Capability, CapabilitySet, EventSequence, PolicyDecision, ProtocolVersion,
    ToolCallId, WorkspaceId,
};
use loom_process::{TaskEvent, TaskKind, TaskSpec, TaskStatus, TerminalEvent};
use loom_protocol::{
    AgentActivityStatus, AgentInteractionStatus, ApprovalDecision, ClientRequest, ContextRequest,
    ControlRequest, ControlResponse, EventsRequest, EventsResponse, FilesystemRequest,
    FilesystemResponse, ProjectRequest, ProjectResponse, ProviderRequest, ProviderResponse,
    RepositoryRequest, RepositoryResponse, RequestEnvelope, RunRequest, RunResponse, ServerEvent,
    ServerResponse, SessionRequest, SessionResponse, TaskRequest, TaskResponse, TerminalRequest,
    TerminalResponse, UsageRequest, UsageResponse, WorkerNodeConfig, WorkspaceConfig,
    WorkspaceRequest, WorkspaceResponse,
};
use loom_providers::CredentialStore;
use loom_workspace::{WorkspaceControl, WorkspaceEdit};

use super::*;

#[test]
fn delegated_child_current_model_alias_resolves_to_manager_model() {
    let current_model = ModelId::new("provider/model");

    assert_eq!(
        delegated_child_model_id(None, &current_model),
        "provider/model"
    );
    assert_eq!(
        delegated_child_model_id(Some("current".to_owned()), &current_model),
        "provider/model"
    );
    assert_eq!(
        delegated_child_model_id(Some("  CURRENT  ".to_owned()), &current_model),
        "provider/model"
    );
    assert_eq!(
        delegated_child_model_id(Some("provider/other".to_owned()), &current_model),
        "provider/other"
    );
}

#[test]
fn api_key_provider_configuration_is_backend_scoped_and_recovers_without_persisting_the_secret_in_sqlite()
 {
    let root = std::env::temp_dir().join(format!("loom-api-key-scope-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let first_path = root.join("first.sqlite");
    let second_path = root.join("second.sqlite");
    let secret = "backend-one-provider-secret";

    let first = InProcessBackend::new_persistent(&first_path).unwrap();
    let connection = first.connect();
    negotiate_m3(&connection);
    let response = connection.request(RequestEnvelope::new(ClientRequest::Provider(
        ProviderRequest::ConfigureApiKeyProvider {
            provider_id: ProviderId::new("openai-compatible"),
            api_key: secret.to_owned(),
        },
    )));
    assert!(matches!(
        response.result,
        Ok(ServerResponse::Provider(
            ProviderResponse::ProviderConfigured
        ))
    ));
    let providers = first.provider_registry().list_providers().unwrap();
    let configured = providers
        .iter()
        .find(|provider| provider.id.as_str() == "openai-compatible")
        .unwrap();
    assert!(
        configured
            .credential_id
            .as_deref()
            .unwrap()
            .starts_with("api-key:")
    );
    drop(connection);
    drop(first);

    let credentials_path = first_path.with_extension("credentials.json");
    let credential_bytes = fs::read(&credentials_path).unwrap();
    assert!(
        credential_bytes
            .windows(secret.len())
            .any(|window| window == secret.as_bytes())
    );
    let database_bytes = fs::read(&first_path).unwrap();
    assert!(
        !database_bytes
            .windows(secret.len())
            .any(|window| window == secret.as_bytes())
    );

    let reopened = InProcessBackend::new_persistent(&first_path).unwrap();
    let recovered = reopened
        .provider_registry()
        .list_providers()
        .unwrap()
        .into_iter()
        .find(|provider| provider.id.as_str() == "openai-compatible")
        .unwrap();
    assert_eq!(recovered.credential_id, configured.credential_id);

    let separate = InProcessBackend::new_persistent(&second_path).unwrap();
    let separate_provider = separate
        .provider_registry()
        .list_providers()
        .unwrap()
        .into_iter()
        .find(|provider| provider.id.as_str() == "openai-compatible")
        .unwrap();
    assert_eq!(separate_provider.credential_id, None);

    drop(reopened);
    drop(separate);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn github_write_access_is_opt_in_and_round_trips() {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate_m3(&connection);

    let default = connection.request(RequestEnvelope::new(ClientRequest::Provider(
        ProviderRequest::GetGitHubWriteAccess,
    )));
    assert!(matches!(
        default.result,
        Ok(ServerResponse::Provider(
            ProviderResponse::GitHubWriteAccess { enabled: false }
        ))
    ));

    let enabled = connection.request(RequestEnvelope::new(ClientRequest::Provider(
        ProviderRequest::ConfigureGitHubWriteAccess { enabled: true },
    )));
    assert!(matches!(
        enabled.result,
        Ok(ServerResponse::Provider(
            ProviderResponse::GitHubWriteAccess { enabled: true }
        ))
    ));
    assert!(backend.provider_registry().github_write_access());

    let disabled = connection.request(RequestEnvelope::new(ClientRequest::Provider(
        ProviderRequest::ConfigureGitHubWriteAccess { enabled: false },
    )));
    assert!(matches!(
        disabled.result,
        Ok(ServerResponse::Provider(
            ProviderResponse::GitHubWriteAccess { enabled: false }
        ))
    ));
    assert!(!backend.provider_registry().github_write_access());
}

#[test]
fn github_repository_access_configures_the_account_token() {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate_m3(&connection);

    let before = connection.request(RequestEnvelope::new(ClientRequest::Provider(
        ProviderRequest::GetGitHubRepositoryAccess,
    )));
    assert!(matches!(
        before.result,
        Ok(ServerResponse::Provider(
            ProviderResponse::GitHubRepositoryAccess { connected: false }
        ))
    ));

    let configured = connection.request(RequestEnvelope::new(ClientRequest::Provider(
        ProviderRequest::ConfigureGitHubRepository {
            access_token: "gho_repository_secret".to_owned(),
        },
    )));
    assert!(matches!(
        configured.result,
        Ok(ServerResponse::Provider(
            ProviderResponse::ProviderConfigured
        ))
    ));
    assert_eq!(
        backend
            .provider_registry()
            .github_repository_token()
            .unwrap(),
        "gho_repository_secret"
    );
    assert_eq!(
        backend.provider_registry().github_account_token().unwrap(),
        "gho_repository_secret"
    );

    let after = connection.request(RequestEnvelope::new(ClientRequest::Provider(
        ProviderRequest::GetGitHubRepositoryAccess,
    )));
    assert!(matches!(
        after.result,
        Ok(ServerResponse::Provider(
            ProviderResponse::GitHubRepositoryAccess { connected: true }
        ))
    ));
}

#[test]
fn github_repository_login_status_reports_progress() {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate_m3(&connection);

    let now = std::time::Instant::now();
    backend
        .credentials
        .begin_pending(
            "repository-login".to_owned(),
            now,
            Duration::from_secs(300),
            8,
            now + Duration::from_secs(600),
        )
        .unwrap();

    let status_request = |login_id: &str| {
        connection.request(RequestEnvelope::new(ClientRequest::Provider(
            ProviderRequest::GetGitHubRepositoryLoginStatus {
                login_id: login_id.to_owned(),
            },
        )))
    };
    let pending = status_request("repository-login");
    assert!(matches!(
        pending.result,
        Ok(ServerResponse::Provider(
            ProviderResponse::GitHubRepositoryLoginStatus {
                status: GitHubCopilotLoginStatus::Pending
            }
        ))
    ));

    backend
        .credentials
        .finish("repository-login", GitHubCopilotLoginStatus::Configured);
    let configured = status_request("repository-login");
    assert!(matches!(
        configured.result,
        Ok(ServerResponse::Provider(
            ProviderResponse::GitHubRepositoryLoginStatus {
                status: GitHubCopilotLoginStatus::Configured
            }
        ))
    ));

    assert!(status_request("missing-login").result.is_err());
}

#[test]
fn opening_a_backend_migrates_legacy_openai_keys_to_its_scoped_store() {
    let root = std::env::temp_dir().join(format!("loom-legacy-provider-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let database = root.join("backend.sqlite");
    let legacy_path = root.join("legacy-credentials.json");
    let legacy_store = Arc::new(FileCredentialStore::open(&legacy_path).unwrap());
    let legacy_reference = CredentialRef::new("legacy-openai-key");
    let secret = "previously-stored-api-key";
    legacy_store
        .insert(legacy_reference.clone(), secret)
        .unwrap();

    let make_registry = || {
        let registry = ProviderRegistry::with_credentials(legacy_store.clone());
        registry
            .register(ProviderConfig::openai_compatible(
                "openai-compatible",
                "OpenAI-compatible model",
                "http://127.0.0.1:8000/v1/chat/completions",
                openai_compatible_descriptor(ModelId::new("gateway/model")),
                Some(legacy_reference.clone()),
            ))
            .unwrap();
        registry
    };

    let original =
        InProcessBackend::with_provider_registry_persistent(make_registry(), &database).unwrap();
    original.persist_state().unwrap();
    drop(original);

    let reopened =
        InProcessBackend::with_provider_registry_persistent(make_registry(), &database).unwrap();
    let provider = reopened
        .provider_registry()
        .list_providers()
        .unwrap()
        .into_iter()
        .find(|provider| provider.id.as_str() == "openai-compatible")
        .unwrap();
    assert!(
        provider
            .credential_id
            .as_deref()
            .is_some_and(|reference| reference.starts_with("api-key:"))
    );
    let scoped_path = database.with_extension("credentials.json");
    let scoped_contents = fs::read_to_string(&scoped_path).unwrap();
    assert!(scoped_contents.contains(secret));
    assert_eq!(legacy_store.resolve(&legacy_reference).unwrap(), secret);
    drop(reopened);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn worker_feed_pruning_threshold_catches_large_payloads_before_count_limit() {
    assert!(!should_prune_worker_feed(
        10,
        0,
        0,
        FEED_PRUNE_AFTER_NEW_BYTES - 1
    ));
    assert!(should_prune_worker_feed(
        10,
        0,
        0,
        FEED_PRUNE_AFTER_NEW_BYTES
    ));
    assert!(should_prune_worker_feed(
        64,
        0,
        0,
        FEED_PRUNE_AFTER_NEW_BYTES - 1
    ));
    assert!(should_prune_worker_feed(
        10,
        0,
        FEED_PRUNE_AFTER_NEW_BYTES / 2,
        FEED_PRUNE_AFTER_NEW_BYTES / 2
    ));
}

fn request_id_with_issued_at(issued_at_ms: u64) -> loom_core::RequestId {
    let mut bytes = *loom_core::RequestId::new().as_uuid().as_bytes();
    bytes[..6].copy_from_slice(&issued_at_ms.to_be_bytes()[2..]);
    loom_core::RequestId::from_uuid(uuid::Uuid::from_bytes(bytes))
}

#[test]
fn idempotency_cache_keeps_uuidv7_horizon_and_bounds_uuidv4_compatibility() {
    let now = Timestamp::now();
    let mut current = BTreeMap::new();
    for _ in 0..LEGACY_IDEMPOTENCY_RETENTION + 1 {
        let request_id = RequestId::new();
        current.insert(
            request_id,
            IdempotencyRecord {
                created_at: now,
                expires_at: request_id.issued_at_unix_millis().map(|issued_at| {
                    Timestamp::from_unix_millis(
                        issued_at + IDEMPOTENCY_RETENTION.as_millis() as u64,
                    )
                }),
                request: ClientRequest::Workspace(WorkspaceRequest::ListWorkspaces),
                response: ResponseEnvelope::success(
                    request_id,
                    ServerResponse::Workspace(WorkspaceResponse::WorkspaceConfigUpdated),
                ),
            },
        );
    }
    let expired_id = request_id_with_issued_at(
        now.as_unix_millis()
            .saturating_sub(IDEMPOTENCY_RETENTION.as_millis() as u64)
            .saturating_sub(1),
    );
    current.insert(
        expired_id,
        IdempotencyRecord {
            created_at: now,
            expires_at: Some(now),
            request: ClientRequest::Workspace(WorkspaceRequest::ListWorkspaces),
            response: ResponseEnvelope::success(
                expired_id,
                ServerResponse::Workspace(WorkspaceResponse::WorkspaceConfigUpdated),
            ),
        },
    );
    trim_idempotency_cache(&mut current);
    assert_eq!(current.len(), LEGACY_IDEMPOTENCY_RETENTION + 1);
    assert!(!current.contains_key(&expired_id));

    let mut legacy = BTreeMap::new();
    for _ in 0..LEGACY_IDEMPOTENCY_RETENTION + 1 {
        let request_id = RequestId::from_uuid(uuid::Uuid::new_v4());
        legacy.insert(
            request_id,
            IdempotencyRecord {
                created_at: now,
                expires_at: None,
                request: ClientRequest::Workspace(WorkspaceRequest::ListWorkspaces),
                response: ResponseEnvelope::success(
                    request_id,
                    ServerResponse::Workspace(WorkspaceResponse::WorkspaceConfigUpdated),
                ),
            },
        );
    }
    trim_idempotency_cache(&mut legacy);
    assert_eq!(legacy.len(), LEGACY_IDEMPOTENCY_RETENTION);
}

#[test]
fn resumable_runs_without_pending_tool_intent_are_deferred_on_restore() {
    for state in [
        AgentRunState::Planning,
        AgentRunState::Executing,
        AgentRunState::Evaluating,
        AgentRunState::AwaitingApproval,
        AgentRunState::NeedsInput,
        AgentRunState::Paused,
    ] {
        assert!(run_can_be_deferred_during_restore(
            state,
            Some(false),
            Some(false)
        ));
        assert!(!run_can_be_deferred_during_restore(
            state,
            Some(true),
            Some(false)
        ));
        assert!(!run_can_be_deferred_during_restore(
            state,
            None,
            Some(false)
        ));
    }
    for state in [
        AgentRunState::Completed,
        AgentRunState::Failed,
        AgentRunState::Cancelled,
    ] {
        assert!(!run_can_be_deferred_during_restore(
            state,
            Some(false),
            Some(false)
        ));
    }
}

#[test]
fn filesystem_change_response_detects_pruned_client_cursors() {
    let session_id = AgentSessionId::new();
    let changes = vec![SessionFilesystemChange {
        sequence: EventSequence::new(5),
        session_id,
        path: "src/main.rs".to_owned(),
        kind: loom_protocol::WorkspaceChangeKind::Modified,
        revision: Some("revision".to_owned()),
    }];
    assert!(filesystem_history_pruned(
        Some(EventSequence::new(1)),
        &changes
    ));
    assert!(!filesystem_history_pruned(
        Some(EventSequence::new(4)),
        &changes
    ));
    assert!(!filesystem_history_pruned(None, &changes));
}

#[test]
fn event_journal_retention_is_independent_per_session() {
    let first_session = AgentSessionId::new();
    let second_session = AgentSessionId::new();
    let mut journal = EventJournal::default();
    journal.set_retention(2);
    for (session_id, name) in [
        (first_session, "first-1"),
        (second_session, "second-1"),
        (first_session, "first-2"),
        (first_session, "first-3"),
    ] {
        journal.append_session(SessionEventRecord {
            sequence: EventSequence::default(),
            session_id,
            occurred_at: Timestamp::from_unix_millis(1),
            event: loom_core::SessionEvent::AgentSessionRenamed {
                session_id,
                name: name.to_owned(),
            },
        });
    }

    assert_eq!(journal.latest_sequence(None), Some(EventSequence::new(4)));
    assert_eq!(
        journal
            .events_since(Some(first_session), None)
            .iter()
            .map(|event| event.sequence.value())
            .collect::<Vec<_>>(),
        vec![3, 4]
    );
    assert_eq!(
        journal
            .events_since(Some(second_session), None)
            .iter()
            .map(|event| event.sequence.value())
            .collect::<Vec<_>>(),
        vec![2]
    );
    assert_eq!(journal.pending_events.len(), 3);
    assert_eq!(
        journal
            .pending_events
            .iter()
            .filter(|event| event.session_id == first_session)
            .count(),
        2
    );
}

#[test]
fn worker_feed_capture_and_acknowledgement_are_session_scoped() {
    let run_session = AgentSessionId::new();
    let other_session = AgentSessionId::new();
    let workspace_id = WorkspaceId::new();
    let mut journal = EventJournal::default();
    for (session_id, name) in [(run_session, "run"), (other_session, "other")] {
        journal.append_session(SessionEventRecord {
            sequence: EventSequence::default(),
            session_id,
            occurred_at: Timestamp::from_unix_millis(1),
            event: loom_core::SessionEvent::AgentSessionRenamed {
                session_id,
                name: name.to_owned(),
            },
        });
    }
    journal.append_workspace(
        workspace_id,
        WorkspaceEvent::Renamed {
            name: "workspace".to_owned(),
        },
    );

    let (captured_feed, captured_sequences) = journal.capture_session_feed(run_session);
    assert_eq!(captured_feed.events.len(), 1);
    assert_eq!(captured_feed.events[0].session_id, run_session);
    assert!(captured_feed.workspace_events.is_empty());

    // Capture is non-mutating; a failed persistence call can retry the
    // same capture because acknowledgement is a separate post-commit step.
    assert_eq!(journal.pending_events.len(), 2);
    assert!(
        journal
            .pending_events
            .iter()
            .any(|event| event.session_id == run_session)
    );
    assert!(
        journal
            .pending_events
            .iter()
            .any(|event| event.session_id == other_session)
    );
    assert_eq!(journal.pending_workspace_events.len(), 1);

    // After commit, only the captured session sequences are acknowledged.
    journal.acknowledge_session_feed(&captured_sequences);
    assert_eq!(journal.pending_events.len(), 1);
    assert_eq!(journal.pending_events[0].session_id, other_session);
    assert_eq!(journal.pending_workspace_events.len(), 1);
    assert_eq!(
        journal.pending_workspace_events[0].workspace_id,
        workspace_id
    );
}

fn negotiate(connection: &InProcessConnection) {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Control(
        ControlRequest::Negotiate {
            client_version: CURRENT_PROTOCOL_VERSION,
            capabilities: connection.backend.supported_capabilities.clone(),
        },
    )));
    assert!(matches!(
        response.result,
        Ok(ServerResponse::Control(ControlResponse::Negotiated(_)))
    ));
}

fn negotiate_m2(connection: &InProcessConnection) {
    negotiate(connection);
}

fn negotiate_m3(connection: &InProcessConnection) {
    negotiate(connection);
}

fn negotiate_m5(connection: &InProcessConnection) {
    negotiate(connection);
}

fn respond_http(mut stream: TcpStream, status: &str, body: &str) {
    let mut request = Vec::new();
    let mut byte = [0_u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        request.push(byte[0]);
    }
    let request = String::from_utf8_lossy(&request);
    let headers = request.to_ascii_lowercase();
    assert!(headers.contains("authorization: bearer fixture-token"));
    assert!(headers.contains("x-github-api-version: 2022-11-28"));
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
}

fn github_repository_json(name: &str) -> serde_json::Value {
    serde_json::json!({
        "full_name": name,
        "description": null,
        "clone_url": format!("https://github.com/{name}.git"),
        "private": false,
        "default_branch": "main"
    })
}

#[test]
fn github_repository_fetch_paginates_sorts_and_maps_api_records() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (first, _) = listener.accept().unwrap();
        let page_one = (0..100)
            .map(|index| github_repository_json(&format!("owner/repo-{index:03}")))
            .collect::<Vec<_>>();
        respond_http(first, "200 OK", &serde_json::to_string(&page_one).unwrap());
        let (second, _) = listener.accept().unwrap();
        respond_http(
            second,
            "200 OK",
            &serde_json::to_string(&vec![github_repository_json("owner/aaa")]).unwrap(),
        );
    });

    let repositories =
        fetch_github_repositories("fixture-token", &format!("http://{address}/user/repos"))
            .unwrap();
    server.join().unwrap();

    assert_eq!(repositories.len(), 101);
    assert_eq!(repositories.first().unwrap().full_name, "owner/aaa");
    assert_eq!(repositories.last().unwrap().full_name, "owner/repo-099");
    assert_eq!(
        repositories[1].clone_url,
        "https://github.com/owner/repo-000.git"
    );
    assert_eq!(repositories[1].default_branch, "main");
}

#[test]
fn github_repository_fetch_normalizes_transport_and_payload_errors() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (invalid_json, _) = listener.accept().unwrap();
        respond_http(invalid_json, "200 OK", "not-json");
        let (unauthorized, _) = listener.accept().unwrap();
        respond_http(unauthorized, "401 Unauthorized", "{}");
    });

    let endpoint = format!("http://{address}/user/repos");
    let malformed = fetch_github_repositories("fixture-token", &endpoint).unwrap_err();
    assert_eq!(malformed.code, ErrorCode::ProviderInvalidResponse);
    let unauthorized = fetch_github_repositories("fixture-token", &endpoint).unwrap_err();
    assert_eq!(unauthorized.code, ErrorCode::ProviderAuthentication);
    assert!(unauthorized.retryable);
    server.join().unwrap();
}

#[test]
fn filesystem_and_repository_helpers_reject_unsafe_inputs_and_copy_trees() {
    assert_eq!(
        checked_session_relative_path("nested/file.txt").unwrap(),
        PathBuf::from("nested/file.txt")
    );
    for invalid in [
        "",
        "  ",
        ".",
        "..",
        "../secret",
        "/absolute",
        "nested\\file",
    ] {
        assert!(
            checked_session_relative_path(invalid).is_err(),
            "{invalid:?}"
        );
    }

    for (url, safe) in [
        ("wss://worker.example/ws", true),
        ("ws://localhost:9000/", true),
        ("https://worker.example/ws", false),
        ("wss://", false),
        ("wss://user@worker.example/ws", false),
        ("wss://user:secret@worker.example/ws", false),
        ("wss://worker.example/ws#fragment", false),
        ("wss://worker.example/ws?access_TOKEN=secret", false),
    ] {
        assert_eq!(worker_node_url_is_safe(url), safe, "{url}");
    }

    assert_eq!(
        repository_display_name("https://github.com/owner/project.git").unwrap(),
        "project"
    );
    assert_eq!(
        repository_display_name("ssh://git@github.com/owner/project.git").unwrap(),
        "project"
    );
    for unsafe_source in [
        "relative/path",
        "http://github.com/owner/project",
        "https://user:secret@github.com/owner/project",
        "https://github.com/owner/project?access_token=secret",
    ] {
        assert!(
            repository_display_name(unsafe_source).is_err(),
            "{unsafe_source}"
        );
    }

    let source = workspace();
    let destination = workspace();
    fs::create_dir(source.join("nested")).unwrap();
    fs::write(source.join("nested/file.txt"), "copy me").unwrap();
    copy_filesystem_tree(&source, &destination).unwrap();
    assert_eq!(
        fs::read_to_string(destination.join("nested/file.txt")).unwrap(),
        "copy me"
    );
    assert_eq!(
        checked_session_path(&destination, "nested/file.txt").unwrap(),
        fs::canonicalize(destination.join("nested/file.txt")).unwrap()
    );
    assert!(checked_session_path(&destination, "../outside").is_err());
    fs::remove_dir_all(source).unwrap();
    fs::remove_dir_all(destination).unwrap();

    let repository = git_repository();
    assert_eq!(
        repository_display_name(repository.to_str().unwrap()).unwrap(),
        repository.file_name().unwrap().to_string_lossy()
    );
    fs::remove_dir_all(repository).unwrap();
}

#[test]
fn local_directory_import_copies_tree_and_rejects_unsafe_sources() {
    let source = workspace();
    let target_root = workspace();
    fs::create_dir(source.join("nested")).unwrap();
    fs::write(source.join("nested/file.txt"), "copied content").unwrap();
    let destination = target_root.join("imported");
    copy_directory_contents(&source, &destination).unwrap();
    assert_eq!(
        fs::read_to_string(destination.join("nested/file.txt")).unwrap(),
        "copied content"
    );
    assert_eq!(
        copy_directory_contents(&source, &destination)
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );

    let inside_source = source.join("session/imported");
    assert_eq!(
        copy_directory_contents(&source, &inside_source)
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    let file_source = source.join("nested/file.txt");
    assert_eq!(
        copy_directory_contents(&file_source, &target_root.join("file"))
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        copy_directory_contents(&source.join("missing"), &target_root.join("missing"))
            .unwrap_err()
            .code,
        ErrorCode::WorkspaceAccessDenied
    );
    fs::remove_dir_all(source).unwrap();
    fs::remove_dir_all(target_root).unwrap();
}

#[cfg(unix)]
#[test]
fn local_directory_import_rejects_symlinks_that_escape_source() {
    use std::os::unix::fs::symlink;

    let source = workspace();
    let outside = workspace();
    let destination_root = workspace();
    fs::write(outside.join("secret.txt"), "secret").unwrap();
    symlink(outside.join("secret.txt"), source.join("escape")).unwrap();
    assert_eq!(
        copy_directory_contents(&source, &destination_root.join("import"))
            .unwrap_err()
            .code,
        ErrorCode::WorkspaceAccessDenied
    );
    assert_eq!(fs::read_dir(&destination_root).unwrap().count(), 0);
    fs::remove_dir_all(source).unwrap();
    fs::remove_dir_all(outside).unwrap();
    fs::remove_dir_all(destination_root).unwrap();
}

#[cfg(unix)]
#[test]
fn filesystem_copy_preserves_symlinks_and_checked_paths_reject_escape() {
    use std::os::unix::fs::symlink;

    let source = workspace();
    let destination = workspace();
    let outside = workspace();
    fs::write(outside.join("secret.txt"), "secret").unwrap();
    symlink(outside.join("secret.txt"), source.join("outside-link")).unwrap();
    copy_filesystem_tree(&source, &destination).unwrap();
    assert!(
        fs::symlink_metadata(destination.join("outside-link"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(checked_session_path(&destination, "outside-link").is_err());

    fs::remove_dir_all(source).unwrap();
    fs::remove_dir_all(destination).unwrap();
    fs::remove_dir_all(outside).unwrap();
}

#[test]
fn bounded_review_text_is_unicode_safe_and_session_auth_errors_are_structured() {
    assert_eq!(bounded_review_text("short", 5), "short");
    assert_eq!(
        bounded_review_text("éclair", 2),
        "é\n...[review output truncated]"
    );
    assert_eq!(
        bounded_review_text("éclair", 1),
        "\n...[review output truncated]"
    );
    let error = unauthorized_session(AgentSessionId::new());
    assert_eq!(error.code, ErrorCode::AuthorizationDenied);
    assert!(!error.retryable);
    assert!(error.message.contains("not authorized for session"));
}

#[test]
fn worker_node_status_reports_capabilities_and_resources() {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate_m5(&connection);
    let response = connection.request(RequestEnvelope::new(ClientRequest::Control(
        ControlRequest::GetWorkerNodeStatus,
    )));
    let response =
        loom_protocol::decode_response(&loom_protocol::encode_response(&response).unwrap())
            .unwrap();
    let ServerResponse::Control(ControlResponse::WorkerNodeStatus(status)) =
        response.result.unwrap()
    else {
        panic!("expected worker node status");
    };
    assert!(status.online);
    assert!(status.resources.cpu_count > 0);
    assert_eq!(status.resources.cpu_usage_percent, None);
    assert!(
        status
            .resources
            .memory_total_bytes
            .is_some_and(|bytes| bytes > 0)
    );
    assert!(status.resources.memory_available_bytes.is_some());
    assert!(
        status
            .resources
            .memory_usage_percent
            .is_some_and(|value| value <= 100)
    );

    std::thread::sleep(Duration::from_millis(250));
    let refreshed = connection.request(RequestEnvelope::new(ClientRequest::Control(
        ControlRequest::GetWorkerNodeStatus,
    )));
    let refreshed =
        loom_protocol::decode_response(&loom_protocol::encode_response(&refreshed).unwrap())
            .unwrap();
    let ServerResponse::Control(ControlResponse::WorkerNodeStatus(refreshed)) =
        refreshed.result.unwrap()
    else {
        panic!("expected refreshed worker node status");
    };
    assert!(
        refreshed
            .resources
            .cpu_usage_percent
            .is_some_and(|value| value <= 100)
    );
    assert!(
        refreshed
            .resources
            .memory_total_bytes
            .is_some_and(|bytes| bytes > 0)
    );
    assert!(
        refreshed
            .resources
            .memory_usage_percent
            .is_some_and(|value| value <= 100)
    );
    assert_eq!(refreshed.resources.cpu_count, status.resources.cpu_count);
    assert_eq!(refreshed.node_id, status.node_id);
    assert_eq!(refreshed.name, status.name);
    assert_eq!(
        refreshed.resources.memory_total_bytes,
        status.resources.memory_total_bytes
    );
    assert!(status.capabilities.contains(Capability::ReadAgentSession));
}

#[test]
fn worker_resource_percentages_handle_unavailable_and_out_of_range_samples() {
    assert_eq!(cpu_usage_percent(f32::NAN), None);
    assert_eq!(cpu_usage_percent(-1.0), Some(0));
    assert_eq!(cpu_usage_percent(47.6), Some(48));
    assert_eq!(cpu_usage_percent(120.0), Some(100));
    assert_eq!(memory_usage_percent(None, Some(5)), None);
    assert_eq!(memory_usage_percent(Some(0), Some(0)), None);
    assert_eq!(memory_usage_percent(Some(100), Some(25)), Some(75));
    assert_eq!(memory_usage_percent(Some(100), Some(150)), Some(0));
}

#[test]
fn worker_resource_monitor_measures_cpu_utilization_after_a_baseline_sample() {
    let mut monitor = ResourceMonitor::default();

    assert_eq!(monitor.sample(None, None).cpu_usage_percent, None);
    std::thread::sleep(Duration::from_millis(250));
    assert!(
        monitor
            .sample(None, None)
            .cpu_usage_percent
            .is_some_and(|value| value <= 100)
    );
}

#[test]
fn workspace_config_is_persisted_and_excludes_access_tokens() {
    let path =
        std::env::temp_dir().join(format!("loom-workspace-config-{}.db", WorkspaceId::new()));
    let workspace_id;
    let config = WorkspaceConfig {
        revision: 1,
        cpu_pulse_threshold_percent: 37,
        project_agent_concurrency: 2,
        worker_nodes: vec![WorkerNodeConfig {
            url: "wss://worker.example/ws".to_owned(),
        }],
    };
    {
        let backend = InProcessBackend::new_persistent(&path).unwrap();
        let connection = backend.connect();
        negotiate_m5(&connection);
        let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Config test".to_owned(),
            },
        )));
        let Ok(ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace))) =
            created.result
        else {
            panic!("expected workspace creation");
        };
        workspace_id = workspace.id;
        let response = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::SetWorkspaceConfigForWorkspace {
                workspace_id: workspace.id,
                config: config.clone(),
            },
        )));
        assert!(matches!(
            response.result,
            Ok(ServerResponse::Workspace(
                WorkspaceResponse::WorkspaceConfigUpdated
            ))
        ));
        backend.shutdown().unwrap();
    }

    {
        let backend = InProcessBackend::new_persistent(&path).unwrap();
        let connection = backend.connect();
        negotiate_m5(&connection);
        let response = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::GetWorkspaceConfigForWorkspace { workspace_id },
        )));
        assert!(matches!(
            response.result,
            Ok(ServerResponse::Workspace(WorkspaceResponse::WorkspaceConfig(saved))) if saved == config
        ));
        let response = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::SetWorkspaceConfigForWorkspace {
                workspace_id,
                config: WorkspaceConfig {
                    revision: 0,
                    cpu_pulse_threshold_percent: 5,
                    project_agent_concurrency: 4,
                    worker_nodes: vec![WorkerNodeConfig {
                        url: "wss://stale.example/ws".to_owned(),
                    }],
                },
            },
        )));
        assert!(matches!(
            response.result,
            Ok(ServerResponse::Workspace(
                WorkspaceResponse::WorkspaceConfigUpdated
            ))
        ));
        let response = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::GetWorkspaceConfigForWorkspace { workspace_id },
        )));
        assert!(matches!(
            response.result,
            Ok(ServerResponse::Workspace(WorkspaceResponse::WorkspaceConfig(saved))) if saved == config
        ));
        for url in [
            "wss://worker.example/ws?%61ccess_token=secret",
            "wss://user:secret@worker.example/ws",
        ] {
            let response = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::SetWorkspaceConfigForWorkspace {
                    workspace_id,
                    config: WorkspaceConfig {
                        revision: 2,
                        cpu_pulse_threshold_percent: 5,
                        project_agent_concurrency: 4,
                        worker_nodes: vec![WorkerNodeConfig {
                            url: url.to_owned(),
                        }],
                    },
                },
            )));
            assert!(response.result.is_err());
        }
        let invalid_concurrency = connection.request(RequestEnvelope::new(
            ClientRequest::Workspace(WorkspaceRequest::SetWorkspaceConfigForWorkspace {
                workspace_id,
                config: WorkspaceConfig {
                    revision: 2,
                    project_agent_concurrency: 0,
                    ..WorkspaceConfig::default()
                },
            }),
        ));
        assert!(matches!(
            invalid_concurrency.result,
            Err(error) if error.code == ErrorCode::InvalidRequest
        ));
        backend.shutdown().unwrap();
    }
    std::fs::remove_file(path).unwrap();
}

/// Waits until a run stops needing the model, because a run is now driven by
/// its own worker rather than by the request that started it.
fn await_settled_run(
    connection: &InProcessConnection,
    run_id: loom_core::RunId,
) -> loom_agent::AgentRunSnapshot {
    // Wait on the run handle's idle condition instead of polling: the worker
    // signals `idle` when it stops running, so this wakes as soon as the run
    // settles rather than after an arbitrary sleep.
    if let Some(handle) = connection
        .backend
        .runs()
        .ok()
        .and_then(|runs| runs.get(&run_id).cloned())
    {
        let _ = handle.wait_until_idle_for(Duration::from_secs(10));
    }
    let mut last_snapshot = None;
    for _ in 0..1_000 {
        let response = connection.request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::GetAgentRun { run_id },
        )));
        let snapshot = match response.result {
            Ok(ServerResponse::Run(RunResponse::AgentRun(snapshot))) => snapshot,
            result => panic!("unexpected run response for {run_id}: {result:?}"),
        };
        last_snapshot = Some(snapshot.clone());
        if !matches!(
            snapshot.state,
            AgentRunState::Planning | AgentRunState::Executing | AgentRunState::Evaluating
        ) {
            return snapshot;
        }
        thread::sleep(Duration::from_millis(1));
    }
    let failure = connection
        .backend
        .runs()
        .ok()
        .and_then(|runs| runs.get(&run_id).cloned())
        .and_then(|handle| handle.failure());
    panic!("agent run did not settle: {last_snapshot:?}; failure: {failure:?}");
}

fn await_project_manager_wait_status(
    persistence: &dyn Persistence,
    wait_id: loom_core::ProjectManagerWaitId,
    expected_status: loom_core::ProjectManagerWaitStatus,
) -> loom_core::ProjectManagerWaitRecord {
    let mut last_status = None;
    for _ in 0..400 {
        let wait = persistence
            .load_project_manager_wait(wait_id)
            .unwrap()
            .expect("project manager wait should remain durable");
        last_status = Some(wait.status);
        if wait.status == expected_status {
            return wait;
        }
        thread::sleep(Duration::from_millis(1));
    }
    panic!(
        "project manager wait {wait_id} did not reach {expected_status:?}; last status: {last_status:?}"
    );
}

fn workspace() -> PathBuf {
    let root = std::env::temp_dir().join(format!("loom-server-{}", AgentSessionId::new()));
    fs::create_dir(&root).unwrap();
    root
}

fn git_repository() -> PathBuf {
    let root = workspace();
    let run = |arguments: &[&str]| {
        assert!(
            Command::new("git")
                .args(["-C", root.to_str().unwrap()])
                .args(arguments)
                .status()
                .unwrap()
                .success()
        );
    };
    run(&["init", "-q"]);
    run(&["config", "user.name", "Loom Test"]);
    run(&["config", "user.email", "loom@example.test"]);
    fs::write(root.join("README.md"), "source\n").unwrap();
    run(&["add", "--", "README.md"]);
    run(&["commit", "-qm", "initial"]);
    root
}

#[cfg(unix)]
#[test]
fn attaching_local_directory_uses_original_and_discovers_immediate_repositories() {
    let source = workspace();
    fs::write(source.join("note.txt"), "original").unwrap();
    fs::rename(git_repository(), source.join("child-repo")).unwrap();
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate_m5(&connection);
    let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "Local source".to_owned(),
        },
    )));
    let Ok(ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace))) =
        created.result
    else {
        panic!("expected workspace creation");
    };
    let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "Local source".to_owned(),
        },
    )));
    let Ok(ServerResponse::Session(SessionResponse::AgentSessionCreated(session))) = created.result
    else {
        panic!("expected session creation");
    };
    let attached = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::AttachSessionDirectory {
            session_id: session.id,
            source: source.display().to_string(),
            path: "sources/local".to_owned(),
        },
    )));
    let Ok(ServerResponse::Filesystem(FilesystemResponse::SessionDirectoryAttached {
        directory,
        repositories,
    })) = attached.result
    else {
        panic!("expected directory attachment: {:?}", attached.result);
    };
    assert_eq!(directory.source, source.display().to_string());
    assert_eq!(repositories.len(), 1);
    assert_eq!(repositories[0].path, "sources/local/child-repo");
    let edit = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::ApplySessionFilesystemEdit {
            session_id: session.id,
            edit: WorkspaceEdit {
                path: "sources/local/note.txt".to_owned(),
                old_text: "original".to_owned(),
                new_text: "changed".to_owned(),
                expected_revision: None,
            },
        },
    )));
    assert!(matches!(
        edit.result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::WorkspaceEditApplied(_)
        ))
    ));
    assert_eq!(
        fs::read_to_string(source.join("note.txt")).unwrap(),
        "changed"
    );
    let detached = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::DetachSessionDirectory {
            session_id: session.id,
            path: directory.path,
        },
    )));
    assert!(matches!(
        detached.result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::SessionDirectoryDetached
        ))
    ));
    assert!(source.join("child-repo/.git").exists());
    let repository_root = git_repository();
    let attached = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::AttachSessionDirectory {
            session_id: session.id,
            source: repository_root.display().to_string(),
            path: "sources/repo-root".to_owned(),
        },
    )));
    let Ok(ServerResponse::Filesystem(FilesystemResponse::SessionDirectoryAttached {
        directory,
        repositories,
    })) = attached.result
    else {
        panic!("expected repository root attachment");
    };
    assert_eq!(repositories.len(), 1);
    assert_eq!(repositories[0].path, "sources/repo-root");
    let detached = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::DetachSessionDirectory {
            session_id: session.id,
            path: directory.path,
        },
    )));
    assert!(matches!(
        detached.result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::SessionDirectoryDetached
        ))
    ));
    fs::remove_dir_all(repository_root).unwrap();
    fs::remove_dir_all(source).unwrap();
}

#[test]
fn workspace_sessions_get_independent_filesystems_and_repository_clones() {
    let source = git_repository();

    let backend = InProcessBackend::new();
    let connection = backend.connect();
    let capabilities = CapabilitySet::new([
        Capability::ManageWorkspaces,
        Capability::ReadAgentSession,
        Capability::CreateAgentSession,
        Capability::ReadSessionFilesystem,
        Capability::WriteSessionFilesystem,
        Capability::ManageSessionRepositories,
        Capability::ForkAgentSession,
        Capability::ReadVcsStatus,
    ]);
    let negotiated = connection.request(RequestEnvelope::new(ClientRequest::Control(
        ControlRequest::Negotiate {
            client_version: CURRENT_PROTOCOL_VERSION,
            capabilities,
        },
    )));
    assert!(matches!(
        negotiated.result,
        Ok(ServerResponse::Control(ControlResponse::Negotiated(_)))
    ));

    let workspace = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "Isolation test".to_owned(),
        },
    )));
    let Ok(ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace))) =
        workspace.result
    else {
        panic!("expected workspace creation");
    };
    let create_session = |name: &str| {
        let response = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: name.to_owned(),
            },
        )));
        let Ok(ServerResponse::Session(SessionResponse::AgentSessionCreated(session))) =
            response.result
        else {
            panic!("expected session creation");
        };
        session
    };
    let first = create_session("First");
    let second = create_session("Second");
    let attach_repository = |session_id| {
        let response = connection.request(RequestEnvelope::new(ClientRequest::Repository(
            RepositoryRequest::AttachSessionRepository {
                session_id,
                source: source.display().to_string(),
                path: "repo".to_owned(),
                revision: None,
            },
        )));
        let Ok(ServerResponse::Repository(RepositoryResponse::SessionRepositoryAttached(
            repository,
        ))) = response.result
        else {
            panic!("expected repository attachment");
        };
        repository
    };
    let first_repository = attach_repository(first.id);
    let second_repository = attach_repository(second.id);
    assert_ne!(first_repository.id, second_repository.id);

    let edit = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::ApplySessionFilesystemEdit {
            session_id: first.id,
            edit: WorkspaceEdit {
                path: "repo/README.md".to_owned(),
                old_text: "source".to_owned(),
                new_text: "first session".to_owned(),
                expected_revision: None,
            },
        },
    )));
    assert!(matches!(
        edit.result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::WorkspaceEditApplied(_)
        ))
    ));

    let fork = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::ForkAgentSession {
            session_id: first.id,
            name: "Forked first".to_owned(),
        },
    )));
    let Ok(ServerResponse::Session(SessionResponse::AgentSessionForked(fork))) = fork.result else {
        panic!("expected forked session");
    };
    let repositories = connection.request(RequestEnvelope::new(ClientRequest::Repository(
        RepositoryRequest::ListSessionRepositories {
            session_id: fork.id,
        },
    )));
    let Ok(ServerResponse::Repository(RepositoryResponse::SessionRepositories { repositories })) =
        repositories.result
    else {
        panic!("expected forked repositories");
    };
    let fork_repository = repositories.first().expect("repository was copied");
    assert_ne!(fork_repository.id, first_repository.id);
    let fork_edit = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::ApplySessionFilesystemEdit {
            session_id: fork.id,
            edit: WorkspaceEdit {
                path: "repo/README.md".to_owned(),
                old_text: "first session".to_owned(),
                new_text: "forked session".to_owned(),
                expected_revision: None,
            },
        },
    )));
    assert!(matches!(
        fork_edit.result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::WorkspaceEditApplied(_)
        ))
    ));
    for (session_id, expected_content) in
        [(first.id, "first session\n"), (fork.id, "forked session\n")]
    {
        let file = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
            FilesystemRequest::ReadSessionFile {
                session_id,
                path: "repo/README.md".to_owned(),
            },
        )));
        let Ok(ServerResponse::Filesystem(FilesystemResponse::SessionFilesystemFile(file))) =
            file.result
        else {
            panic!("expected session file");
        };
        assert_eq!(file.content, expected_content);
    }

    for (session_id, expected_content, repository_id, expected_clean) in [
        (first.id, "first session\n", first_repository.id, false),
        (second.id, "source\n", second_repository.id, true),
    ] {
        let file = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
            FilesystemRequest::ReadSessionFile {
                session_id,
                path: "repo/README.md".to_owned(),
            },
        )));
        let Ok(ServerResponse::Filesystem(FilesystemResponse::SessionFilesystemFile(file))) =
            file.result
        else {
            panic!("expected session file");
        };
        assert_eq!(file.content, expected_content);

        let status = connection.request(RequestEnvelope::new(ClientRequest::Repository(
            RepositoryRequest::GetSessionVcsStatus {
                session_id,
                repository_id,
            },
        )));
        let Ok(ServerResponse::Repository(RepositoryResponse::VcsStatus(status))) = status.result
        else {
            panic!("expected repository status");
        };
        assert_eq!(status.clean, expected_clean);
    }
    assert_eq!(
        fs::read_to_string(source.join("README.md")).unwrap(),
        "source\n"
    );
    fs::remove_dir_all(&backend.session_root_base).unwrap();
    fs::remove_dir_all(source).unwrap();
}

#[test]
fn session_filesystem_and_repository_metadata_survive_restart() {
    let source = git_repository();
    let state_dir = workspace();
    let persistence = state_dir.join("backend.sqlite");
    let (workspace_id, session_id, checkpoint_id) = {
        let backend = InProcessBackend::new_persistent(&persistence).unwrap();
        let connection = backend.connect();
        let capabilities = CapabilitySet::new([
            Capability::ManageWorkspaces,
            Capability::ReadAgentSession,
            Capability::CreateAgentSession,
            Capability::ReadSessionFilesystem,
            Capability::WriteSessionFilesystem,
            Capability::ManageCheckpoints,
            Capability::ManageSessionRepositories,
            Capability::ReadVcsStatus,
        ]);
        let negotiated = connection.request(RequestEnvelope::new(ClientRequest::Control(
            ControlRequest::Negotiate {
                client_version: CURRENT_PROTOCOL_VERSION,
                capabilities,
            },
        )));
        assert!(matches!(
            negotiated.result,
            Ok(ServerResponse::Control(ControlResponse::Negotiated(_)))
        ));
        let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Persistent workspace".to_owned(),
            },
        )));
        let Ok(ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace))) =
            created.result
        else {
            panic!("expected workspace creation");
        };
        let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Persistent session".to_owned(),
            },
        )));
        let Ok(ServerResponse::Session(SessionResponse::AgentSessionCreated(session))) =
            created.result
        else {
            panic!("expected session creation");
        };
        let attached = connection.request(RequestEnvelope::new(ClientRequest::Repository(
            RepositoryRequest::AttachSessionRepository {
                session_id: session.id,
                source: source.display().to_string(),
                path: "repo".to_owned(),
                revision: None,
            },
        )));
        assert!(
            matches!(
                attached.result,
                Ok(ServerResponse::Repository(
                    RepositoryResponse::SessionRepositoryAttached(_)
                ))
            ),
            "{:?}",
            attached.result
        );
        let checkpoint = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
            FilesystemRequest::CreateSessionCheckpoint {
                session_id: session.id,
                label: "before persistent edit".to_owned(),
            },
        )));
        let Ok(ServerResponse::Filesystem(FilesystemResponse::CheckpointCreated(checkpoint))) =
            checkpoint.result
        else {
            panic!("expected persisted checkpoint, got {:?}", checkpoint.result);
        };
        backend
            .session_filesystems()
            .unwrap()
            .get(&session.id)
            .unwrap()
            .apply_edit(WorkspaceEdit {
                path: "repo/README.md".to_owned(),
                old_text: "source".to_owned(),
                new_text: "persisted session edit".to_owned(),
                expected_revision: None,
            })
            .unwrap();
        backend
            .session_filesystems()
            .unwrap()
            .get(&session.id)
            .unwrap()
            .apply_edit(WorkspaceEdit {
                path: "repo/README.md".to_owned(),
                old_text: "persisted session edit".to_owned(),
                new_text: "temporary edit to undo".to_owned(),
                expected_revision: None,
            })
            .unwrap();
        backend
            .session_filesystems()
            .unwrap()
            .get(&session.id)
            .unwrap()
            .undo_last_agent_edit()
            .unwrap();
        backend
            .session_filesystems()
            .unwrap()
            .get(&session.id)
            .unwrap()
            .poll_changes()
            .unwrap();
        backend.flush().unwrap();
        backend.shutdown().unwrap();
        (workspace.id, session.id, checkpoint.id)
    };

    {
        let backend = InProcessBackend::new_persistent(&persistence).unwrap();
        let connection = backend.connect();
        let capabilities = CapabilitySet::new([
            Capability::ReadAgentSession,
            Capability::ReadSessionFilesystem,
            Capability::WriteSessionFilesystem,
            Capability::ManageCheckpoints,
            Capability::ReadVcsStatus,
        ]);
        let negotiated = connection.request(RequestEnvelope::new(ClientRequest::Control(
            ControlRequest::Negotiate {
                client_version: CURRENT_PROTOCOL_VERSION,
                capabilities,
            },
        )));
        assert!(matches!(
            negotiated.result,
            Ok(ServerResponse::Control(ControlResponse::Negotiated(_)))
        ));
        let sessions = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::ListWorkspaceSessions {
                workspace_id,
                include_archived: false,
            },
        )));
        assert!(matches!(
            sessions.result,
            Ok(ServerResponse::Session(SessionResponse::AgentSessions{ sessions }))
                if sessions.iter().any(|session| session.id == session_id)
        ));
        let repositories = connection.request(RequestEnvelope::new(ClientRequest::Repository(
            RepositoryRequest::ListSessionRepositories { session_id },
        )));
        let Ok(ServerResponse::Repository(RepositoryResponse::SessionRepositories {
            repositories,
        })) = repositories.result
        else {
            panic!("expected restored repository metadata");
        };
        let repository = repositories.first().expect("repository was restored");
        let persisted_filesystem = backend
            .persistence
            .as_ref()
            .unwrap()
            .load_filesystem_record(session_id)
            .unwrap()
            .expect("filesystem record remains inspectable");
        assert!(persisted_filesystem.edits.iter().any(|edit| {
            edit.path == "repo/README.md" && edit.before.as_deref() == Some("source\n")
        }));
        assert_eq!(
            persisted_filesystem.edits.len(),
            1,
            "undone edit ID is deleted from durable history"
        );
        assert!(
            backend
                .persistence
                .as_ref()
                .unwrap()
                .load_filesystem_changes_page(session_id, None, 512)
                .unwrap()
                .changes
                .iter()
                .any(|change| {
                    change.path == "repo/README.md"
                        && change.session_id == session_id
                        && change.sequence.value() > 0
                })
        );
        let file = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
            FilesystemRequest::ReadSessionFile {
                session_id,
                path: "repo/README.md".to_owned(),
            },
        )));
        let Ok(ServerResponse::Filesystem(FilesystemResponse::SessionFilesystemFile(file))) =
            file.result
        else {
            panic!("expected restored session file");
        };
        assert_eq!(file.content, "persisted session edit\n");
        assert_eq!(
            persisted_filesystem.checkpoints[0].files["repo/README.md"].expected_revision,
            file.revision
        );
        let reverted = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
            FilesystemRequest::RevertSessionCheckpoint {
                session_id,
                checkpoint_id,
            },
        )));
        assert!(
            matches!(
                reverted.result,
                Ok(ServerResponse::Filesystem(
                    FilesystemResponse::CheckpointReverted(_)
                ))
            ),
            "checkpoint revert failed: {:?}",
            reverted.result
        );
        let restored = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
            FilesystemRequest::ReadSessionFile {
                session_id,
                path: "repo/README.md".to_owned(),
            },
        )));
        let Ok(ServerResponse::Filesystem(FilesystemResponse::SessionFilesystemFile(restored))) =
            restored.result
        else {
            panic!("expected checkpoint file contents after revert");
        };
        assert_eq!(restored.content, "source\n");
        let status = connection.request(RequestEnvelope::new(ClientRequest::Repository(
            RepositoryRequest::GetSessionVcsStatus {
                session_id,
                repository_id: repository.id,
            },
        )));
        let Ok(ServerResponse::Repository(RepositoryResponse::VcsStatus(status))) = status.result
        else {
            panic!("expected restored repository status");
        };
        assert!(status.clean);
        backend.shutdown().unwrap();
    }

    fs::remove_dir_all(source).unwrap();
    fs::remove_dir_all(persistence.with_extension("session-roots")).unwrap();
    fs::remove_dir_all(state_dir).unwrap();
}

#[test]
fn forked_session_filesystem_and_policy_survive_restart_and_checkpoint_revert() {
    let source = git_repository();
    let state_dir = workspace();
    let persistence = state_dir.join("backend.sqlite");
    let (workspace_id, source_session_id, fork_session_id, checkpoint_id) = {
        let backend = InProcessBackend::new_persistent(&persistence).unwrap();
        let connection = backend.connect();
        negotiate_m5(&connection);
        let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Persistent fork workspace".to_owned(),
            },
        )));
        let Ok(ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace))) =
            created.result
        else {
            panic!("expected workspace creation");
        };
        let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Persistent source".to_owned(),
            },
        )));
        let Ok(ServerResponse::Session(SessionResponse::AgentSessionCreated(session))) =
            created.result
        else {
            panic!("expected source session creation");
        };
        let policy = ApprovalPolicy::auto_approve();
        let configured = connection.request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::SetSessionApprovalPolicy {
                session_id: session.id,
                policy: policy.clone(),
                auto_approve_actions: Some(true),
            },
        )));
        assert!(matches!(
            configured.result,
            Ok(ServerResponse::Session(SessionResponse::ApprovalPolicy(configured))) if configured == policy
        ));
        let attached = connection.request(RequestEnvelope::new(ClientRequest::Repository(
            RepositoryRequest::AttachSessionRepository {
                session_id: session.id,
                source: source.display().to_string(),
                path: "repo".to_owned(),
                revision: None,
            },
        )));
        assert!(matches!(
            attached.result,
            Ok(ServerResponse::Repository(
                RepositoryResponse::SessionRepositoryAttached(_)
            ))
        ));
        let edited = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
            FilesystemRequest::ApplySessionFilesystemEdit {
                session_id: session.id,
                edit: WorkspaceEdit {
                    path: "repo/README.md".to_owned(),
                    old_text: "source".to_owned(),
                    new_text: "source branch".to_owned(),
                    expected_revision: None,
                },
            },
        )));
        assert!(matches!(
            edited.result,
            Ok(ServerResponse::Filesystem(
                FilesystemResponse::WorkspaceEditApplied(_)
            ))
        ));
        let forked = connection.request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::ForkAgentSession {
                session_id: session.id,
                name: "Persistent fork".to_owned(),
            },
        )));
        let Ok(ServerResponse::Session(SessionResponse::AgentSessionForked(forked))) =
            forked.result
        else {
            panic!("expected fork creation: {:?}", forked.result);
        };
        let checkpoint = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
            FilesystemRequest::CreateSessionCheckpoint {
                session_id: forked.id,
                label: "fork baseline".to_owned(),
            },
        )));
        let Ok(ServerResponse::Filesystem(FilesystemResponse::CheckpointCreated(checkpoint))) =
            checkpoint.result
        else {
            panic!("expected fork checkpoint: {:?}", checkpoint.result);
        };
        backend
            .session_filesystems()
            .unwrap()
            .get(&forked.id)
            .unwrap()
            .apply_edit(WorkspaceEdit {
                path: "repo/README.md".to_owned(),
                old_text: "source branch".to_owned(),
                new_text: "fork-only change".to_owned(),
                expected_revision: None,
            })
            .unwrap();
        backend
            .session_filesystems()
            .unwrap()
            .get(&forked.id)
            .unwrap()
            .poll_changes()
            .unwrap();
        backend.flush().unwrap();
        backend.shutdown().unwrap();
        (workspace.id, session.id, forked.id, checkpoint.id)
    };

    {
        let backend = InProcessBackend::new_persistent(&persistence).unwrap();
        let connection = backend.connect();
        negotiate_m5(&connection);
        let sessions = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::ListWorkspaceSessions {
                workspace_id,
                include_archived: false,
            },
        )));
        let Ok(ServerResponse::Session(SessionResponse::AgentSessions { sessions })) =
            sessions.result
        else {
            panic!("expected restored sessions");
        };
        assert!(
            sessions
                .iter()
                .any(|session| session.id == source_session_id)
        );
        assert!(sessions.iter().any(|session| session.id == fork_session_id));

        let snapshot = connection.request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::GetAgentSessionSnapshot {
                session_id: fork_session_id,
            },
        )));
        let Ok(ServerResponse::Session(SessionResponse::AgentSessionSnapshot(snapshot))) =
            snapshot.result
        else {
            panic!("expected restored fork snapshot");
        };
        assert!(snapshot.auto_approve_actions);
        assert_eq!(snapshot.approval_policy, ApprovalPolicy::auto_approve());

        let source_repositories = connection.request(RequestEnvelope::new(
            ClientRequest::Repository(RepositoryRequest::ListSessionRepositories {
                session_id: source_session_id,
            }),
        ));
        let Ok(ServerResponse::Repository(RepositoryResponse::SessionRepositories {
            repositories: source_repositories,
        })) = source_repositories.result
        else {
            panic!("expected restored source repository metadata");
        };
        let fork_repositories = connection.request(RequestEnvelope::new(
            ClientRequest::Repository(RepositoryRequest::ListSessionRepositories {
                session_id: fork_session_id,
            }),
        ));
        let Ok(ServerResponse::Repository(RepositoryResponse::SessionRepositories {
            repositories: fork_repositories,
        })) = fork_repositories.result
        else {
            panic!("expected restored fork repository metadata");
        };
        assert_ne!(
            source_repositories.first().unwrap().id,
            fork_repositories.first().unwrap().id
        );

        let read_file = |session_id| {
            let response = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
                FilesystemRequest::ReadSessionFile {
                    session_id,
                    path: "repo/README.md".to_owned(),
                },
            )));
            let Ok(ServerResponse::Filesystem(FilesystemResponse::SessionFilesystemFile(file))) =
                response.result
            else {
                panic!("expected restored session file: {:?}", response.result);
            };
            file.content
        };
        assert_eq!(read_file(source_session_id), "source branch\n");
        assert_eq!(read_file(fork_session_id), "fork-only change\n");

        let events = connection.request(RequestEnvelope::new(ClientRequest::Events(
            EventsRequest::GetSessionEvents {
                session_id: Some(fork_session_id),
                workspace_id: None,
                after_sequence: None,
                stream_epoch: None,
            },
        )));
        let Ok(ServerResponse::Events(EventsResponse::SessionEvents { events, .. })) =
            events.result
        else {
            panic!("expected restored fork event stream");
        };
        assert!(events.iter().any(|event| {
            matches!(
                &event.event,
                loom_protocol::ServerEvent::AgentSessionForked {
                    source_session_id: source_id,
                    snapshot,
                } if *source_id == source_session_id && snapshot.id == fork_session_id
            )
        }));

        let reverted = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
            FilesystemRequest::RevertSessionCheckpoint {
                session_id: fork_session_id,
                checkpoint_id,
            },
        )));
        assert!(
            matches!(
                reverted.result,
                Ok(ServerResponse::Filesystem(
                    FilesystemResponse::CheckpointReverted(_)
                ))
            ),
            "fork checkpoint revert failed: {:?}",
            reverted.result
        );
        assert_eq!(read_file(fork_session_id), "source branch\n");
        assert_eq!(read_file(source_session_id), "source branch\n");
        backend.shutdown().unwrap();
    }

    fs::remove_dir_all(source).unwrap();
    fs::remove_dir_all(persistence.with_extension("session-roots")).unwrap();
    fs::remove_dir_all(state_dir).unwrap();
}

#[test]
fn persisted_session_filesystems_restore_lazily_and_survive_unrelated_writes() {
    let persistence =
        std::env::temp_dir().join(format!("loom-server-state-{}.db", WorkspaceId::new()));
    let session_root_base;
    let (workspace_id, session_id) = {
        let backend = InProcessBackend::new_persistent(&persistence).unwrap();
        session_root_base = backend.session_root_base.clone();
        let connection = backend.connect();
        negotiate_m3(&connection);
        let workspace = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Lazy restore workspace".to_owned(),
            },
        )));
        let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
            workspace.result.unwrap()
        else {
            panic!("unexpected workspace response");
        };
        let session = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Archived history".to_owned(),
            },
        )));
        let ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) =
            session.result.unwrap()
        else {
            panic!("unexpected session response");
        };
        let root = session_root_base
            .join(workspace.id.to_string())
            .join(session.id.to_string())
            .join("fs");
        fs::write(root.join("retained.txt"), "retained content\n").unwrap();
        backend.flush().unwrap();
        backend.shutdown().unwrap();
        (workspace.id, session.id)
    };

    let filesystem_root = session_root_base
        .join(workspace_id.to_string())
        .join(session_id.to_string())
        .join("fs");
    let parked_root = filesystem_root.with_extension("parked");
    fs::rename(&filesystem_root, &parked_root).unwrap();
    {
        let backend = InProcessBackend::new_persistent(&persistence).unwrap();
        let connection = backend.connect();
        negotiate_m3(&connection);
        let renamed = connection.request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::RenameAgentSession {
                session_id,
                name: "Still lazy".to_owned(),
            },
        )));
        assert!(matches!(
            renamed.result,
            Ok(ServerResponse::Session(
                SessionResponse::AgentSessionRenamed(_)
            ))
        ));
        backend.shutdown().unwrap();
    }
    fs::rename(&parked_root, &filesystem_root).unwrap();
    {
        let backend = InProcessBackend::new_persistent(&persistence).unwrap();
        let connection = backend.connect();
        negotiate_m3(&connection);
        let file = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
            FilesystemRequest::ReadSessionFile {
                session_id,
                path: "retained.txt".to_owned(),
            },
        )));
        let ServerResponse::Filesystem(FilesystemResponse::SessionFilesystemFile(file)) =
            file.result.unwrap()
        else {
            panic!("unexpected filesystem response");
        };
        assert_eq!(file.content, "retained content\n");
        backend.shutdown().unwrap();
        fs::remove_dir_all(&backend.session_root_base).unwrap();
    }
    let _ = fs::remove_file(&persistence);
}

#[test]
fn m5_session_projections_reconnect_and_archive_authoritatively() {
    let root = git_repository();
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate_m5(&connection);
    let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "Navigator".to_owned(),
        },
    )));
    let Ok(ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace))) =
        created.result
    else {
        panic!("expected workspace creation");
    };
    let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "Navigator session".to_owned(),
        },
    )));
    let session = match created.result.unwrap() {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot)) => snapshot,
        response => panic!("unexpected response: {response:?}"),
    };

    let attached = connection.request(RequestEnvelope::new(ClientRequest::Repository(
        RepositoryRequest::AttachSessionRepository {
            session_id: session.id,
            source: root.display().to_string(),
            path: "repo".to_owned(),
            revision: None,
        },
    )));
    assert!(matches!(
        attached.result,
        Ok(ServerResponse::Repository(
            RepositoryResponse::SessionRepositoryAttached(_)
        ))
    ));
    let workspaces = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::ListWorkspaces,
    )));
    let ServerResponse::Workspace(WorkspaceResponse::Workspaces { workspaces }) =
        workspaces.result.unwrap()
    else {
        panic!("unexpected workspace response");
    };
    assert_eq!(workspaces, vec![workspace.clone()]);

    let renamed = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::RenameAgentSession {
            session_id: session.id,
            name: "Renamed session".to_owned(),
        },
    )));
    let session = match renamed.result.unwrap() {
        ServerResponse::Session(SessionResponse::AgentSessionRenamed(snapshot)) => snapshot,
        response => panic!("unexpected rename response: {response:?}"),
    };
    assert_eq!(session.name, "Renamed session");

    let started = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRun {
            session_id: session.id,
            task: "inspect the workspace".to_owned(),
            model: loom_model::ModelId::new("deterministic/demo"),
            system_instructions: None,
            repository_instructions: None,
        },
    )));
    let run_id = match started.result.unwrap() {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected run response: {response:?}"),
    };

    let snapshot = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::GetAgentSessionSnapshot {
            session_id: session.id,
        },
    )));
    let ServerResponse::Session(SessionResponse::AgentSessionSnapshot(snapshot)) =
        snapshot.result.unwrap()
    else {
        panic!("unexpected session snapshot response");
    };
    assert_eq!(snapshot.session.id, session.id);
    assert_eq!(
        snapshot.active_run.as_ref().map(|run| run.run.id),
        Some(run_id)
    );
    assert!(snapshot.active_run.unwrap().plan.is_empty());

    let metadata = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::GetAgentSessionSnapshotMetadata {
            session_id: session.id,
        },
    )));
    let ServerResponse::Session(SessionResponse::AgentSessionSnapshot(metadata)) =
        metadata.result.unwrap()
    else {
        panic!("unexpected metadata snapshot response");
    };
    assert_eq!(
        metadata.active_run.as_ref().map(|run| run.run.id),
        Some(run_id)
    );
    assert!(
        metadata
            .active_run
            .as_ref()
            .is_some_and(|run| run.messages.is_empty())
    );
    let run = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunSnapshot { run_id },
    )));
    let ServerResponse::Run(RunResponse::AgentRunSnapshot(run)) = run.result.unwrap() else {
        panic!("unexpected run snapshot response");
    };
    assert_eq!(run.run.id, run_id);
    assert!(!run.messages.is_empty());

    let changes = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::GetSessionFilesystemChanges {
            session_id: session.id,
            after_sequence: None,
        },
    )));
    assert!(matches!(
        changes.result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::SessionFilesystemChanges { .. }
        ))
    ));

    let archived = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::ArchiveAgentSession {
            session_id: session.id,
        },
    )));
    assert!(matches!(
        archived.result,
        Ok(ServerResponse::Session(
            SessionResponse::AgentSessionArchived(_)
        ))
    ));
    let sessions = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::ListWorkspaceSessions {
            workspace_id: workspace.id,
            include_archived: false,
        },
    )));
    let ServerResponse::Session(SessionResponse::AgentSessions { sessions }) =
        sessions.result.unwrap()
    else {
        panic!("unexpected session list response");
    };
    assert!(sessions.is_empty());
    fs::remove_dir_all(&backend.session_root_base).unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn creates_session_and_reads_event_stream() {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate(&connection);

    let workspace = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "In-process workspace".to_owned(),
        },
    )));
    let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
        workspace.result.unwrap()
    else {
        panic!("unexpected workspace response");
    };
    let create = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "In-process demo".to_owned(),
        },
    )));
    let session_id = match create.result.unwrap() {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };

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
fn runs_deterministic_agent_through_approvals() {
    let root = git_repository();
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "M1 workspace".to_owned(),
        },
    )));
    let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
        workspace.result.unwrap()
    else {
        panic!("unexpected workspace response");
    };
    let session = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "M1 run".to_owned(),
        },
    )));
    let session_id = match session.result.unwrap() {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::SetSessionApprovalPolicy {
            session_id,
            policy: ApprovalPolicy::default(),
            auto_approve_actions: Some(false),
        },
    )));
    let attached = connection.request(RequestEnvelope::new(ClientRequest::Repository(
        RepositoryRequest::AttachSessionRepository {
            session_id,
            source: root.display().to_string(),
            path: "repo".to_owned(),
            revision: None,
        },
    )));
    assert!(
        matches!(
            attached.result,
            Ok(ServerResponse::Repository(
                RepositoryResponse::SessionRepositoryAttached(_)
            ))
        ),
        "{:?}",
        attached.result
    );
    let started = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRun {
            session_id,
            task: "create a demo file".to_owned(),
            model: ModelId::new("deterministic/demo"),
            system_instructions: Some("Be concise.".to_owned()),
            repository_instructions: Some("Keep changes focused.".to_owned()),
        },
    )));
    let run_id = match started.result.unwrap() {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };

    let mut after = None;
    loop {
        let response = connection.request(RequestEnvelope::new(ClientRequest::Events(
            EventsRequest::GetSessionEvents {
                session_id: Some(session_id),
                workspace_id: None,
                after_sequence: after,
                stream_epoch: None,
            },
        )));
        let ServerResponse::Events(EventsResponse::SessionEvents { events, .. }) =
            response.result.unwrap()
        else {
            panic!("unexpected response");
        };
        let mut completed = false;
        for event in &events {
            after = Some(event.sequence);
            if let ServerEvent::Agent {
                event:
                    AgentEvent::ToolApprovalRequired {
                        run_id: event_run,
                        attempt_id,
                        control_revision,
                        call,
                        ..
                    },
            } = &event.event
            {
                assert_eq!(*event_run, run_id);
                let response = connection.request(RequestEnvelope::new(ClientRequest::Run(
                    RunRequest::ApproveAgentAction {
                        run_id,
                        attempt_id: *attempt_id,
                        expected_control_revision: *control_revision,
                        tool_call_id: call.id,
                    },
                )));
                assert!(response.result.is_ok());
            }
            if matches!(
                &event.event,
                ServerEvent::Agent {
                    event: AgentEvent::RunCompleted { .. }
                }
            ) {
                completed = true;
            }
        }
        if completed {
            break;
        }
    }
    let final_run = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRun { run_id },
    )));
    let ServerResponse::Run(RunResponse::AgentRun(snapshot)) = final_run.result.unwrap() else {
        panic!("unexpected response");
    };
    assert_eq!(snapshot.state, AgentRunState::Completed);
    let page = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunMessagePage {
            run_id,
            before_ordinal: None,
            limit: 2,
        },
    )));
    let ServerResponse::Run(RunResponse::AgentRunMessagePage { messages, .. }) =
        page.result.unwrap()
    else {
        panic!("unexpected run message page response");
    };
    let oldest_ordinal = messages.last().unwrap().ordinal;
    let previous_page = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunMessagePage {
            run_id,
            before_ordinal: Some(oldest_ordinal),
            limit: 1,
        },
    )));
    let ServerResponse::Run(RunResponse::AgentRunMessagePage {
        messages: previous_messages,
        ..
    }) = previous_page.result.unwrap()
    else {
        panic!("unexpected previous run message page response");
    };
    assert!(
        previous_messages
            .iter()
            .all(|message| message.ordinal < oldest_ordinal)
    );
    let header = messages
        .iter()
        .find(|message| message.content_bytes > 0)
        .unwrap();
    let length = header.content_bytes.min(32) as u32;
    let content = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunMessageContentRange {
            run_id,
            message_ordinal: header.ordinal,
            byte_offset: 0,
            length,
        },
    )));
    let ServerResponse::Run(RunResponse::AgentRunMessageContentRange { content, .. }) =
        content.result.unwrap()
    else {
        panic!("unexpected run message content response");
    };
    assert_eq!(content.len(), length as usize);
    let beyond_content = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunMessageContentRange {
            run_id,
            message_ordinal: header.ordinal,
            byte_offset: u64::MAX,
            length: 8,
        },
    )));
    assert!(matches!(
        beyond_content.result,
        Ok(ServerResponse::Run(RunResponse::AgentRunMessageContentRange{ content, .. })) if content.is_empty()
    ));
    let missing_message = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunMessageContentRange {
            run_id,
            message_ordinal: oldest_ordinal + 1000,
            byte_offset: 0,
            length: 8,
        },
    )));
    assert_eq!(
        missing_message.result.unwrap_err().code,
        ErrorCode::NotFound
    );
    let empty_page = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunMessagePage {
            run_id,
            before_ordinal: Some(0),
            limit: 1,
        },
    )));
    assert!(matches!(
        empty_page.result,
        Ok(ServerResponse::Run(RunResponse::AgentRunMessagePage{ messages, .. })) if messages.is_empty()
    ));
    let history = connection.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetSessionEvents {
            session_id: Some(session_id),
            workspace_id: None,
            after_sequence: None,
            stream_epoch: None,
        },
    )));
    let ServerResponse::Events(EventsResponse::SessionEvents { events, .. }) =
        history.result.unwrap()
    else {
        panic!("unexpected history response");
    };
    assert!(events.iter().any(|event| {
        matches!(
            &event.event,
            ServerEvent::Agent {
                event: AgentEvent::ActivityRecorded { activity, .. }
            } if activity.run_id == run_id && activity.completed_at.is_some()
        )
    }));
    assert!(
        backend
            .session_root_base
            .join(workspace.id.to_string())
            .join(session_id.to_string())
            .join("fs/loom-m1-demo.txt")
            .is_file()
    );
    fs::remove_dir_all(&backend.session_root_base).unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn requires_negotiation_before_session_requests() {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    let response = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::ListWorkspaces,
    )));

    assert_eq!(response.result.unwrap_err().code, ErrorCode::InvalidRequest);
}

#[test]
fn unknown_run_is_structured_not_found() {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate(&connection);

    let response = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRun {
            run_id: loom_core::RunId::new(),
        },
    )));

    assert_eq!(response.result.unwrap_err().code, ErrorCode::NotFound);
}

#[test]
fn exposes_workspace_terminal_task_and_checkpoint_controls() {
    let root = git_repository();
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate_m2(&connection);
    let workspace = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "Filesystem controls".to_owned(),
        },
    )));
    let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
        workspace.result.unwrap()
    else {
        panic!("unexpected workspace response");
    };
    let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "Filesystem controls".to_owned(),
        },
    )));
    let ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) =
        created.result.unwrap()
    else {
        panic!("unexpected session response");
    };
    let session_id = session.id;
    let attached = connection.request(RequestEnvelope::new(ClientRequest::Repository(
        RepositoryRequest::AttachSessionRepository {
            session_id,
            source: root.display().to_string(),
            path: "repo".to_owned(),
            revision: None,
        },
    )));
    assert!(matches!(
        attached.result,
        Ok(ServerResponse::Repository(
            RepositoryResponse::SessionRepositoryAttached(_)
        ))
    ));
    let snapshot = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::GetSessionFilesystemSnapshot { session_id },
    )));
    let ServerResponse::Filesystem(FilesystemResponse::SessionFilesystemSnapshot(snapshot)) =
        snapshot.result.unwrap()
    else {
        panic!("unexpected filesystem snapshot");
    };
    assert!(
        snapshot
            .entries
            .iter()
            .any(|entry| entry.path == "repo/README.md")
    );
    let file = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::ReadSessionFile {
            session_id,
            path: "repo/README.md".to_owned(),
        },
    )));
    let revision = match file.result.unwrap() {
        ServerResponse::Filesystem(FilesystemResponse::SessionFilesystemFile(file)) => {
            file.revision
        }
        response => panic!("unexpected response: {response:?}"),
    };
    let checkpoint = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::CreateSessionCheckpoint {
            session_id,
            label: "before user edit".to_owned(),
        },
    )));
    let checkpoint_id = match checkpoint.result.unwrap() {
        ServerResponse::Filesystem(FilesystemResponse::CheckpointCreated(checkpoint)) => {
            checkpoint.id
        }
        response => panic!("unexpected response: {response:?}"),
    };
    let edit = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::ApplySessionFilesystemEdit {
            session_id,
            edit: WorkspaceEdit {
                path: "repo/README.md".to_owned(),
                old_text: "source".to_owned(),
                new_text: "user".to_owned(),
                expected_revision: Some(revision),
            },
        },
    )));
    assert!(matches!(
        edit.result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::WorkspaceEditApplied(_)
        ))
    ));
    let changes = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::GetSessionFilesystemChanges {
            session_id,
            after_sequence: None,
        },
    )));
    let ServerResponse::Filesystem(FilesystemResponse::SessionFilesystemChanges {
        changes, ..
    }) = changes.result.unwrap()
    else {
        panic!("unexpected filesystem changes response");
    };
    assert!(changes.iter().any(|event| event.path == "repo/README.md"));
    let revert = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::RevertSessionCheckpoint {
            session_id,
            checkpoint_id,
        },
    )));
    assert_eq!(
        revert.result.unwrap_err().code,
        ErrorCode::Conflict,
        "checkpoint revert must preserve the intervening user edit"
    );
    let file = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::ReadSessionFile {
            session_id,
            path: "repo/README.md".to_owned(),
        },
    )));
    assert!(matches!(
        file.result,
        Ok(ServerResponse::Filesystem(FilesystemResponse::SessionFilesystemFile(file))) if file.content == "user\n"
    ));

    let terminal_command = if cfg!(windows) {
        (
            "cmd".to_owned(),
            vec!["/C".to_owned(), "echo terminal".to_owned()],
        )
    } else {
        ("printf".to_owned(), vec!["terminal".to_owned()])
    };
    let terminal = connection.request(RequestEnvelope::new(ClientRequest::Terminal(
        TerminalRequest::OpenSessionTerminal {
            session_id,
            command: terminal_command.0,
            args: terminal_command.1,
            cwd: None,
        },
    )));
    let terminal_id = match terminal.result.unwrap() {
        ServerResponse::Terminal(TerminalResponse::TerminalOpened(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    let mut terminal_done = false;
    for _ in 0..100 {
        let events = connection.request(RequestEnvelope::new(ClientRequest::Terminal(
            TerminalRequest::GetSessionTerminalEvents {
                session_id,
                terminal_id,
                after_sequence: None,
            },
        )));
        let ServerResponse::Terminal(TerminalResponse::TerminalEvents { events }) =
            events.result.unwrap()
        else {
            panic!("unexpected terminal event response");
        };
        if events.iter().any(|event| {
            matches!(
                event.event,
                TerminalEvent::Exited {
                    status: loom_process::TerminalStatus::Exited,
                    ..
                }
            )
        }) {
            terminal_done = true;
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert!(terminal_done);

    let task_command = if cfg!(windows) {
        (
            "cmd".to_owned(),
            vec!["/C".to_owned(), "echo artifact>artifact.txt".to_owned()],
        )
    } else {
        (
            "sh".to_owned(),
            vec!["-c".to_owned(), "printf artifact > artifact.txt".to_owned()],
        )
    };
    let task = connection.request(RequestEnvelope::new(ClientRequest::Task(
        TaskRequest::StartSessionTask {
            session_id,
            spec: TaskSpec {
                kind: TaskKind::Test,
                label: "M2 task".to_owned(),
                command: task_command.0,
                args: task_command.1,
                cwd: Some("repo".to_owned()),
                output_limit_bytes: Some(4096),
                artifact_paths: vec!["repo/artifact.txt".to_owned()],
            },
        },
    )));
    let task_id = match task.result.unwrap() {
        ServerResponse::Task(TaskResponse::TaskStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    let mut task_done = false;
    for _ in 0..100 {
        let current = connection.request(RequestEnvelope::new(ClientRequest::Task(
            TaskRequest::GetSessionTask {
                session_id,
                task_id,
            },
        )));
        let ServerResponse::Task(TaskResponse::Task(snapshot)) = current.result.unwrap() else {
            panic!("unexpected task response");
        };
        if matches!(
            snapshot.status,
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
        ) {
            assert!(snapshot.artifacts[0].exists);
            task_done = true;
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert!(task_done);
    let listed = connection.request(RequestEnvelope::new(ClientRequest::Task(
        TaskRequest::ListSessionTasks { session_id },
    )));
    let ServerResponse::Task(TaskResponse::Tasks { tasks }) = listed.result.unwrap() else {
        panic!("unexpected task list response");
    };
    assert!(tasks.iter().any(|task| task.id == task_id));
    let task_events = connection.request(RequestEnvelope::new(ClientRequest::Task(
        TaskRequest::GetSessionTaskEvents {
            session_id,
            task_id,
            after_sequence: None,
        },
    )));
    let ServerResponse::Task(TaskResponse::TaskEvents { events }) = task_events.result.unwrap()
    else {
        panic!("unexpected task event response");
    };
    assert!(
        events
            .iter()
            .any(|event| matches!(event.event, TaskEvent::Completed { .. }))
    );

    let control = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::TakeSessionFilesystemControl {
            session_id,
            control: WorkspaceControl::User,
        },
    )));
    assert!(matches!(
        control.result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::WorkspaceControl(WorkspaceControl::User)
        ))
    ));
    fs::remove_dir_all(&backend.session_root_base).unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn policy_decisions_are_visible_and_can_stop_agent_writes() {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate_m2(&connection);
    let workspace = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "Policy workspace".to_owned(),
        },
    )));
    let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
        workspace.result.unwrap()
    else {
        panic!("unexpected workspace response");
    };
    let session = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "M2 policy".to_owned(),
        },
    )));
    let session_id = match session.result.unwrap() {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    let default_settings = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::GetAgentSessionSnapshot { session_id },
    )));
    let ServerResponse::Session(SessionResponse::AgentSessionSnapshot(default_settings)) =
        default_settings.result.unwrap()
    else {
        panic!("unexpected session snapshot response");
    };
    assert!(default_settings.auto_approve_actions);
    assert_eq!(
        default_settings.approval_policy,
        loom_core::ApprovalPolicy::auto_approve()
    );
    let other_session = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "Other session".to_owned(),
        },
    )));
    let other_session_id = match other_session.result.unwrap() {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    let policy = loom_core::ApprovalPolicy {
        write: PolicyDecision::Deny,
        ..Default::default()
    };
    let policy_response = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::SetSessionApprovalPolicy {
            session_id,
            policy,
            auto_approve_actions: Some(false),
        },
    )));
    assert!(matches!(
        policy_response.result,
        Ok(ServerResponse::Session(SessionResponse::ApprovalPolicy(_)))
    ));
    let other_settings = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::GetAgentSessionSnapshot {
            session_id: other_session_id,
        },
    )));
    let ServerResponse::Session(SessionResponse::AgentSessionSnapshot(other_settings)) =
        other_settings.result.unwrap()
    else {
        panic!("unexpected session snapshot response");
    };
    assert!(other_settings.auto_approve_actions);
    assert_eq!(
        other_settings.approval_policy,
        loom_core::ApprovalPolicy::auto_approve()
    );
    let started = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRun {
            session_id,
            task: "attempt a write".to_owned(),
            model: ModelId::new("deterministic/demo"),
            system_instructions: None,
            repository_instructions: None,
        },
    )));
    let run_id = match started.result.unwrap() {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    await_settled_run(&connection, run_id);
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
        panic!("unexpected session event response");
    };
    assert!(events.iter().any(|event| {
        matches!(
            &event.event,
            ServerEvent::Agent {
                event: loom_agent::AgentEvent::ToolPolicyEvaluated {
                    run_id: event_run,
                    evaluation,
                    ..
                }
            } if *event_run == run_id && evaluation.decision == PolicyDecision::Deny
        )
    }));
    let run = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRun { run_id },
    )));
    let ServerResponse::Run(RunResponse::AgentRun(snapshot)) = run.result.unwrap() else {
        panic!("unexpected run response");
    };
    assert_eq!(snapshot.state, AgentRunState::AwaitingApproval);
    fs::remove_dir_all(&backend.session_root_base).unwrap();
}

#[test]
fn persistent_backend_recovers_transcript_workspace_and_pending_approval() {
    let persistence =
        std::env::temp_dir().join(format!("loom-server-state-{}.db", WorkspaceId::new()));
    let session_root_base;
    let (session_id, run_id, approval) = {
        let backend = InProcessBackend::new_persistent(&persistence).unwrap();
        session_root_base = backend.session_root_base.clone();
        let connection = backend.connect();
        negotiate_m3(&connection);
        let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Durable workspace".to_owned(),
            },
        )));
        let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
            created.result.unwrap()
        else {
            panic!("unexpected workspace response");
        };
        let session = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "durable run".to_owned(),
            },
        )));
        let session_id = match session.result.unwrap() {
            ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot)) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        connection.request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::SetSessionApprovalPolicy {
                session_id,
                policy: ApprovalPolicy::default(),
                auto_approve_actions: Some(false),
            },
        )));
        let started = connection.request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::StartSessionAgentRunWithOptions {
                session_id,
                task: "create a demo file".to_owned(),
                model: ModelId::new("deterministic/demo"),
                system_instructions: Some("Be concise.".to_owned()),
                repository_instructions: Some("Keep changes focused.".to_owned()),
                limits: loom_core::SessionLimits {
                    max_tool_calls: Some(20),
                    ..Default::default()
                },
                context: ContextAssemblyOptions {
                    context_window: Some(8_192),
                    max_input_tokens: Some(4_096),
                    reserved_output_tokens: Some(1_024),
                },
            },
        )));
        let run_id = match started.result.unwrap() {
            ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        await_settled_run(&connection, run_id);
        backend.flush().unwrap();
        let persisted = FilePersistence::open(&persistence).unwrap();
        let runtime_config = persisted.load_run_runtime_config(run_id).unwrap().unwrap();
        let system_instructions = runtime_config.system_instructions.unwrap();
        assert!(system_instructions.starts_with("Be concise."));
        assert!(system_instructions.contains("project manager for this project"));
        assert_eq!(
            runtime_config.repository_instructions.as_deref(),
            Some("Keep changes focused.")
        );
        assert_eq!(runtime_config.context_options.context_window, Some(8_192));
        assert_eq!(runtime_config.limits.max_tool_calls, Some(20));
        let events = match connection
            .request(RequestEnvelope::new(ClientRequest::Events(
                EventsRequest::GetSessionEvents {
                    session_id: Some(session_id),
                    workspace_id: None,
                    after_sequence: None,
                    stream_epoch: None,
                },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Events(EventsResponse::SessionEvents { events, .. }) => events,
            response => panic!("unexpected response: {response:?}"),
        };
        let approval = events
            .iter()
            .find_map(|event| match &event.event {
                ServerEvent::Agent {
                    event:
                        AgentEvent::ToolApprovalRequired {
                            call,
                            attempt_id,
                            control_revision,
                            ..
                        },
                } => Some((call.id, *attempt_id, *control_revision)),
                _ => None,
            })
            .unwrap();
        backend.shutdown().unwrap();
        (session_id, run_id, approval)
    };
    assert!(persistence.is_file());

    let backend = InProcessBackend::new_persistent(&persistence).unwrap();
    assert!(backend.journal().unwrap().events.is_empty());
    assert!(backend.runs().unwrap().is_empty());
    assert!(backend.persisted_runs().unwrap().contains_key(&run_id));
    let connection = backend.connect();
    negotiate_m3(&connection);
    let recovered_session = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::GetAgentSessionSnapshot { session_id },
    )));
    let ServerResponse::Session(SessionResponse::AgentSessionSnapshot(recovered_session)) =
        recovered_session.result.unwrap()
    else {
        panic!("unexpected recovered session snapshot response");
    };
    assert!(!recovered_session.auto_approve_actions);
    assert_eq!(recovered_session.approval_policy, ApprovalPolicy::default());
    let detail = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunSnapshot { run_id },
    )));
    let ServerResponse::Run(RunResponse::AgentRunSnapshot(detail)) = detail.result.unwrap() else {
        panic!("unexpected run snapshot response");
    };
    assert_eq!(detail.run.id, run_id);
    assert!(detail.messages.iter().any(|message| {
        message.role == loom_model::MessageRole::User
            && message.content.contains("create a demo file")
    }));
    assert!(detail.messages.iter().any(|message| {
        message.role == loom_model::MessageRole::Assistant && !message.tool_calls.is_empty()
    }));
    assert!(
        detail
            .activities
            .iter()
            .any(|activity| { activity.status == AgentActivityStatus::AwaitingApproval })
    );
    assert!(backend.runs().unwrap().is_empty());
    let recovered = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRun { run_id },
    )));
    let ServerResponse::Run(RunResponse::AgentRun(snapshot)) = recovered.result.unwrap() else {
        panic!("unexpected run response");
    };
    assert_eq!(snapshot.state, AgentRunState::AwaitingApproval);
    assert_eq!(snapshot.attempt_id, approval.1);
    assert_eq!(snapshot.control_revision, approval.2);
    let reconstructed_state = connection.run_handle(run_id).unwrap().state();
    let system_instructions = reconstructed_state.task.system_instructions.unwrap();
    assert!(system_instructions.starts_with("Be concise."));
    assert!(system_instructions.contains("project manager for this project"));
    assert_eq!(
        reconstructed_state.options.context.context_window,
        Some(8_192)
    );
    assert_eq!(reconstructed_state.options.limits.max_tool_calls, Some(20));
    let recovered_interactions = backend
        .persistence
        .as_ref()
        .unwrap()
        .load_run_interactions(run_id)
        .unwrap();
    assert!(recovered_interactions.iter().any(|interaction| {
        interaction.attempt_id == approval.1
            && interaction.control_revision == approval.2
            && interaction.tool_call_id == Some(approval.0)
            && interaction.status == AgentInteractionStatus::Pending
    }));
    let recovered_snapshot = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunSnapshot { run_id },
    )));
    let ServerResponse::Run(RunResponse::AgentRunSnapshot(projection)) =
        recovered_snapshot.result.unwrap()
    else {
        panic!("unexpected run snapshot response");
    };
    assert!(!projection.activities.is_empty());
    assert!(
        projection
            .activities
            .iter()
            .any(|activity| activity.status == AgentActivityStatus::AwaitingApproval)
    );
    let page = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunMessagePage {
            run_id,
            before_ordinal: None,
            limit: MAX_AGENT_RUN_MESSAGE_PAGE_SIZE,
        },
    )));
    let ServerResponse::Run(RunResponse::AgentRunMessagePage {
        run_id: page_run_id,
        messages,
    }) = page.result.unwrap()
    else {
        panic!("unexpected run message page response");
    };
    assert_eq!(page_run_id, run_id);
    assert!(!messages.is_empty());
    assert!(
        messages
            .windows(2)
            .all(|pair| pair[0].ordinal > pair[1].ordinal),
        "message page must be in descending keyset order"
    );
    let oversized_page = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunMessagePage {
            run_id,
            before_ordinal: None,
            limit: MAX_AGENT_RUN_MESSAGE_PAGE_SIZE + 1,
        },
    )));
    assert_eq!(
        oversized_page.result.unwrap_err().code,
        ErrorCode::InvalidRequest
    );
    let message_header = messages
        .iter()
        .find(|message| message.content_bytes > 0)
        .unwrap();
    let range = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunMessageContentRange {
            run_id,
            message_ordinal: message_header.ordinal,
            byte_offset: 0,
            length: message_header.content_bytes.min(32) as u32,
        },
    )));
    let ServerResponse::Run(RunResponse::AgentRunMessageContentRange {
        run_id: range_run_id,
        message_ordinal,
        byte_offset,
        content,
    }) = range.result.unwrap()
    else {
        panic!("unexpected run message content response");
    };
    assert_eq!(range_run_id, run_id);
    assert_eq!(message_ordinal, message_header.ordinal);
    assert_eq!(byte_offset, 0);
    assert!(!content.is_empty());
    let expected_content = projection.messages[message_ordinal as usize]
        .content
        .as_bytes();
    assert_eq!(content, expected_content[..content.len()]);
    let oversized_range = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunMessageContentRange {
            run_id,
            message_ordinal,
            byte_offset: 0,
            length: MAX_AGENT_RUN_MESSAGE_CONTENT_RANGE_BYTES + 1,
        },
    )));
    assert_eq!(
        oversized_range.result.unwrap_err().code,
        ErrorCode::InvalidRequest
    );
    let legacy_connection = backend.connect();
    let legacy_negotiation = legacy_connection.request(RequestEnvelope::new(
        ClientRequest::Control(ControlRequest::Negotiate {
            client_version: ProtocolVersion::new(2, 0),
            capabilities: backend.supported_capabilities.clone(),
        }),
    ));
    assert_eq!(
        legacy_negotiation.result.unwrap_err().code,
        ErrorCode::UnsupportedProtocol
    );
    let protocol_3_connection = backend.connect();
    let protocol_3_negotiation = protocol_3_connection.request(RequestEnvelope::new(
        ClientRequest::Control(ControlRequest::Negotiate {
            client_version: ProtocolVersion::new(3, 0),
            capabilities: backend.supported_capabilities.clone(),
        }),
    ));
    assert_eq!(
        protocol_3_negotiation.result.unwrap_err().code,
        ErrorCode::UnsupportedProtocol
    );
    let protocol_4_connection = backend.connect();
    let protocol_4_negotiation = protocol_4_connection.request(RequestEnvelope::with_version(
        ProtocolVersion::new(4, 1),
        ClientRequest::Control(ControlRequest::Negotiate {
            client_version: ProtocolVersion::new(4, 1),
            capabilities: backend.supported_capabilities.clone(),
        }),
    ));
    assert_eq!(
        protocol_4_negotiation.result.unwrap_err().code,
        ErrorCode::UnsupportedProtocol
    );
    let protocol_5_connection = backend.connect();
    let protocol_5_negotiation = protocol_5_connection.request(RequestEnvelope::with_version(
        ProtocolVersion::new(5, 0),
        ClientRequest::Control(ControlRequest::Negotiate {
            client_version: ProtocolVersion::new(5, 0),
            capabilities: backend.supported_capabilities.clone(),
        }),
    ));
    assert_eq!(
        protocol_5_negotiation.result.unwrap_err().code,
        ErrorCode::UnsupportedProtocol
    );
    let protocol_6_connection = backend.connect();
    let protocol_6_negotiation = protocol_6_connection.request(RequestEnvelope::with_version(
        ProtocolVersion::new(6, 0),
        ClientRequest::Control(ControlRequest::Negotiate {
            client_version: ProtocolVersion::new(6, 0),
            capabilities: backend.supported_capabilities.clone(),
        }),
    ));
    assert_eq!(
        protocol_6_negotiation.result.unwrap_err().code,
        ErrorCode::UnsupportedProtocol
    );
    let protocol_7_connection = backend.connect();
    let protocol_7_negotiation = protocol_7_connection.request(RequestEnvelope::with_version(
        ProtocolVersion::new(7, 0),
        ClientRequest::Control(ControlRequest::Negotiate {
            client_version: ProtocolVersion::new(7, 0),
            capabilities: backend.supported_capabilities.clone(),
        }),
    ));
    assert_eq!(
        protocol_7_negotiation.result.unwrap_err().code,
        ErrorCode::UnsupportedProtocol
    );
    let protocol_8_connection = backend.connect();
    let protocol_8_discovery = protocol_8_connection.request(RequestEnvelope::with_version(
        ProtocolVersion::new(8, 0),
        ClientRequest::Control(ControlRequest::DiscoverCapabilities),
    ));
    assert_eq!(
        protocol_8_discovery.result.unwrap_err().code,
        ErrorCode::UnsupportedProtocol
    );
    let protocol_8_negotiation = protocol_8_connection.request(RequestEnvelope::with_version(
        CURRENT_PROTOCOL_VERSION,
        ClientRequest::Control(ControlRequest::Negotiate {
            client_version: ProtocolVersion::new(8, 0),
            capabilities: backend.supported_capabilities.clone(),
        }),
    ));
    assert_eq!(
        protocol_8_negotiation.result.unwrap_err().code,
        ErrorCode::UnsupportedProtocol
    );
    let protocol_9_connection = backend.connect();
    let protocol_9_discovery = protocol_9_connection.request(RequestEnvelope::with_version(
        ProtocolVersion::new(9, 0),
        ClientRequest::Control(ControlRequest::DiscoverCapabilities),
    ));
    assert_eq!(
        protocol_9_discovery.result.unwrap_err().code,
        ErrorCode::UnsupportedProtocol
    );
    let protocol_9_negotiation = protocol_9_connection.request(RequestEnvelope::with_version(
        CURRENT_PROTOCOL_VERSION,
        ClientRequest::Control(ControlRequest::Negotiate {
            client_version: ProtocolVersion::new(9, 0),
            capabilities: backend.supported_capabilities.clone(),
        }),
    ));
    assert_eq!(
        protocol_9_negotiation.result.unwrap_err().code,
        ErrorCode::UnsupportedProtocol
    );

    let capability_limited_connection = backend.connect();
    let capability_limited = CapabilitySet::new(
        backend
            .supported_capabilities
            .iter()
            .copied()
            .filter(|capability| *capability != Capability::ReadAgentRunMessages),
    );
    let current_negotiation = capability_limited_connection.request(RequestEnvelope::new(
        ClientRequest::Control(ControlRequest::Negotiate {
            client_version: CURRENT_PROTOCOL_VERSION,
            capabilities: capability_limited,
        }),
    ));
    assert!(matches!(
        current_negotiation.result,
        Ok(ServerResponse::Control(ControlResponse::Negotiated(_)))
    ));
    let unsupported_page = capability_limited_connection.request(RequestEnvelope::new(
        ClientRequest::Run(RunRequest::GetAgentRunMessagePage {
            run_id,
            before_ordinal: None,
            limit: MAX_AGENT_RUN_MESSAGE_PAGE_SIZE,
        }),
    ));
    assert_eq!(
        unsupported_page.result.unwrap_err().code,
        ErrorCode::CapabilityDenied
    );
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
        panic!("unexpected events response");
    };
    assert!(events.len() >= 5);
    let checkpoint = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetRunCheckpoint { run_id },
    )));
    let ServerResponse::Run(RunResponse::RunCheckpoint(checkpoint)) = checkpoint.result.unwrap()
    else {
        panic!("unexpected checkpoint response");
    };
    assert_eq!(checkpoint.session_id, session_id);

    let wrong_attempt = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::ApproveAgentAction {
            run_id,
            attempt_id: loom_core::RunAttemptId::new(),
            expected_control_revision: approval.2,
            tool_call_id: approval.0,
        },
    )));
    assert_eq!(wrong_attempt.result.unwrap_err().code, ErrorCode::Conflict);
    let stale_revision = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::ApproveAgentAction {
            run_id,
            attempt_id: approval.1,
            expected_control_revision: approval.2.saturating_sub(1),
            tool_call_id: approval.0,
        },
    )));
    assert_eq!(stale_revision.result.unwrap_err().code, ErrorCode::Conflict);

    let response = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::ApproveAgentAction {
            run_id,
            attempt_id: approval.1,
            expected_control_revision: approval.2,
            tool_call_id: approval.0,
        },
    )));
    assert!(response.result.is_ok());
    await_settled_run(&connection, run_id);
    let resolved_interactions = backend
        .persistence
        .as_ref()
        .unwrap()
        .load_run_interactions(run_id)
        .unwrap();
    assert!(resolved_interactions.iter().any(|interaction| {
        interaction.tool_call_id == Some(approval.0)
            && interaction.status == AgentInteractionStatus::Approved
            && interaction.decision == Some(ApprovalDecision::Approved)
    }));
    let command_approval = (0..1_000)
        .find_map(|_| {
            let events = match connection
                .request(RequestEnvelope::new(ClientRequest::Events(
                    EventsRequest::GetSessionEvents {
                        session_id: Some(session_id),
                        workspace_id: None,
                        after_sequence: None,
                        stream_epoch: None,
                    },
                )))
                .result
                .unwrap()
            {
                ServerResponse::Events(EventsResponse::SessionEvents { events, .. }) => events,
                response => panic!("unexpected events response: {response:?}"),
            };
            let approval = events.iter().find_map(|event| match &event.event {
                ServerEvent::Agent {
                    event:
                        AgentEvent::ToolApprovalRequired {
                            call,
                            attempt_id,
                            control_revision,
                            ..
                        },
                } if call.name == "run_command" => Some((call.id, *attempt_id, *control_revision)),
                _ => None,
            });
            approval.or_else(|| {
                thread::sleep(Duration::from_millis(1));
                None
            })
        })
        .expect("run_command approval did not arrive");
    let response = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::ApproveAgentAction {
            run_id,
            attempt_id: command_approval.1,
            expected_control_revision: command_approval.2,
            tool_call_id: command_approval.0,
        },
    )));
    assert!(response.result.is_ok());
    let mut usage = match connection
        .request(RequestEnvelope::new(ClientRequest::Usage(
            UsageRequest::GetRunUsage { run_id },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Usage(UsageResponse::RunUsage { usage, .. }) => usage,
        response => panic!("unexpected usage response: {response:?}"),
    };
    for _ in 0..1_000 {
        if usage.input_tokens > 0 {
            break;
        }
        thread::sleep(Duration::from_millis(1));
        usage = match connection
            .request(RequestEnvelope::new(ClientRequest::Usage(
                UsageRequest::GetRunUsage { run_id },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Usage(UsageResponse::RunUsage { usage, .. }) => usage,
            response => panic!("unexpected usage response: {response:?}"),
        };
    }
    assert_eq!(usage.input_tokens, 240);
    assert_eq!(usage.output_tokens, 52);
    assert_eq!(usage.tool_calls, 3);
    let filesystem = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::GetSessionFilesystemSnapshot { session_id },
    )));
    assert!(matches!(
        filesystem.result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::SessionFilesystemSnapshot(_)
        ))
    ));
    backend.shutdown().unwrap();
    backend.shutdown().unwrap();
    assert_eq!(
        connection
            .request(RequestEnvelope::new(ClientRequest::Usage(
                UsageRequest::GetRunUsage { run_id }
            )))
            .result
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    drop(connection);
    drop(backend);
    let reopened = InProcessBackend::new_persistent(&persistence).unwrap();
    let reopened_connection = reopened.connect();
    negotiate_m3(&reopened_connection);
    let recovered_usage = reopened_connection.request(RequestEnvelope::new(ClientRequest::Usage(
        UsageRequest::GetRunUsage { run_id },
    )));
    let ServerResponse::Usage(UsageResponse::RunUsage { usage, .. }) =
        recovered_usage.result.unwrap()
    else {
        panic!("unexpected recovered usage response");
    };
    assert_eq!(usage.input_tokens, 240);
    assert_eq!(usage.output_tokens, 52);
    let before_retry = reopened_connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRun { run_id },
    )));
    let ServerResponse::Run(RunResponse::AgentRun(before_retry)) = before_retry.result.unwrap()
    else {
        panic!("unexpected run response before checkpoint retry");
    };
    let prior_attempts = reopened
        .persistence
        .as_ref()
        .unwrap()
        .load_run_attempts(run_id)
        .unwrap();
    assert_eq!(prior_attempts.len(), 1);
    assert_eq!(prior_attempts[0].id, before_retry.attempt_id);
    let retried = reopened_connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::RetryAgentFromCheckpoint {
            run_id,
            checkpoint_id: checkpoint.id,
        },
    )));
    let ServerResponse::Run(RunResponse::AgentRun(retried)) = retried.result.unwrap() else {
        panic!("unexpected checkpoint retry response");
    };
    assert_ne!(retried.attempt_id, before_retry.attempt_id);
    assert_eq!(
        await_settled_run(&reopened_connection, run_id).state,
        AgentRunState::AwaitingApproval
    );
    let after_retry = reopened_connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRun { run_id },
    )));
    let ServerResponse::Run(RunResponse::AgentRun(after_retry)) = after_retry.result.unwrap()
    else {
        panic!("unexpected run response after checkpoint retry");
    };
    assert_eq!(after_retry.attempt_id, retried.attempt_id);
    let mut attempts = Vec::new();
    for _ in 0..1_000 {
        attempts = reopened
            .persistence
            .as_ref()
            .unwrap()
            .load_run_attempts(run_id)
            .unwrap();
        if attempts
            .last()
            .is_some_and(|attempt| attempt.state == AgentRunState::AwaitingApproval)
        {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].id, before_retry.attempt_id);
    assert_eq!(attempts[0].number, 1);
    assert_eq!(attempts[1].id, retried.attempt_id);
    assert_eq!(attempts[1].number, 2);
    assert_eq!(attempts[1].state, AgentRunState::AwaitingApproval);
    reopened.shutdown().unwrap();
    drop(reopened_connection);
    drop(reopened);

    // Model a crash after execution started but before the runtime could
    // persist its paused recovery state.
    let persistence_store = FilePersistence::open(&persistence).unwrap();
    let mut summary = persistence_store.load_run_summary(run_id).unwrap().unwrap();
    summary.snapshot.state = AgentRunState::Executing;
    summary.snapshot.completed_at = None;
    let mut execution = persistence_store
        .load_run_execution_state(run_id)
        .unwrap()
        .unwrap();
    execution.state = AgentRunState::Executing;
    execution.pending_approval = None;
    execution.pending_input = None;
    summary.execution_state = Some(execution);
    let mut attempts = persistence_store.load_run_attempts(run_id).unwrap();
    let current_attempt = attempts.last_mut().unwrap();
    current_attempt.state = AgentRunState::Executing;
    current_attempt.completed_at = None;
    summary.attempts = Some(attempts);
    let summaries = BTreeMap::from([(run_id, summary)]);
    let sessions = persistence_store.load_sessions().unwrap().unwrap();
    persistence_store
        .save_state(DurableStateWrite {
            sessions: &sessions,
            workspaces: None,
            settings: None,
            workspace_configs: None,
            providers: None,
            usage: None,
            idempotency: None,
            run_summaries: Some(&summaries),
            run_runtime_configs: None,
            run_context_checkpoints: None,
            run_plans: None,
            run_messages: None,
            run_activities: None,
            filesystem_records: None,
            feed: None,
        })
        .unwrap();
    drop(persistence_store);

    let restored = InProcessBackend::new_persistent(&persistence).unwrap();
    assert!(restored.runs().unwrap().is_empty());
    assert_eq!(
        restored
            .persisted_runs()
            .unwrap()
            .get(&run_id)
            .unwrap()
            .snapshot
            .state,
        AgentRunState::Paused
    );
    let recovery_session_id = restored
        .persisted_runs()
        .unwrap()
        .get(&run_id)
        .unwrap()
        .snapshot
        .session_id;
    let restored_connection = restored.connect();
    negotiate_m3(&restored_connection);
    let metadata = restored_connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::GetAgentSessionSnapshotMetadata {
            session_id: recovery_session_id,
        },
    )));
    let ServerResponse::Session(SessionResponse::AgentSessionSnapshot(metadata)) =
        metadata.result.unwrap()
    else {
        panic!("unexpected metadata session snapshot response");
    };
    assert!(
        metadata
            .active_run
            .as_ref()
            .is_some_and(|projection| projection.messages.is_empty())
    );
    let initial = restored_connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::GetAgentSessionInitialState {
            session_id: recovery_session_id,
        },
    )));
    let ServerResponse::Session(SessionResponse::AgentSessionInitialState(initial)) =
        initial.result.unwrap()
    else {
        panic!("unexpected initial session state response");
    };
    assert_eq!(initial.cursor, initial.projection.latest_sequence);
    assert!(
        initial
            .projection
            .active_run
            .as_ref()
            .is_some_and(|projection| projection.messages.is_empty())
    );
    assert!(restored.runs().unwrap().is_empty());
    let page = restored_connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunMessagePage {
            run_id,
            before_ordinal: None,
            limit: 10,
        },
    )));
    let ServerResponse::Run(RunResponse::AgentRunMessagePage { messages, .. }) =
        page.result.unwrap()
    else {
        panic!("unexpected transcript page response");
    };
    assert!(!messages.is_empty());
    let first = messages.first().unwrap();
    assert!(first.content_bytes > 0);
    let content = restored_connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunMessageContentRange {
            run_id,
            message_ordinal: first.ordinal,
            byte_offset: 0,
            length: u32::try_from(first.content_bytes.min(128)).unwrap(),
        },
    )));
    let ServerResponse::Run(RunResponse::AgentRunMessageContentRange { content, .. }) =
        content.result.unwrap()
    else {
        panic!("unexpected transcript content response");
    };
    assert!(!content.is_empty());
    let invalid_page = restored_connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunTranscriptPage {
            run_id,
            before_ordinal: None,
            limit: 0,
        },
    )));
    assert_eq!(
        invalid_page.result.unwrap_err().code,
        ErrorCode::InvalidRequest
    );
    let transcript_page = restored_connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunTranscriptPage {
            run_id,
            before_ordinal: None,
            limit: MAX_AGENT_RUN_TRANSCRIPT_PAGE_SIZE,
        },
    )));
    let ServerResponse::Run(RunResponse::AgentRunTranscriptPage {
        messages,
        next_before,
        has_older,
        ..
    }) = transcript_page.result.unwrap()
    else {
        panic!("unexpected bounded transcript page response");
    };
    assert!(!messages.is_empty());
    assert!(
        messages
            .windows(2)
            .all(|pair| pair[0].ordinal < pair[1].ordinal)
    );
    assert_eq!(next_before, messages.first().map(|message| message.ordinal));
    assert!(!has_older);

    let projection = restored_connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunSnapshot { run_id },
    )));
    let ServerResponse::Run(RunResponse::AgentRunSnapshot(projection)) = projection.result.unwrap()
    else {
        panic!("unexpected lazily restored run snapshot response");
    };
    assert_eq!(projection.run.state, AgentRunState::Paused);
    assert!(!projection.messages.is_empty());
    assert!(restored.runs().unwrap().is_empty());
    let execution = restored
        .persistence
        .as_ref()
        .unwrap()
        .load_run_execution_state(run_id)
        .unwrap()
        .unwrap();
    assert_eq!(execution.state, AgentRunState::Paused);
    assert!(execution.pending_tool_execution.is_none());
    let events = restored_connection.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetSessionEvents {
            session_id: Some(recovery_session_id),
            workspace_id: None,
            after_sequence: None,
            stream_epoch: None,
        },
    )));
    let ServerResponse::Events(EventsResponse::SessionEvents { events, .. }) =
        events.result.unwrap()
    else {
        panic!("unexpected recovery event response");
    };
    assert!(events.iter().any(|event| matches!(
        &event.event,
        ServerEvent::Agent {
            event: AgentEvent::RunStateChanged {
                run_id: event_run_id,
                state: AgentRunState::Paused,
            }
        } if *event_run_id == run_id
    )));
    drop(restored);
    fs::remove_file(persistence).unwrap();
    fs::remove_dir_all(session_root_base).unwrap();
}

#[test]
fn completed_runs_keep_indexed_summaries_without_restoring_runtime_objects() {
    let persistence =
        std::env::temp_dir().join(format!("loom-server-state-{}.db", WorkspaceId::new()));
    let (run_id, session_root_base) = {
        let backend = InProcessBackend::new_persistent(&persistence).unwrap();
        let session_root_base = backend.session_root_base.clone();
        let connection = backend.connect();
        negotiate_m3(&connection);
        let workspace = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Run summary workspace".to_owned(),
            },
        )));
        let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
            workspace.result.unwrap()
        else {
            panic!("unexpected workspace response");
        };
        let session = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Completed history".to_owned(),
            },
        )));
        let ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) =
            session.result.unwrap()
        else {
            panic!("unexpected session response");
        };
        let started = connection.request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::StartSessionAgentRun {
                session_id: session.id,
                task: "answer briefly".to_owned(),
                model: ModelId::new("deterministic/demo"),
                system_instructions: None,
                repository_instructions: None,
            },
        )));
        let run_id = match started.result.unwrap() {
            ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        let settled = await_settled_run(&connection, run_id);
        assert_eq!(settled.state, AgentRunState::Completed);
        backend.flush().unwrap();
        backend.shutdown().unwrap();
        (run_id, session_root_base)
    };

    let backend = InProcessBackend::new_persistent(&persistence).unwrap();
    assert!(backend.runs().unwrap().is_empty());
    assert!(backend.persisted_runs().unwrap().is_empty());
    let connection = backend.connect();
    negotiate_m3(&connection);
    let response = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRun { run_id },
    )));
    assert!(matches!(
        response.result,
        Ok(ServerResponse::Run(RunResponse::AgentRun(snapshot))) if snapshot.state == AgentRunState::Completed
    ));
    let projection = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunSnapshot { run_id },
    )));
    assert!(matches!(
        projection.result,
        Ok(ServerResponse::Run(RunResponse::AgentRunSnapshot(snapshot)))
            if snapshot.run.state == AgentRunState::Completed && !snapshot.messages.is_empty()
    ));
    assert!(backend.runs().unwrap().is_empty());
    fs::remove_dir_all(session_root_base).unwrap();
    let _ = fs::remove_file(persistence);
}

#[test]
fn pause_resume_fork_and_provider_discovery_are_protocol_operations() {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate_m3(&connection);
    let workspace = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "Control workspace".to_owned(),
        },
    )));
    let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
        workspace.result.unwrap()
    else {
        panic!("unexpected workspace response");
    };
    let session = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "control run".to_owned(),
        },
    )));
    let session_id = match session.result.unwrap() {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::SetSessionApprovalPolicy {
            session_id,
            policy: ApprovalPolicy::default(),
            auto_approve_actions: Some(false),
        },
    )));
    let started = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRun {
            session_id,
            task: "control".to_owned(),
            model: ModelId::new("deterministic/demo"),
            system_instructions: None,
            repository_instructions: None,
        },
    )));
    let run_id = match started.result.unwrap() {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    await_settled_run(&connection, run_id);
    let paused = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::PauseAgentRun { run_id },
    )));
    let ServerResponse::Run(RunResponse::AgentRun(snapshot)) = paused.result.unwrap() else {
        panic!("unexpected pause response");
    };
    assert_eq!(snapshot.state, AgentRunState::Paused);
    let resumed = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::ResumeAgentRun { run_id },
    )));
    let ServerResponse::Run(RunResponse::AgentRun(snapshot)) = resumed.result.unwrap() else {
        panic!("unexpected resume response");
    };
    assert_eq!(snapshot.state, AgentRunState::AwaitingApproval);

    let forked = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::ForkAgentSession {
            session_id,
            name: "control fork".to_owned(),
        },
    )));
    let forked_id = match forked.result.unwrap() {
        ServerResponse::Session(SessionResponse::AgentSessionForked(snapshot)) => snapshot.id,
        response => panic!("unexpected fork response: {response:?}"),
    };
    assert_ne!(forked_id, session_id);
    let forked_settings = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::GetAgentSessionSnapshot {
            session_id: forked_id,
        },
    )));
    let ServerResponse::Session(SessionResponse::AgentSessionSnapshot(forked_settings)) =
        forked_settings.result.unwrap()
    else {
        panic!("unexpected forked session snapshot response");
    };
    assert!(!forked_settings.auto_approve_actions);
    assert_eq!(forked_settings.approval_policy, ApprovalPolicy::default());

    let providers = connection.request(RequestEnvelope::new(ClientRequest::Provider(
        ProviderRequest::ListProviders,
    )));
    let ServerResponse::Provider(ProviderResponse::Providers { providers }) =
        providers.result.unwrap()
    else {
        panic!("unexpected provider response");
    };
    assert!(
        providers
            .iter()
            .any(|provider| provider.kind == loom_providers::ProviderKind::Ollama)
    );
    assert!(
        providers
            .iter()
            .any(|provider| provider.kind == loom_providers::ProviderKind::Deterministic)
    );
    fs::remove_dir_all(&backend.session_root_base).unwrap();
}

#[test]
fn explicit_limits_and_context_inspection_are_durable_protocol_state() {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate_m3(&connection);
    let workspace = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "Limited workspace".to_owned(),
        },
    )));
    let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
        workspace.result.unwrap()
    else {
        panic!("unexpected workspace response");
    };
    let session = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "limited run".to_owned(),
        },
    )));
    let session_id = match session.result.unwrap() {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    let started = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRunWithOptions {
            session_id,
            task: "limited".to_owned(),
            model: ModelId::new("deterministic/demo"),
            system_instructions: Some("system".to_owned()),
            repository_instructions: Some("repository".to_owned()),
            limits: loom_core::SessionLimits {
                max_tool_calls: Some(0),
                ..Default::default()
            },
            context: ContextAssemblyOptions {
                context_window: Some(1_024),
                max_input_tokens: Some(512),
                reserved_output_tokens: Some(128),
            },
        },
    )));
    let run_id = match started.result.unwrap() {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    assert_eq!(
        await_settled_run(&connection, run_id).state,
        AgentRunState::Failed
    );
    let events = match connection
        .request(RequestEnvelope::new(ClientRequest::Events(
            EventsRequest::GetSessionEvents {
                session_id: Some(session_id),
                workspace_id: None,
                after_sequence: None,
                stream_epoch: None,
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Events(EventsResponse::SessionEvents { events, .. }) => events,
        response => panic!("unexpected response: {response:?}"),
    };
    assert!(events.iter().any(|event| {
        matches!(
            &event.event,
            ServerEvent::Agent {
                event: AgentEvent::RunLimitReached { status, .. }
            } if status.exceeded.contains(&loom_core::LimitKind::ToolCalls)
        )
    }));
    let usage = connection.request(RequestEnvelope::new(ClientRequest::Usage(
        UsageRequest::GetSessionUsage { session_id },
    )));
    let ServerResponse::Usage(UsageResponse::SessionUsage { usage, .. }) = usage.result.unwrap()
    else {
        panic!("unexpected session usage response");
    };
    assert_eq!(usage.tool_calls, 0);
    let context = connection.request(RequestEnvelope::new(ClientRequest::Context(
        ContextRequest::InspectAgentContext { run_id },
    )));
    assert_eq!(context.result.unwrap_err().code, ErrorCode::InvalidState);
    fs::remove_dir_all(&backend.session_root_base).unwrap();
}

#[test]
fn non_sqlite_persistence_file_is_rejected_without_fallback() {
    let path =
        std::env::temp_dir().join(format!("loom-server-malformed-{}.db", WorkspaceId::new()));
    fs::write(&path, br#"{"schema_version":1,"state":{"broken":true}}"#).unwrap();
    let error = match InProcessBackend::new_persistent(&path) {
        Ok(_) => panic!("non-SQLite persistence unexpectedly loaded"),
        Err(error) => error,
    };
    assert_eq!(error.code, ErrorCode::Persistence);
    assert!(!path.with_extension("json.legacy").exists());
    fs::remove_file(path).unwrap();
}

#[test]
fn m5_workspace_context_vcs_and_task_evidence_are_authoritative() {
    let root = workspace();
    fs::write(root.join("README.md"), "fn answer() {\n TODO\n}\n").unwrap();
    let git = |arguments: &[&str]| {
        assert!(
            Command::new("git")
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .env_remove("GIT_COMMON_DIR")
                .args(arguments)
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "loom@example.test"]);
    git(&["config", "user.name", "Loom Test"]);
    git(&["add", "--", "README.md"]);
    git(&["commit", "-qm", "initial"]);

    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate_m5(&connection);
    let workspace = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "Context workspace".to_owned(),
        },
    )));
    let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
        workspace.result.unwrap()
    else {
        panic!("unexpected workspace response");
    };
    let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "Context session".to_owned(),
        },
    )));
    let ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) =
        created.result.unwrap()
    else {
        panic!("unexpected session response");
    };
    let session_id = session.id;
    let attached = connection.request(RequestEnvelope::new(ClientRequest::Repository(
        RepositoryRequest::AttachSessionRepository {
            session_id,
            source: root.display().to_string(),
            path: "repo".to_owned(),
            revision: None,
        },
    )));
    assert!(matches!(
        attached.result,
        Ok(ServerResponse::Repository(
            RepositoryResponse::SessionRepositoryAttached(_)
        ))
    ));
    let context = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::GetSessionContextFiles { session_id },
    )));
    assert!(matches!(
        context.result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::ContextFiles { .. }
        ))
    ));
    let repositories = connection.request(RequestEnvelope::new(ClientRequest::Repository(
        RepositoryRequest::ListSessionRepositories { session_id },
    )));
    let ServerResponse::Repository(RepositoryResponse::SessionRepositories { repositories }) =
        repositories.result.unwrap()
    else {
        panic!("unexpected session repositories");
    };
    let vcs = connection.request(RequestEnvelope::new(ClientRequest::Repository(
        RepositoryRequest::GetSessionVcsStatus {
            session_id,
            repository_id: repositories[0].id,
        },
    )));
    assert!(matches!(
        vcs.result,
        Ok(ServerResponse::Repository(RepositoryResponse::VcsStatus(_)))
    ));

    let task = connection.request(RequestEnvelope::new(ClientRequest::Task(
        TaskRequest::StartSessionTask {
            session_id,
            spec: TaskSpec {
                kind: TaskKind::Test,
                label: "evidence fixture".to_owned(),
                command: if cfg!(windows) {
                    "cmd".to_owned()
                } else {
                    "printf".to_owned()
                },
                args: if cfg!(windows) {
                    vec!["/C".to_owned(), "ok".to_owned()]
                } else {
                    vec!["ok".to_owned()]
                },
                cwd: None,
                output_limit_bytes: Some(128),
                artifact_paths: Vec::new(),
            },
        },
    )));
    let task_id = match task.result.unwrap() {
        ServerResponse::Task(TaskResponse::TaskStarted(task)) => task.id,
        response => panic!("unexpected task response: {response:?}"),
    };
    for _ in 0..100 {
        let current = connection.request(RequestEnvelope::new(ClientRequest::Task(
            TaskRequest::GetSessionTask {
                session_id,
                task_id,
            },
        )));
        if let Ok(ServerResponse::Task(TaskResponse::Task(snapshot))) = current.result
            && matches!(
                snapshot.status,
                TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
            )
        {
            let evidence = connection.request(RequestEnvelope::new(ClientRequest::Task(
                TaskRequest::GetSessionTaskEvidence {
                    session_id,
                    task_id,
                },
            )));
            assert!(matches!(
                evidence.result,
                Ok(ServerResponse::Task(TaskResponse::TaskEvidence { .. }))
            ));
            fs::remove_dir_all(&backend.session_root_base).unwrap();
            fs::remove_dir_all(root).unwrap();
            return;
        }
        thread::sleep(Duration::from_millis(2));
    }
    panic!("task evidence fixture did not finish");
}

#[test]
fn protocol_filesystem_requests_cover_snapshots_edits_checkpoints_and_undo() {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate_m5(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Filesystem protocol".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let session_id = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Filesystem session".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };

    let created = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::ApplySessionFilesystemEdit {
            session_id,
            edit: WorkspaceEdit {
                path: "notes/plan.md".to_owned(),
                old_text: String::new(),
                new_text: "first version".to_owned(),
                expected_revision: None,
            },
        },
    )));
    assert!(matches!(
        created.result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::WorkspaceEditApplied(_)
        ))
    ));
    let checkpoint_id = match connection
        .request(RequestEnvelope::new(ClientRequest::Filesystem(
            FilesystemRequest::CreateSessionCheckpoint {
                session_id,
                label: "before update".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Filesystem(FilesystemResponse::CheckpointCreated(checkpoint)) => {
            checkpoint.id
        }
        response => panic!("unexpected checkpoint response: {response:?}"),
    };
    let read = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::ReadSessionFile {
            session_id,
            path: "notes/plan.md".to_owned(),
        },
    )));
    let revision = match read.result.unwrap() {
        ServerResponse::Filesystem(FilesystemResponse::SessionFilesystemFile(file)) => {
            assert_eq!(file.content, "first version");
            file.revision
        }
        response => panic!("unexpected file response: {response:?}"),
    };
    assert!(matches!(
        connection
            .request(RequestEnvelope::new(ClientRequest::Filesystem(
                FilesystemRequest::RevertSessionCheckpoint {
                    session_id,
                    checkpoint_id,
                }
            ),))
            .result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::CheckpointReverted(_)
        ))
    ));
    let updated = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::ApplySessionFilesystemEdit {
            session_id,
            edit: WorkspaceEdit {
                path: "notes/plan.md".to_owned(),
                old_text: "first".to_owned(),
                new_text: "second".to_owned(),
                expected_revision: Some(revision),
            },
        },
    )));
    assert!(matches!(
        updated.result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::WorkspaceEditApplied(_)
        ))
    ));
    let undo = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::UndoSessionEdit { session_id },
    )));
    assert_eq!(undo.result.unwrap_err().code, ErrorCode::InvalidState);
    assert!(matches!(
        connection
            .request(RequestEnvelope::new(ClientRequest::Filesystem(
                FilesystemRequest::GetSessionFilesystemSnapshot { session_id }
            ),))
            .result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::SessionFilesystemSnapshot(_)
        ))
    ));
    assert!(matches!(
        connection
            .request(RequestEnvelope::new(ClientRequest::Filesystem(
                FilesystemRequest::GetSessionFilesystemChanges {
                    session_id,
                    after_sequence: None,
                }
            ),))
            .result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::SessionFilesystemChanges { .. }
        ))
    ));
    assert!(matches!(
        connection
            .request(RequestEnvelope::new(ClientRequest::Filesystem(
                FilesystemRequest::GetSessionContextFiles { session_id }
            )))
            .result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::ContextFiles { .. }
        ))
    ));
    fs::remove_dir_all(&backend.session_root_base).unwrap();
}

#[test]
fn session_scoped_tokens_are_limited_to_their_sessions_and_source_roots() {
    let backend = InProcessBackend::new();
    let unrestricted = backend.connect();
    negotiate(&unrestricted);
    let workspace = match unrestricted
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Scoped access".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let session_id = match unrestricted
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Authorized session".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };

    assert!(matches!(
        unrestricted
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::ListWorkspaces
            )))
            .result,
        Ok(ServerResponse::Workspace(
            WorkspaceResponse::Workspaces { .. }
        ))
    ));
    assert!(matches!(
        unrestricted
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::GetWorkspaceConfigForWorkspace {
                    workspace_id: workspace.id,
                }
            ),))
            .result,
        Ok(ServerResponse::Workspace(
            WorkspaceResponse::WorkspaceConfig(_)
        ))
    ));
    assert!(matches!(
        unrestricted
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::SetWorkspaceConfigForWorkspace {
                    workspace_id: workspace.id,
                    config: WorkspaceConfig::default(),
                }
            ),))
            .result,
        Ok(ServerResponse::Workspace(
            WorkspaceResponse::WorkspaceConfigUpdated
        ))
    ));
    assert!(matches!(
        unrestricted
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::RenameWorkspace {
                    workspace_id: workspace.id,
                    name: "Renamed workspace".to_owned(),
                }
            )))
            .result,
        Ok(ServerResponse::Workspace(
            WorkspaceResponse::WorkspaceRenamed(_)
        ))
    ));
    assert!(matches!(
        unrestricted
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::ListWorkspaceSessions {
                    workspace_id: workspace.id,
                    include_archived: false,
                }
            )))
            .result,
        Ok(ServerResponse::Session(
            SessionResponse::AgentSessions { .. }
        ))
    ));

    let tokens = AuthTokenStore::new();
    let issued = tokens
        .issue(AuthorizationScope::for_sessions(
            [session_id],
            backend.supported_capabilities.clone(),
        ))
        .unwrap();
    let scoped = backend.connect_authenticated(tokens.authenticate(&issued.token).unwrap());
    negotiate(&scoped);
    assert!(matches!(
        scoped
            .request(RequestEnvelope::new(ClientRequest::Session(
                SessionRequest::GetAgentSession { session_id }
            )))
            .result,
        Ok(ServerResponse::Session(SessionResponse::AgentSession(_)))
    ));
    assert_eq!(
        scoped
            .request(RequestEnvelope::new(ClientRequest::Session(
                SessionRequest::GetAgentSession {
                    session_id: AgentSessionId::new(),
                }
            )))
            .result
            .unwrap_err()
            .code,
        ErrorCode::AuthorizationDenied
    );
    assert_eq!(
        scoped
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::CreateWorkspace {
                    name: "Denied workspace".to_owned(),
                }
            )))
            .result
            .unwrap_err()
            .code,
        ErrorCode::AuthorizationDenied
    );
    assert_eq!(
        scoped
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::CreateAgentSessionInWorkspace {
                    workspace_id: workspace.id,
                    name: "Denied session".to_owned(),
                }
            ),))
            .result
            .unwrap_err()
            .code,
        ErrorCode::AuthorizationDenied
    );
    assert_eq!(
        scoped
            .request(RequestEnvelope::new(ClientRequest::Filesystem(
                FilesystemRequest::AttachSessionDirectory {
                    session_id,
                    source: std::env::temp_dir().display().to_string(),
                    path: "sources/local".to_owned(),
                }
            )))
            .result
            .unwrap_err()
            .code,
        ErrorCode::WorkspaceAccessDenied
    );
    assert_eq!(
        scoped
            .request(RequestEnvelope::new(ClientRequest::Events(
                EventsRequest::GetSessionEvents {
                    session_id: None,
                    workspace_id: None,
                    after_sequence: None,
                    stream_epoch: None,
                }
            )))
            .result
            .unwrap_err()
            .code,
        ErrorCode::AuthorizationDenied
    );

    fs::remove_dir_all(&backend.session_root_base).unwrap();
}

#[test]
fn project_snapshot_is_available_to_authorized_tokens_and_scoped_by_membership() {
    let backend = InProcessBackend::new();
    let unrestricted = backend.connect();
    negotiate(&unrestricted);
    let workspace = match unrestricted
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Project snapshots".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let root = match unrestricted
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Project root".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(root)) => root,
        response => panic!("unexpected session response: {response:?}"),
    };
    let project_id = ProjectId::from_uuid(*root.id.as_uuid());
    let snapshot = unrestricted
        .request(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::GetProjectSnapshot { project_id },
        )))
        .result
        .unwrap();
    let ServerResponse::Project(ProjectResponse::ProjectSnapshot(snapshot)) = snapshot else {
        panic!("expected project snapshot, received {snapshot:?}");
    };
    assert_eq!(snapshot.project_id, project_id);
    assert_eq!(snapshot.root_session_id, root.id);
    assert_eq!(snapshot.agents.len(), 1);
    assert_eq!(snapshot.agents[0].depth, 1);
    assert_eq!(snapshot.agents[0].session_id, root.id);

    let unknown_project_id = ProjectId::new();
    assert_eq!(
        unrestricted
            .request(RequestEnvelope::new(ClientRequest::Project(
                ProjectRequest::GetProjectSnapshot {
                    project_id: unknown_project_id,
                }
            )))
            .result
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );

    let tokens = AuthTokenStore::new();
    let issued = tokens
        .issue(AuthorizationScope::for_sessions(
            [],
            backend.supported_capabilities.clone(),
        ))
        .unwrap();
    let scoped = backend.connect_authenticated(tokens.authenticate(&issued.token).unwrap());
    negotiate(&scoped);
    assert_eq!(
        scoped
            .request(RequestEnvelope::new(ClientRequest::Project(
                ProjectRequest::GetProjectSnapshot { project_id }
            )))
            .result
            .unwrap_err()
            .code,
        ErrorCode::AuthorizationDenied
    );
    let wrong_workspace = tokens
        .issue(AuthorizationScope::for_workspaces(
            [WorkspaceId::new()],
            backend.supported_capabilities.clone(),
        ))
        .unwrap();
    let workspace_scoped =
        backend.connect_authenticated(tokens.authenticate(&wrong_workspace.token).unwrap());
    negotiate(&workspace_scoped);
    assert_eq!(
        workspace_scoped
            .request(RequestEnvelope::new(ClientRequest::Project(
                ProjectRequest::GetProjectSnapshot { project_id }
            )))
            .result
            .unwrap_err()
            .code,
        ErrorCode::AuthorizationDenied
    );
    fs::remove_dir_all(&backend.session_root_base).unwrap();
}

#[test]
fn archiving_project_requires_terminal_children_then_archives_the_tree() {
    let temp = std::env::temp_dir().join(format!(
        "loom-project-archive-policy-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&temp).unwrap();
    let path = temp.join("state.sqlite");
    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Archive policy".into(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let root = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Manager".into(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session,
        response => panic!("unexpected session response: {response:?}"),
    };
    let child_id = AgentSessionId::new();
    let created_at = Timestamp::now();
    let child = AgentSessionSnapshot {
        id: child_id,
        workspace_id: workspace.id,
        name: "Child".into(),
        state: AgentSessionState::Idle,
        created_at,
        updated_at: created_at,
    };
    let task = loom_core::DelegatedTaskRecord {
        task_id: loom_core::TaskId::new(),
        project_id: ProjectId::from_uuid(*root.id.as_uuid()),
        requester_session_id: root.id,
        target_session_id: child_id,
        child_name: child.name.clone(),
        intent: "Wait for manager direction".into(),
        model_id: "deterministic/demo".into(),
        context_references: vec![],
        dependencies: vec![],
        code_change: false,
        permissions: loom_core::ProjectAgentPermissions::default(),
        status: loom_core::DelegatedTaskStatus::Queued,
        created_at,
        updated_at: created_at,
    };
    backend
        .persistence
        .as_ref()
        .unwrap()
        .create_project_child(
            RequestId::new(),
            &child,
            backend.sessions().unwrap().next_sequence().next(),
            &task,
        )
        .unwrap();
    let (_, created_event) = backend
        .sessions()
        .unwrap()
        .create_in_workspace_with_id(workspace.id, child_id, child.name.clone())
        .unwrap();
    backend.journal().unwrap().append_session(created_event);

    assert_eq!(
        connection
            .request(RequestEnvelope::new(ClientRequest::Session(
                SessionRequest::ArchiveAgentSession {
                    session_id: root.id,
                }
            )))
            .result
            .unwrap_err()
            .code,
        ErrorCode::InvalidState
    );
    assert_eq!(
        backend.sessions().unwrap().get(root.id).unwrap().state,
        AgentSessionState::Idle
    );

    backend
        .persistence
        .as_ref()
        .unwrap()
        .update_delegated_task_status(
            task.task_id,
            loom_core::DelegatedTaskStatus::Cancelled,
            Timestamp::now(),
        )
        .unwrap();
    assert!(matches!(
        connection
            .request(RequestEnvelope::new(ClientRequest::Session(SessionRequest::ArchiveAgentSession{
                session_id: root.id,
            })))
            .result,
        Ok(ServerResponse::Session(SessionResponse::AgentSessionArchived(session)))
            if session.state == AgentSessionState::Archived
    ));
    assert_eq!(
        backend.sessions().unwrap().get(child_id).unwrap().state,
        AgentSessionState::Archived
    );

    drop(connection);
    drop(backend);
    let _ = fs::remove_dir_all(temp);
    let _ = fs::remove_dir_all(path.with_extension("session-roots"));
    let _ = fs::remove_file(path.with_extension("credentials.json"));
}

#[test]
fn delegated_children_and_direct_messages_are_durable_and_bounded() {
    let temp = std::env::temp_dir().join(format!("loom-project-agents-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&temp).unwrap();
    let backend = InProcessBackend::new_persistent(temp.join("state.sqlite")).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Project agents".into(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let root = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Manager".into(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };

    let create_spec = loom_core::DelegatedTaskSpec {
        intent: "Review a bounded task".into(),
        model_id: "deterministic/demo".into(),
        context_references: vec![],
        dependencies: vec![],
        code_change: false,
        permissions: loom_core::ProjectAgentPermissions::default(),
    };
    let request_id = RequestId::new();
    let response =
        connection.create_project_child(request_id, root, "worker-1".into(), create_spec.clone());
    let (task, child) = match response.unwrap() {
        ServerResponse::Project(ProjectResponse::ProjectChildCreated { task, child }) => {
            (task, child)
        }
        response => panic!("unexpected child response: {response:?}"),
    };
    assert_eq!(task.requester_session_id, root);
    assert_eq!(task.target_session_id, child.session_id);
    assert_eq!(child.depth, 2);
    let child_project_snapshot = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::GetProjectSnapshotForSession {
            session_id: child.session_id,
        },
    )));
    assert!(matches!(
        child_project_snapshot.result,
        Ok(ServerResponse::Project(ProjectResponse::ProjectSnapshot(snapshot)))
            if snapshot.project_id == task.project_id
                && snapshot.root_session_id == root
                && snapshot.agents.len() == 2
    ));
    assert!(matches!(
        connection.create_project_child(
            request_id,
            root,
            "worker-1".into(),
            create_spec.clone(),
        ),
        Ok(ServerResponse::Project(ProjectResponse::ProjectChildCreated{ task: repeated, child: repeated_child }))
            if repeated.task_id == task.task_id && repeated_child.session_id == child.session_id
    ));
    let snapshot = connection
        .request(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::GetProjectSnapshot {
                project_id: ProjectId::from_uuid(*root.as_uuid()),
            },
        )))
        .result
        .unwrap();
    assert!(
        matches!(snapshot, ServerResponse::Project(ProjectResponse::ProjectSnapshot(snapshot)) if snapshot.agents.len() == 2)
    );
    let ServerResponse::Project(ProjectResponse::ProjectSnapshot(project_snapshot)) = connection
        .request(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::GetProjectSnapshot {
                project_id: ProjectId::from_uuid(*root.as_uuid()),
            },
        )))
        .result
        .unwrap()
    else {
        panic!("expected project snapshot")
    };
    assert!(
        project_snapshot
            .tasks
            .iter()
            .any(|candidate| candidate.task_id == task.task_id)
    );

    let message = loom_core::AgentMessageDraft {
        project_id: ProjectId::from_uuid(*root.as_uuid()),
        task_id: Some(task.task_id),
        sender_session_id: root,
        target_session_id: child.session_id,
        kind: loom_core::AgentMessageKind::Direction,
        body: "Please report findings.".into(),
    };
    for untrusted_message in [
        message.clone(),
        loom_core::AgentMessageDraft {
            sender_session_id: child.session_id,
            ..message.clone()
        },
    ] {
        assert_eq!(
            connection
                .request(RequestEnvelope::new(ClientRequest::Project(
                    ProjectRequest::SendProjectAgentMessage {
                        message: untrusted_message,
                    }
                )))
                .result
                .unwrap_err()
                .code,
            ErrorCode::AuthorizationDenied
        );
    }
    let root_events = connection.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetSessionEvents {
            session_id: Some(root),
            workspace_id: None,
            after_sequence: Some(EventSequence::default()),
            stream_epoch: None,
        },
    )));
    assert!(matches!(
        root_events.result,
        Ok(ServerResponse::Events(EventsResponse::SessionEvents{ events, .. }))
            if events.iter().all(|event| !matches!(
                &event.event,
                ServerEvent::ProjectAgentMessageAccepted { message: accepted }
                    if accepted.project_id == message.project_id
            ))
    ));
    let messages = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::ListProjectAgentMessages {
            project_id: message.project_id,
            session_id: child.session_id,
            after_project_sequence: None,
            limit: 10,
        },
    )));
    assert!(matches!(
        messages.result,
        Ok(ServerResponse::Project(ProjectResponse::ProjectAgentMessages{ messages, .. })) if messages.is_empty()
    ));
    let tokens = AuthTokenStore::new();
    let scoped_token = tokens
        .issue(AuthorizationScope::for_sessions(
            [root],
            backend.supported_capabilities.clone(),
        ))
        .unwrap();
    let scoped = backend.connect_authenticated(tokens.authenticate(&scoped_token.token).unwrap());
    negotiate(&scoped);
    assert_eq!(
        scoped
            .request(RequestEnvelope::new(ClientRequest::Project(
                ProjectRequest::SendProjectAgentMessage {
                    message: message.clone(),
                }
            )))
            .result
            .unwrap_err()
            .code,
        ErrorCode::AuthorizationDenied
    );
    let broad_token = tokens.issue(AuthorizationScope::all()).unwrap();
    let broad = backend.connect_authenticated(tokens.authenticate(&broad_token.token).unwrap());
    negotiate(&broad);
    assert_eq!(
        broad
            .request(RequestEnvelope::new(ClientRequest::Project(
                ProjectRequest::SendProjectAgentMessage {
                    message: message.clone(),
                }
            )))
            .result
            .unwrap_err()
            .code,
        ErrorCode::AuthorizationDenied
    );
    assert_eq!(
        scoped
            .request(RequestEnvelope::new(ClientRequest::Project(
                ProjectRequest::ListProjectAgentMessages {
                    project_id: message.project_id,
                    session_id: child.session_id,
                    after_project_sequence: None,
                    limit: 10,
                }
            )))
            .result
            .unwrap_err()
            .code,
        ErrorCode::AuthorizationDenied
    );
    let wrong_direction = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::SendProjectAgentMessage {
            message: loom_core::AgentMessageDraft {
                target_session_id: AgentSessionId::new(),
                ..message.clone()
            },
        },
    )));
    assert!(matches!(
        wrong_direction.result,
        Err(error) if error.code == ErrorCode::AuthorizationDenied
    ));
    let code_change = connection.create_project_child(
        RequestId::new(),
        root,
        "coder".into(),
        loom_core::DelegatedTaskSpec {
            intent: "Change code".into(),
            model_id: "deterministic/demo".into(),
            context_references: vec![],
            dependencies: vec![],
            code_change: true,
            permissions: loom_core::ProjectAgentPermissions::default(),
        },
    );
    assert!(matches!(
        code_change,
        Err(ref error)
            if error.code == ErrorCode::InvalidRequest
                && error.message.contains("exactly one Git repository")
    ));

    for index in 1..4 {
        let result = connection.create_project_child(
            RequestId::new(),
            root,
            format!("worker-{index}"),
            loom_core::DelegatedTaskSpec {
                intent: format!("Task {index}"),
                model_id: "deterministic/demo".into(),
                context_references: vec![],
                dependencies: vec![],
                code_change: false,
                permissions: loom_core::ProjectAgentPermissions::default(),
            },
        );
        assert!(matches!(
            result,
            Ok(ServerResponse::Project(
                ProjectResponse::ProjectChildCreated { .. }
            ))
        ));
    }
    let additional_child = connection.create_project_child(
        RequestId::new(),
        root,
        "worker-5".into(),
        loom_core::DelegatedTaskSpec {
            intent: "One too many".into(),
            model_id: "deterministic/demo".into(),
            context_references: vec![],
            dependencies: vec![],
            code_change: false,
            permissions: loom_core::ProjectAgentPermissions::default(),
        },
    );
    assert!(
        matches!(
            additional_child,
            Ok(ServerResponse::Project(
                ProjectResponse::ProjectChildCreated { .. }
            ))
        ),
        "unexpected additional child response: {additional_child:?}"
    );
    assert!(matches!(
        connection.create_project_child(
            request_id,
            root,
            "different-child".into(),
            loom_core::DelegatedTaskSpec {
                intent: "Changed request under reused ID".into(),
                model_id: "deterministic/demo".into(),
                context_references: vec![],
                dependencies: vec![],
                code_change: false,
                permissions: loom_core::ProjectAgentPermissions::default(),
            },
        ),
        Err(error) if error.code == ErrorCode::InvalidRequest
    ));
    drop(connection);
    drop(backend);
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn project_child_worktree_can_be_reviewed_fast_forwarded_and_cleaned_up() {
    let temp = workspace();
    let source = git_repository();
    let database_path = temp.join("state.sqlite");
    let backend = InProcessBackend::new_persistent(&database_path).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace_record = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Project worktree".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let root = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace_record.id,
                name: "Manager".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session,
        response => panic!("unexpected session response: {response:?}"),
    };
    let parent_repository = match connection
        .request(RequestEnvelope::new(ClientRequest::Repository(
            RepositoryRequest::AttachSessionRepository {
                session_id: root.id,
                source: source.display().to_string(),
                path: "repo".to_owned(),
                revision: None,
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Repository(RepositoryResponse::SessionRepositoryAttached(repository)) => {
            repository
        }
        response => panic!("unexpected repository response: {response:?}"),
    };
    let parent_git = connection
        .session_git(root.id, parent_repository.id)
        .unwrap();
    let base_revision = parent_git.status().unwrap().head.unwrap();

    let task_id = loom_core::TaskId::new();
    let child_session_id = AgentSessionId::new();
    let created_at = Timestamp::now();
    let child_snapshot = AgentSessionSnapshot {
        id: child_session_id,
        workspace_id: workspace_record.id,
        name: "Code child".to_owned(),
        state: AgentSessionState::Idle,
        created_at,
        updated_at: created_at,
    };
    let task = loom_core::DelegatedTaskRecord {
        task_id,
        project_id: ProjectId::from_uuid(*root.id.as_uuid()),
        requester_session_id: root.id,
        target_session_id: child_session_id,
        child_name: child_snapshot.name.clone(),
        intent: "Make a committed code change".to_owned(),
        model_id: "deterministic/demo".to_owned(),
        context_references: Vec::new(),
        dependencies: Vec::new(),
        code_change: true,
        permissions: loom_core::ProjectAgentPermissions::default(),
        status: loom_core::DelegatedTaskStatus::Queued,
        created_at,
        updated_at: created_at,
    };
    let mut worktree = ProjectWorktreeRecord {
        project_id: task.project_id,
        task_id,
        parent_session_id: root.id,
        child_session_id,
        parent_repository_id: parent_repository.id,
        child_repository_id: RepositoryId::new(),
        relative_path: format!("project-worktrees/{task_id}"),
        worktree_name: format!("loom-child-{task_id}"),
        branch_name: format!("loom/project-child-{task_id}"),
        base_revision,
        result_revision: None,
        integrated_revision: None,
        status: ProjectWorktreeStatus::Creating,
        conflict_paths: Vec::new(),
        error: None,
        cleanup_disposition: None,
        created_at,
        updated_at: created_at,
    };
    backend
        .persistence
        .as_ref()
        .unwrap()
        .create_project_child_with_worktree(
            RequestId::new(),
            &child_snapshot,
            backend.sessions().unwrap().next_sequence().next(),
            &task,
            &worktree,
        )
        .unwrap();
    let (_, event) = backend
        .sessions()
        .unwrap()
        .create_in_workspace_with_id(
            workspace_record.id,
            child_session_id,
            child_snapshot.name.clone(),
        )
        .unwrap();
    backend.journal().unwrap().append_session(event);
    connection
        .ensure_project_worktree_ready(&mut worktree)
        .unwrap();

    let child_checkout = connection
        .session_filesystem(child_session_id)
        .unwrap()
        .root()
        .join(&worktree.relative_path);
    fs::write(child_checkout.join("README.md"), "child result\n").unwrap();
    let git = |arguments: &[&str]| {
        let output = Command::new("git")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .args(["-C", child_checkout.to_str().unwrap()])
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            arguments,
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["config", "user.name", "Loom Test"]);
    git(&["config", "user.email", "loom@example.test"]);
    git(&["add", "--", "README.md"]);
    git(&["commit", "-qm", "child result"]);
    backend
        .persistence
        .as_ref()
        .unwrap()
        .update_delegated_task_status(
            task_id,
            loom_core::DelegatedTaskStatus::Completed,
            Timestamp::now(),
        )
        .unwrap();

    let integration_before_review = connection.request(RequestEnvelope::new(
        ClientRequest::Project(ProjectRequest::IntegrateProjectChild {
            project_id: task.project_id,
            manager_session_id: root.id,
            task_id,
            expected_parent_revision: worktree.base_revision.clone(),
        }),
    ));
    assert!(matches!(
        integration_before_review.result,
        Err(error) if error.code == ErrorCode::InvalidState
    ));

    let review = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::GetProjectChildReview {
            project_id: task.project_id,
            manager_session_id: root.id,
            task_id,
        },
    )));
    let ServerResponse::Project(ProjectResponse::ProjectChildReview {
        worktree: reviewed,
        status: reviewed_status,
        diff,
    }) = review.result.unwrap()
    else {
        panic!("unexpected child review response");
    };
    assert_eq!(reviewed.status, ProjectWorktreeStatus::Ready);
    assert_eq!(
        reviewed_status.branch.as_deref(),
        Some(worktree.branch_name.as_str())
    );
    assert!(diff.patch.contains("child result"));

    let completion_guard = ProjectAgentTools {
        backend: Arc::downgrade(&backend),
        session_id: root.id,
        project_id: task.project_id,
        model_id: ModelId::new("deterministic/demo"),
        can_delegate: false,
        can_delegate_code: false,
        can_message: false,
        can_branch_message: false,
        can_inspect_children: false,
        can_wait_children: false,
        can_control_children: false,
        can_review_children: false,
        can_integrate_children: false,
    };
    let unreviewed_path = child_checkout.join("unreviewed.txt");
    fs::write(&unreviewed_path, "unreviewed change\n").unwrap();
    assert!(
        completion_guard
            .completion_blocker()
            .is_some_and(|blocker| blocker.contains("changed after review"))
    );
    fs::remove_file(unreviewed_path).unwrap();

    let integration = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::IntegrateProjectChild {
            project_id: task.project_id,
            manager_session_id: root.id,
            task_id,
            expected_parent_revision: worktree.base_revision.clone(),
        },
    )));
    let ServerResponse::Project(ProjectResponse::ProjectChildWorktreeUpdated(integrated)) =
        integration.result.unwrap()
    else {
        panic!("unexpected child integration response");
    };
    assert_eq!(integrated.status, ProjectWorktreeStatus::Integrated);
    assert_eq!(
        parent_git.status().unwrap().head,
        integrated.integrated_revision
    );

    let cleanup = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::CleanupProjectChildWorktree {
            project_id: task.project_id,
            manager_session_id: root.id,
            task_id,
            disposition: ProjectWorktreeCleanupDisposition::RemoveClean,
        },
    )));
    let ServerResponse::Project(ProjectResponse::ProjectChildWorktreeUpdated(removed)) =
        cleanup.result.unwrap()
    else {
        panic!("unexpected child cleanup response");
    };
    assert_eq!(removed.status, ProjectWorktreeStatus::Removed);
    assert!(!child_checkout.exists());
    assert_eq!(
        fs::read_to_string(parent_git.root().join("README.md")).unwrap(),
        "child result\n"
    );

    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    fs::remove_dir_all(temp).unwrap();
    fs::remove_dir_all(source).unwrap();
}

#[test]
fn nested_code_child_worktree_integrates_through_parent_to_root() {
    let temp = workspace();
    let source = git_repository();
    let persistence_path = temp.join("state.sqlite");
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_endpoint = model.endpoint.clone();
    let model_id = ModelId::new("fixture/nested-worktree-integration");
    let backend = InProcessBackend::with_provider_registry_persistent(
        scripted_project_provider_registry(&model_endpoint, model_id.clone()),
        &persistence_path,
    )
    .unwrap();

    let connection = backend.connect();
    negotiate(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Nested project worktree integration".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let root = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Root manager".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };
    assert!(matches!(
        connection
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::SetWorkspaceConfigForWorkspace {
                    workspace_id: workspace.id,
                    config: WorkspaceConfig {
                        project_agent_concurrency: 1,
                        ..WorkspaceConfig::default()
                    },
                }
            ),))
            .result,
        Ok(ServerResponse::Workspace(
            WorkspaceResponse::WorkspaceConfigUpdated
        ))
    ));
    let root_repository = match connection
        .request(RequestEnvelope::new(ClientRequest::Repository(
            RepositoryRequest::AttachSessionRepository {
                session_id: root,
                source: source.display().to_string(),
                path: "repo".to_owned(),
                revision: None,
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Repository(RepositoryResponse::SessionRepositoryAttached(repository)) => {
            repository
        }
        response => panic!("unexpected repository response: {response:?}"),
    };
    let root_git = connection.session_git(root, root_repository.id).unwrap();
    let root_base_revision = root_git.status().unwrap().head.unwrap();

    let manager_permissions = loom_core::ProjectAgentPermissions {
        delegation: true,
        worktree_creation: true,
        review: true,
        integration: true,
        ..loom_core::ProjectAgentPermissions::default()
    };
    let (manager_task, manager_agent) = match connection
        .create_project_child(
            RequestId::new(),
            root,
            "code-manager".to_owned(),
            loom_core::DelegatedTaskSpec {
                intent: "Integrate a reviewed nested code change.".to_owned(),
                model_id: model_id.as_str().to_owned(),
                context_references: vec![],
                dependencies: vec![],
                code_change: true,
                permissions: manager_permissions,
            },
        )
        .unwrap()
    {
        ServerResponse::Project(ProjectResponse::ProjectChildCreated { task, child }) => {
            (task, child)
        }
        response => panic!("unexpected manager creation response: {response:?}"),
    };
    assert_eq!(manager_task.status, loom_core::DelegatedTaskStatus::Running);
    assert!(manager_task.code_change);
    assert_eq!(manager_task.permissions, manager_permissions);
    let manager_session = manager_agent.session_id;
    let project_id = manager_task.project_id;
    let persistence = backend.persistence.as_ref().unwrap();
    let manager_worktree = persistence
        .load_project_worktree_by_task(manager_task.task_id)
        .unwrap()
        .expect("code manager should have a durable worktree");
    assert_eq!(manager_worktree.status, ProjectWorktreeStatus::Ready);
    assert_eq!(manager_worktree.parent_session_id, root);
    assert_eq!(manager_worktree.parent_repository_id, root_repository.id);
    assert_eq!(manager_worktree.base_revision, root_base_revision);
    let manager_git = connection
        .session_git(manager_session, manager_worktree.child_repository_id)
        .unwrap();
    assert_eq!(
        manager_git.status().unwrap().head.as_deref(),
        Some(root_base_revision.as_str())
    );

    // Keep the manager's only workspace slot occupied while the nested
    // child is created, reviewed, and integrated into its checkout.
    let manager_turn = model.next_for_child();
    assert!(request_has_tool(
        &manager_turn.request,
        "delegate_project_task"
    ));
    assert!(request_has_tool(
        &manager_turn.request,
        "review_project_child"
    ));
    assert!(request_has_tool(
        &manager_turn.request,
        "integrate_project_child"
    ));

    let (grandchild_task, grandchild_agent) = match connection
        .create_project_child(
            RequestId::new(),
            manager_session,
            "nested-code-child".to_owned(),
            loom_core::DelegatedTaskSpec {
                intent: "Commit the nested result for integration.".to_owned(),
                model_id: model_id.as_str().to_owned(),
                context_references: vec![],
                dependencies: vec![],
                code_change: true,
                permissions: loom_core::ProjectAgentPermissions::default(),
            },
        )
        .unwrap()
    {
        ServerResponse::Project(ProjectResponse::ProjectChildCreated { task, child }) => {
            (task, child)
        }
        response => panic!("unexpected nested child creation response: {response:?}"),
    };
    assert_eq!(
        grandchild_task.status,
        loom_core::DelegatedTaskStatus::Queued
    );
    assert_eq!(grandchild_task.requester_session_id, manager_session);
    let grandchild_worktree = persistence
        .load_project_worktree_by_task(grandchild_task.task_id)
        .unwrap()
        .expect("nested code child should have a durable worktree");
    assert_eq!(grandchild_worktree.status, ProjectWorktreeStatus::Ready);
    assert_eq!(grandchild_worktree.parent_session_id, manager_session);
    assert_eq!(
        grandchild_worktree.parent_repository_id,
        manager_worktree.child_repository_id
    );
    assert_eq!(
        grandchild_worktree.base_revision, root_base_revision,
        "nested worktree should branch from the manager checkout HEAD"
    );
    let grandchild_checkout = connection
        .session_filesystem(grandchild_agent.session_id)
        .unwrap()
        .root()
        .join(&grandchild_worktree.relative_path);
    fs::write(
        grandchild_checkout.join("nested-result.txt"),
        "grandchild result\n",
    )
    .unwrap();
    let git = |arguments: &[&str]| {
        let output = Command::new("git")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .args(["-C", grandchild_checkout.to_str().unwrap()])
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            arguments,
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["config", "user.name", "Loom Test"]);
    git(&["config", "user.email", "loom@example.test"]);
    git(&["add", "--", "nested-result.txt"]);
    git(&["commit", "-qm", "nested result"]);
    let grandchild_git = connection
        .session_git(
            grandchild_agent.session_id,
            grandchild_worktree.child_repository_id,
        )
        .unwrap();
    let grandchild_revision = grandchild_git.status().unwrap().head.unwrap();
    assert_ne!(grandchild_revision, grandchild_worktree.base_revision);
    persistence
        .update_delegated_task_status(
            grandchild_task.task_id,
            loom_core::DelegatedTaskStatus::Completed,
            Timestamp::now(),
        )
        .unwrap();

    let review = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::GetProjectChildReview {
            project_id,
            manager_session_id: manager_session,
            task_id: grandchild_task.task_id,
        },
    )));
    let ServerResponse::Project(ProjectResponse::ProjectChildReview {
        worktree: reviewed_grandchild,
        status: grandchild_status,
        diff,
    }) = review.result.unwrap()
    else {
        panic!("unexpected nested child review response");
    };
    assert_eq!(reviewed_grandchild.status, ProjectWorktreeStatus::Ready);
    assert_eq!(
        grandchild_status.head.as_deref(),
        Some(grandchild_revision.as_str())
    );
    assert!(diff.patch.contains("grandchild result"));
    assert_eq!(
        root_git.status().unwrap().head.as_deref(),
        Some(root_base_revision.as_str()),
        "reviewing the grandchild must not advance the root checkout"
    );
    assert!(
        !root_git.root().join("nested-result.txt").exists(),
        "the nested change should not reach root before either integration"
    );

    let integration = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::IntegrateProjectChild {
            project_id,
            manager_session_id: manager_session,
            task_id: grandchild_task.task_id,
            expected_parent_revision: grandchild_worktree.base_revision.clone(),
        },
    )));
    let ServerResponse::Project(ProjectResponse::ProjectChildWorktreeUpdated(
        integrated_grandchild,
    )) = integration.result.unwrap()
    else {
        panic!("unexpected nested child integration response");
    };
    assert_eq!(
        integrated_grandchild.status,
        ProjectWorktreeStatus::Integrated
    );
    assert_eq!(
        integrated_grandchild.integrated_revision.as_deref(),
        Some(grandchild_revision.as_str())
    );
    assert_eq!(
        manager_git.status().unwrap().head.as_deref(),
        Some(grandchild_revision.as_str()),
        "integrating the grandchild should advance the manager checkout"
    );
    let manager_checkout = connection
        .session_filesystem(manager_session)
        .unwrap()
        .root()
        .join(&manager_worktree.relative_path);
    assert_eq!(
        fs::read_to_string(manager_checkout.join("nested-result.txt")).unwrap(),
        "grandchild result\n"
    );
    assert_eq!(
        root_git.status().unwrap().head.as_deref(),
        Some(root_base_revision.as_str()),
        "the grandchild integration should stop at its manager parent"
    );

    model.respond_with_text(
        manager_turn,
        "The nested result is integrated and the delegated work is complete.",
    );
    let manager_run_id = persistence
        .load_latest_run_summary_for_session(manager_session)
        .unwrap()
        .expect("manager run should have a durable checkpoint")
        .snapshot
        .id;
    assert_eq!(
        await_settled_run(&connection, manager_run_id).state,
        AgentRunState::Completed
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = persistence
            .load_delegated_task(manager_task.task_id)
            .unwrap()
            .unwrap()
            .status;
        if status == loom_core::DelegatedTaskStatus::Completed {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "manager should complete after its child is integrated; status={status:?}"
        );
        thread::sleep(Duration::from_millis(1));
    }

    let root_review = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::GetProjectChildReview {
            project_id,
            manager_session_id: root,
            task_id: manager_task.task_id,
        },
    )));
    let ServerResponse::Project(ProjectResponse::ProjectChildReview {
        worktree: reviewed_manager,
        status: manager_status,
        diff: manager_diff,
    }) = root_review.result.unwrap()
    else {
        panic!("unexpected manager review response");
    };
    assert_eq!(reviewed_manager.status, ProjectWorktreeStatus::Ready);
    assert_eq!(
        manager_status.head.as_deref(),
        Some(grandchild_revision.as_str())
    );
    assert!(manager_diff.patch.contains("grandchild result"));
    assert_eq!(
        reviewed_manager.base_revision, root_base_revision,
        "manager worktree should retain its original root-relative base"
    );

    let root_integration = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::IntegrateProjectChild {
            project_id,
            manager_session_id: root,
            task_id: manager_task.task_id,
            expected_parent_revision: manager_worktree.base_revision.clone(),
        },
    )));
    let ServerResponse::Project(ProjectResponse::ProjectChildWorktreeUpdated(integrated_manager)) =
        root_integration.result.unwrap()
    else {
        panic!("unexpected manager integration response");
    };
    assert_eq!(integrated_manager.status, ProjectWorktreeStatus::Integrated);
    assert_eq!(
        integrated_manager.integrated_revision.as_deref(),
        Some(grandchild_revision.as_str())
    );
    assert_eq!(
        root_git.status().unwrap().head.as_deref(),
        Some(grandchild_revision.as_str()),
        "integrating the manager should advance root to the nested result"
    );
    assert_eq!(
        fs::read_to_string(root_git.root().join("nested-result.txt")).unwrap(),
        "grandchild result\n"
    );

    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    drop(model);
    fs::remove_dir_all(temp).unwrap();
    fs::remove_dir_all(source).unwrap();
}

#[test]
fn project_concurrency_limit_queues_additional_children() {
    let (endpoint, request_seen) = slow_model_endpoint();
    let path = std::env::temp_dir().join(format!(
        "loom-project-concurrency-{}.db",
        uuid::Uuid::new_v4()
    ));
    let backend = InProcessBackend::with_openai_compatible_persistent(
        endpoint,
        "test-key",
        ModelId::new("slow/model"),
        &path,
    )
    .unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Project concurrency".into(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let root = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Manager".into(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };
    assert!(matches!(
        connection
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::SetWorkspaceConfigForWorkspace {
                    workspace_id: workspace.id,
                    config: WorkspaceConfig {
                        project_agent_concurrency: 1,
                        ..WorkspaceConfig::default()
                    },
                }
            ),))
            .result,
        Ok(ServerResponse::Workspace(
            WorkspaceResponse::WorkspaceConfigUpdated
        ))
    ));

    let create_child = |child_name: &str| {
        connection.create_project_child(
            RequestId::new(),
            root,
            child_name.to_owned(),
            loom_core::DelegatedTaskSpec {
                intent: format!("Review {child_name}"),
                model_id: "slow/model".into(),
                context_references: vec![],
                dependencies: vec![],
                code_change: false,
                permissions: loom_core::ProjectAgentPermissions::default(),
            },
        )
    };

    let first = match create_child("worker-1").unwrap() {
        ServerResponse::Project(ProjectResponse::ProjectChildCreated { task, child }) => {
            (task, child)
        }
        response => panic!("unexpected first child response: {response:?}"),
    };
    request_seen
        .recv_timeout(Duration::from_secs(3))
        .expect("first child should begin its provider request");
    let second = match create_child("worker-2").unwrap() {
        ServerResponse::Project(ProjectResponse::ProjectChildCreated { task, child }) => {
            (task, child)
        }
        response => panic!("unexpected second child response: {response:?}"),
    };
    assert_eq!(first.0.status, loom_core::DelegatedTaskStatus::Running);
    assert_eq!(second.0.status, loom_core::DelegatedTaskStatus::Queued);
    assert_ne!(first.1.session_id, second.1.session_id);
    assert!(matches!(
        connection
            .request(RequestEnvelope::new(
                ClientRequest::Run(RunRequest::StartSessionAgentRun{
                    session_id: second.1.session_id,
                    task: "Bypass the project queue".into(),
                    model: ModelId::new("slow/model"),
                    system_instructions: None,
                    repository_instructions: None,
                }),
            ))
            .result,
        Err(error) if error.code == ErrorCode::Conflict
    ));

    backend.shutdown().unwrap();
    drop(connection);
    drop(backend);
    let session_roots = path.with_extension("session-roots");
    let _ = fs::remove_dir_all(session_roots);
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(path.with_extension("credentials.json"));
}

#[test]
fn project_manager_tool_creates_an_idempotent_non_code_child() {
    let temp = std::env::temp_dir().join(format!(
        "loom-project-delegation-tool-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&temp).unwrap();
    let backend = InProcessBackend::new_persistent(temp.join("state.sqlite")).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Project tool test".into(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let root = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Manager".into(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };
    assert!(
        connection
            .project_delegation_enabled_for_session(root)
            .unwrap()
    );
    let tokens = AuthTokenStore::new();
    let read_only = tokens
        .issue(AuthorizationScope::for_sessions(
            [root],
            CapabilitySet::new([Capability::StartAgentRun, Capability::ReadAgentSession]),
        ))
        .unwrap();
    let read_only_connection =
        backend.connect_authenticated(tokens.authenticate(&read_only.token).unwrap());
    assert!(
        !read_only_connection
            .project_delegation_enabled_for_session(root)
            .unwrap()
    );
    let delegation_grant = tokens
        .issue(AuthorizationScope::for_sessions(
            [root],
            CapabilitySet::new([Capability::StartAgentRun, Capability::CreateProjectChild]),
        ))
        .unwrap();
    let delegation_connection =
        backend.connect_authenticated(tokens.authenticate(&delegation_grant.token).unwrap());
    assert!(
        delegation_connection
            .project_delegation_enabled_for_session(root)
            .unwrap()
    );
    assert!(
        !delegation_connection
            .project_capability_enabled_for_session(root, Capability::SendProjectAgentMessage)
            .unwrap()
    );
    let coordination_grant = tokens
        .issue(AuthorizationScope::for_sessions(
            [root],
            CapabilitySet::new([
                Capability::StartAgentRun,
                Capability::SendProjectAgentMessage,
                Capability::ReadProject,
            ]),
        ))
        .unwrap();
    let coordination_connection =
        backend.connect_authenticated(tokens.authenticate(&coordination_grant.token).unwrap());
    assert!(
        coordination_connection
            .project_capability_enabled_for_session(root, Capability::SendProjectAgentMessage)
            .unwrap()
    );
    assert!(
        coordination_connection
            .project_capability_enabled_for_session(root, Capability::ReadProject)
            .unwrap()
    );
    let control_grant = tokens
        .issue(AuthorizationScope::for_sessions(
            [root],
            CapabilitySet::new([Capability::StartAgentRun, Capability::ControlProjectChild]),
        ))
        .unwrap();
    let control_connection =
        backend.connect_authenticated(tokens.authenticate(&control_grant.token).unwrap());
    assert!(
        control_connection
            .project_capability_enabled_for_session(root, Capability::ControlProjectChild)
            .unwrap()
    );
    assert!(
        !delegation_connection
            .project_capability_enabled_for_session(root, Capability::ControlProjectChild)
            .unwrap()
    );
    let extension = backend
        .project_agent_tools(
            root,
            ModelId::new("deterministic/demo"),
            ProjectAgentToolGrants {
                delegation: true,
                messaging: true,
                branch_messaging: true,
                inspection: true,
                child_control: true,
                worktree: false,
                review: false,
                integration: false,
            },
        )
        .unwrap()
        .expect("root project tools");
    let tools = ToolExecutor::new_with_workspace(connection.session_filesystem(root).unwrap())
        .with_extension(extension);
    let call = ToolCall {
        id: ToolCallId::new(),
        name: "delegate_project_task".to_owned(),
        arguments: serde_json::json!({
            "child_name": "protocol reviewer",
            "intent": "Review the protocol compatibility design",
            "context_references": [{"label": "Design", "uri": "docs/project-sessions-design.md"}],
            "dependencies": [],
            "permissions": {"branch_messaging": true}
        }),
    };

    assert!(
        tools
            .definitions()
            .iter()
            .any(|definition| definition.name == "delegate_project_task")
    );
    assert!(
        tools
            .definitions()
            .iter()
            .any(|definition| definition.name == "send_project_agent_message")
    );
    assert!(
        tools
            .definitions()
            .iter()
            .any(|definition| definition.name == "list_project_children")
    );
    assert!(
        tools
            .definitions()
            .iter()
            .any(|definition| definition.name == "control_project_child")
    );
    assert_eq!(tools.action_kind(&call), Some(loom_core::ActionKind::Write));
    let first = tools.execute(&call);
    assert!(first.success, "{}", first.output);
    let first_output: serde_json::Value = serde_json::from_str(&first.output).unwrap();
    let task_id = first_output["task_id"].as_str().unwrap();
    let child_session_id = first_output["child_session_id"]
        .as_str()
        .unwrap()
        .parse::<AgentSessionId>()
        .unwrap();
    let repeated = tools.execute(&call);
    assert!(repeated.success, "{}", repeated.output);
    let repeated_output: serde_json::Value = serde_json::from_str(&repeated.output).unwrap();
    assert_eq!(repeated_output["task_id"].as_str(), Some(task_id));

    let snapshot = backend
        .persistence
        .as_ref()
        .unwrap()
        .load_project_snapshot(ProjectId::from_uuid(*root.as_uuid()))
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.agents.len(), 2);
    assert!(
        snapshot
            .tasks
            .iter()
            .any(|task| task.task_id.to_string() == task_id)
    );
    let task_id = task_id.parse::<loom_core::TaskId>().unwrap();
    let task_id_string = task_id.to_string();
    let task = backend
        .persistence
        .as_ref()
        .unwrap()
        .load_delegated_task(task_id)
        .unwrap()
        .unwrap();
    assert_eq!(task.model_id, "deterministic/demo");
    assert!(!task.code_change);

    let ungranted_peer = tools.execute(&ToolCall {
        id: ToolCallId::new(),
        name: "delegate_project_task".to_owned(),
        arguments: serde_json::json!({
            "child_name": "ungranted peer",
            "intent": "Remain available for a bounded follow-up",
        }),
    });
    assert!(ungranted_peer.success, "{}", ungranted_peer.output);
    let ungranted_peer: serde_json::Value = serde_json::from_str(&ungranted_peer.output).unwrap();
    let ungranted_peer_id = ungranted_peer["child_session_id"]
        .as_str()
        .unwrap()
        .parse::<AgentSessionId>()
        .unwrap();

    // Keep a second child idle so the manager-to-child tool path does not
    // race the deterministic child runner finishing its first task.
    let idle_child_id = AgentSessionId::new();
    let created_at = Timestamp::now();
    let idle_child = AgentSessionSnapshot {
        id: idle_child_id,
        workspace_id: workspace.id,
        name: "idle reviewer".to_owned(),
        state: AgentSessionState::Idle,
        created_at,
        updated_at: created_at,
    };
    let idle_task = loom_core::DelegatedTaskRecord {
        task_id: loom_core::TaskId::new(),
        project_id: ProjectId::from_uuid(*root.as_uuid()),
        requester_session_id: root,
        target_session_id: idle_child_id,
        child_name: idle_child.name.clone(),
        intent: "Wait for manager direction".to_owned(),
        model_id: "deterministic/demo".to_owned(),
        context_references: vec![],
        dependencies: vec![],
        code_change: false,
        permissions: loom_core::ProjectAgentPermissions {
            branch_messaging: true,
            ..loom_core::ProjectAgentPermissions::default()
        },
        status: loom_core::DelegatedTaskStatus::Queued,
        created_at,
        updated_at: created_at,
    };
    let persistence = backend.persistence.as_ref().unwrap();
    persistence
        .create_project_child(
            RequestId::new(),
            &idle_child,
            backend.sessions().unwrap().next_sequence().next(),
            &idle_task,
        )
        .unwrap();
    let (_, created_event) = backend
        .sessions()
        .unwrap()
        .create_in_workspace_with_id(workspace.id, idle_child_id, idle_child.name.clone())
        .unwrap();
    backend.journal().unwrap().append_session(created_event);
    let filesystem = backend
        .create_session_filesystem(workspace.id, idle_child_id)
        .unwrap();
    backend
        .session_filesystems()
        .unwrap()
        .insert(idle_child_id, filesystem);
    backend
        .session_repositories()
        .unwrap()
        .insert(idle_child_id, BTreeMap::new());
    let direction_call = ToolCall {
        id: ToolCallId::new(),
        name: "send_project_agent_message".to_owned(),
        arguments: serde_json::json!({
            "target_session_id": idle_child_id,
            "task_id": idle_task.task_id,
            "kind": "direction",
            "body": "Review the current protocol gate and report back."
        }),
    };
    let direction = tools.execute(&direction_call);
    assert!(direction.success, "{}", direction.output);
    let child_inbox = persistence
        .list_agent_messages(idle_task.project_id, idle_child_id, 0, 10)
        .unwrap();
    assert_eq!(child_inbox.len(), 1);
    assert_eq!(child_inbox[0].sender_session_id, root);

    let branch_extension = backend
        .project_agent_tools(
            child_session_id,
            ModelId::new("deterministic/demo"),
            ProjectAgentToolGrants {
                delegation: false,
                messaging: true,
                branch_messaging: true,
                inspection: false,
                child_control: false,
                worktree: false,
                review: false,
                integration: false,
            },
        )
        .unwrap()
        .expect("branch-messaging project tools");
    let branch_tools =
        ToolExecutor::new_with_workspace(connection.session_filesystem(child_session_id).unwrap())
            .with_extension(branch_extension);
    let recipients = branch_tools.execute(&ToolCall {
        id: ToolCallId::new(),
        name: "list_project_message_recipients".to_owned(),
        arguments: serde_json::json!({}),
    });
    assert!(recipients.success, "{}", recipients.output);
    let recipients: serde_json::Value = serde_json::from_str(&recipients.output).unwrap();
    assert_eq!(recipients.as_array().unwrap().len(), 1);
    assert_eq!(
        recipients[0]["session_id"].as_str(),
        Some(idle_child_id.to_string().as_str())
    );
    let branch_message = branch_tools.execute(&ToolCall {
        id: ToolCallId::new(),
        name: "send_project_agent_message".to_owned(),
        arguments: serde_json::json!({
            "target_session_id": idle_child_id,
            "task_id": task.task_id,
            "kind": "progress",
            "body": "I found a related point that may help your review."
        }),
    });
    assert!(branch_message.success, "{}", branch_message.output);
    let denied_branch_message = branch_tools.execute(&ToolCall {
        id: ToolCallId::new(),
        name: "send_project_agent_message".to_owned(),
        arguments: serde_json::json!({
            "target_session_id": ungranted_peer_id,
            "kind": "progress",
            "body": "This recipient has no branch-messaging grant."
        }),
    });
    assert!(!denied_branch_message.success);
    let idle_inbox = persistence
        .list_agent_messages(idle_task.project_id, idle_child_id, 0, 10)
        .unwrap();
    assert_eq!(idle_inbox.len(), 2);
    assert_eq!(idle_inbox[1].sender_session_id, child_session_id);
    assert_eq!(idle_inbox[1].target_session_id, idle_child_id);
    let root_branch_inbox = persistence
        .list_agent_messages(idle_task.project_id, root, 0, 10)
        .unwrap();
    assert!(root_branch_inbox.is_empty());

    let cancel_call = ToolCall {
        id: ToolCallId::new(),
        name: "control_project_child".to_owned(),
        arguments: serde_json::json!({
            "task_id": idle_task.task_id,
            "action": "cancel"
        }),
    };
    assert_eq!(
        tools.action_kind(&cancel_call),
        Some(loom_core::ActionKind::Write)
    );
    let cancellation = tools.execute(&cancel_call);
    assert!(cancellation.success, "{}", cancellation.output);
    let direct_control = connection
        .request(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::ControlProjectChild {
                project_id: idle_task.project_id,
                manager_session_id: root,
                task_id: idle_task.task_id,
                action: ProjectChildControlAction::Cancel,
            },
        )))
        .result
        .unwrap();
    assert!(matches!(
        direct_control,
        ServerResponse::Project(ProjectResponse::ProjectChildControlled{ task, .. })
            if task.status == loom_core::DelegatedTaskStatus::Cancelled
    ));
    assert_eq!(
        persistence
            .load_delegated_task(idle_task.task_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Cancelled
    );
    let after_cancel_message = ToolCall {
        id: ToolCallId::new(),
        name: "send_project_agent_message".to_owned(),
        arguments: serde_json::json!({
            "target_session_id": idle_child_id,
            "task_id": idle_task.task_id,
            "kind": "direction",
            "body": "This new message must be rejected after cancellation."
        }),
    };
    assert!(!tools.execute(&after_cancel_message).success);

    let inspect_call = ToolCall {
        id: ToolCallId::new(),
        name: "list_project_children".to_owned(),
        arguments: serde_json::json!({}),
    };
    let inspection = tools.execute(&inspect_call);
    assert!(inspection.success, "{}", inspection.output);
    let inspection: serde_json::Value = serde_json::from_str(&inspection.output).unwrap();
    let child_session_id_string = child_session_id.to_string();
    assert!(
        inspection["children"]
            .as_array()
            .unwrap()
            .iter()
            .any(|child| {
                child["session_id"].as_str() == Some(child_session_id_string.as_str())
                    && child["task_id"].as_str() == Some(task_id_string.as_str())
            })
    );

    let child_extension = backend
        .project_agent_tools(
            child_session_id,
            ModelId::new("deterministic/demo"),
            ProjectAgentToolGrants {
                delegation: false,
                messaging: true,
                branch_messaging: false,
                inspection: false,
                child_control: false,
                worktree: false,
                review: false,
                integration: false,
            },
        )
        .unwrap()
        .expect("child message tool");
    let child_tools =
        ToolExecutor::new_with_workspace(connection.session_filesystem(child_session_id).unwrap())
            .with_extension(child_extension);
    let report_call = ToolCall {
        id: ToolCallId::new(),
        name: "send_project_agent_message".to_owned(),
        arguments: serde_json::json!({
            "target_session_id": root,
            "kind": "result",
            "body": "Protocol review complete; the upgrade boundary is explicit."
        }),
    };
    assert_eq!(
        child_tools.action_kind(&report_call),
        Some(loom_core::ActionKind::Read)
    );
    let report = child_tools.execute(&report_call);
    assert!(report.success, "{}", report.output);
    let report_retry = child_tools.execute(&report_call);
    assert!(report_retry.success, "{}", report_retry.output);
    let root_inbox = backend
        .persistence
        .as_ref()
        .unwrap()
        .list_agent_messages(ProjectId::from_uuid(*root.as_uuid()), root, 0, 10)
        .unwrap();
    assert_eq!(root_inbox.len(), 1);
    assert_eq!(root_inbox[0].sender_session_id, child_session_id);
    assert_eq!(root_inbox[0].target_session_id, root);
    drop(connection);
    drop(backend);
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn project_child_control_covers_lifecycle_and_parent_grants() {
    let temp = std::env::temp_dir().join(format!(
        "loom-project-child-control-e2e-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&temp).unwrap();
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_endpoint = model.endpoint.clone();
    let model_id = ModelId::new("fixture/project-child-control");
    let provider_registry =
        || scripted_project_provider_registry(&model_endpoint, model_id.clone());
    let backend = InProcessBackend::with_provider_registry_persistent(
        provider_registry(),
        temp.join("state.sqlite"),
    )
    .unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Project child control test".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let manager_session_id = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Manager".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };
    let child_session_id = AgentSessionId::new();
    let task_id = loom_core::TaskId::new();
    let project_id = ProjectId::from_uuid(*manager_session_id.as_uuid());
    let created_at = Timestamp::now();
    let child_name = "Queued child".to_owned();
    let child = AgentSessionSnapshot {
        id: child_session_id,
        workspace_id: workspace.id,
        name: child_name.clone(),
        state: AgentSessionState::Idle,
        created_at,
        updated_at: created_at,
    };
    let task = loom_core::DelegatedTaskRecord {
        task_id,
        project_id,
        requester_session_id: manager_session_id,
        target_session_id: child_session_id,
        child_name,
        intent: "Remain queued until the manager continues the child".to_owned(),
        model_id: "missing-provider/model".to_owned(),
        context_references: vec![],
        dependencies: vec![],
        code_change: false,
        permissions: loom_core::ProjectAgentPermissions::default(),
        status: loom_core::DelegatedTaskStatus::Queued,
        created_at,
        updated_at: created_at,
    };
    let persistence = backend.persistence.as_ref().unwrap();
    persistence
        .create_project_child(
            RequestId::new(),
            &child,
            backend.sessions().unwrap().next_sequence().next(),
            &task,
        )
        .unwrap();
    let (_, created_event) = backend
        .sessions()
        .unwrap()
        .create_in_workspace_with_id(workspace.id, child_session_id, task.child_name.clone())
        .unwrap();
    backend.journal().unwrap().append_session(created_event);
    let filesystem = backend
        .create_session_filesystem(workspace.id, child_session_id)
        .unwrap();
    backend
        .session_filesystems()
        .unwrap()
        .insert(child_session_id, filesystem);
    backend
        .session_repositories()
        .unwrap()
        .insert(child_session_id, BTreeMap::new());

    let control_task = |task_id, action| {
        connection.request(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::ControlProjectChild {
                project_id,
                manager_session_id,
                task_id,
                action,
            },
        )))
    };
    let control = |action| control_task(task_id, action);
    let continued = control(ProjectChildControlAction::Continue);
    let ServerResponse::Project(ProjectResponse::ProjectChildControlled { task, run }) =
        continued.result.unwrap()
    else {
        panic!("unexpected child continue response");
    };
    assert_eq!(task.status, loom_core::DelegatedTaskStatus::Blocked);
    assert!(run.is_none());

    let retried = control(ProjectChildControlAction::Continue);
    let ServerResponse::Project(ProjectResponse::ProjectChildControlled { task, run }) =
        retried.result.unwrap()
    else {
        panic!("unexpected blocked child continue response");
    };
    assert_eq!(task.status, loom_core::DelegatedTaskStatus::Blocked);
    assert!(run.is_none());

    for action in [
        ProjectChildControlAction::Pause,
        ProjectChildControlAction::Interrupt,
        ProjectChildControlAction::RetryFailedStep,
    ] {
        assert_eq!(
            control(action).result.unwrap_err().code,
            ErrorCode::InvalidState
        );
    }

    let resumable_session_id = AgentSessionId::new();
    let resumable_task = loom_core::DelegatedTaskRecord {
        task_id: loom_core::TaskId::new(),
        project_id,
        requester_session_id: manager_session_id,
        target_session_id: resumable_session_id,
        child_name: "Resumable child".to_owned(),
        intent: "Pause and then continue this child".to_owned(),
        model_id: model_id.as_str().to_owned(),
        context_references: vec![],
        dependencies: vec![],
        code_change: false,
        permissions: loom_core::ProjectAgentPermissions::default(),
        status: loom_core::DelegatedTaskStatus::Queued,
        created_at,
        updated_at: created_at,
    };
    let resumable_child = AgentSessionSnapshot {
        id: resumable_session_id,
        workspace_id: workspace.id,
        name: resumable_task.child_name.clone(),
        state: AgentSessionState::Idle,
        created_at,
        updated_at: created_at,
    };
    persistence
        .create_project_child(
            RequestId::new(),
            &resumable_child,
            backend.sessions().unwrap().next_sequence().next(),
            &resumable_task,
        )
        .unwrap();
    let (_, created_event) = backend
        .sessions()
        .unwrap()
        .create_in_workspace_with_id(
            workspace.id,
            resumable_session_id,
            resumable_child.name.clone(),
        )
        .unwrap();
    backend.journal().unwrap().append_session(created_event);
    let filesystem = backend
        .create_session_filesystem(workspace.id, resumable_session_id)
        .unwrap();
    backend
        .session_filesystems()
        .unwrap()
        .insert(resumable_session_id, filesystem);
    backend
        .session_repositories()
        .unwrap()
        .insert(resumable_session_id, BTreeMap::new());
    connection
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::SetSessionApprovalPolicy {
                session_id: resumable_session_id,
                policy: ApprovalPolicy::default(),
                auto_approve_actions: Some(false),
            },
        )))
        .result
        .unwrap();

    let continue_resumable = || {
        connection.request(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::ControlProjectChild {
                project_id,
                manager_session_id,
                task_id: resumable_task.task_id,
                action: ProjectChildControlAction::Continue,
            },
        )))
    };
    let started = continue_resumable();
    let ServerResponse::Project(ProjectResponse::ProjectChildControlled { run, .. }) =
        started.result.unwrap()
    else {
        panic!("unexpected resumable child response");
    };
    let run_id = run.expect("queued child should start a run").id;
    let first_model_turn = model.next_for_child();
    model.respond_with_tool(
        first_model_turn,
        "ask_user",
        serde_json::json!({"prompt": "Which option should I use?"}),
    );
    assert_eq!(
        await_settled_run(&connection, run_id).state,
        AgentRunState::NeedsInput
    );
    for action in [
        ProjectChildControlAction::Continue,
        ProjectChildControlAction::Pause,
    ] {
        assert_eq!(
            control_task(resumable_task.task_id, action)
                .result
                .unwrap_err()
                .code,
            ErrorCode::InvalidState
        );
    }
    let waiting_snapshot = match connection
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::GetAgentRun { run_id },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Run(RunResponse::AgentRun(snapshot)) => snapshot,
        response => panic!("unexpected waiting child response: {response:?}"),
    };
    connection
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::SendAgentMessage {
                run_id,
                attempt_id: waiting_snapshot.attempt_id,
                expected_control_revision: waiting_snapshot.control_revision,
                message: "Use the first option".to_owned(),
            },
        )))
        .result
        .unwrap();
    let approval_turn = model.next_for_child();
    model.respond_with_tool(
        approval_turn,
        "apply_patch",
        serde_json::json!({
            "path": "controlled-child.txt",
            "old_text": "before",
            "new_text": "after"
        }),
    );
    assert_eq!(
        await_settled_run(&connection, run_id).state,
        AgentRunState::AwaitingApproval
    );
    let paused = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::ControlProjectChild {
            project_id,
            manager_session_id,
            task_id: resumable_task.task_id,
            action: ProjectChildControlAction::Pause,
        },
    )));
    let ServerResponse::Project(ProjectResponse::ProjectChildControlled { run, .. }) =
        paused.result.unwrap()
    else {
        panic!("unexpected child pause response");
    };
    assert!(matches!(run, Some(run)
        if run.id == run_id && run.state == AgentRunState::Paused));
    let paused_again = control_task(resumable_task.task_id, ProjectChildControlAction::Pause);
    let ServerResponse::Project(ProjectResponse::ProjectChildControlled { run, .. }) =
        paused_again.result.unwrap()
    else {
        panic!("unexpected repeated child pause response");
    };
    assert!(matches!(run, Some(run)
        if run.id == run_id && run.state == AgentRunState::Paused));

    let resumed = continue_resumable();
    let ServerResponse::Project(ProjectResponse::ProjectChildControlled { run, .. }) =
        resumed.result.unwrap()
    else {
        panic!("unexpected child resume response");
    };
    assert!(matches!(run, Some(run)
        if run.id == run_id && run.state == AgentRunState::AwaitingApproval));
    let retry_while_awaiting_approval = control_task(
        resumable_task.task_id,
        ProjectChildControlAction::RetryFailedStep,
    );
    let ServerResponse::Project(ProjectResponse::ProjectChildControlled { run, .. }) =
        retry_while_awaiting_approval.result.unwrap()
    else {
        panic!("unexpected retry response for a run awaiting approval");
    };
    assert!(matches!(run, Some(run)
        if run.id == run_id && run.state == AgentRunState::AwaitingApproval));
    let approval_events = connection.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetSessionEvents {
            session_id: Some(resumable_session_id),
            workspace_id: None,
            after_sequence: None,
            stream_epoch: None,
        },
    )));
    let ServerResponse::Events(EventsResponse::SessionEvents {
        events: approval_events,
        ..
    }) = approval_events.result.unwrap()
    else {
        panic!("unexpected child approval events response");
    };
    let (approval_call_id, approval_attempt_id, approval_control_revision) = approval_events
        .iter()
        .find_map(|event| match &event.event {
            ServerEvent::Agent {
                event:
                    AgentEvent::ToolApprovalRequired {
                        call,
                        attempt_id,
                        control_revision,
                        ..
                    },
            } if call.name == "apply_patch" => Some((call.id, *attempt_id, *control_revision)),
            _ => None,
        })
        .expect("resumed child should retain its pending tool approval");
    connection
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::ApproveAgentAction {
                run_id,
                attempt_id: approval_attempt_id,
                expected_control_revision: approval_control_revision,
                tool_call_id: approval_call_id,
            },
        )))
        .result
        .unwrap();
    let completion_turn = model.next_for_child();
    model.respond_with_text(completion_turn, "The child completed after approval.");
    assert_eq!(
        await_settled_run(&connection, run_id).state,
        AgentRunState::Completed
    );
    for action in [
        ProjectChildControlAction::Continue,
        ProjectChildControlAction::Pause,
        ProjectChildControlAction::Interrupt,
        ProjectChildControlAction::RetryFailedStep,
    ] {
        assert_eq!(
            connection
                .request(RequestEnvelope::new(ClientRequest::Project(
                    ProjectRequest::ControlProjectChild {
                        project_id,
                        manager_session_id,
                        task_id: resumable_task.task_id,
                        action,
                    }
                )))
                .result
                .unwrap_err()
                .code,
            ErrorCode::InvalidState
        );
    }

    let grandchild_session_id = AgentSessionId::new();
    let grandchild_task = loom_core::DelegatedTaskRecord {
        task_id: loom_core::TaskId::new(),
        project_id,
        requester_session_id: child_session_id,
        target_session_id: grandchild_session_id,
        child_name: "Grandchild".to_owned(),
        intent: "Remain queued under the delegated manager".to_owned(),
        model_id: "missing-provider/model".to_owned(),
        context_references: vec![],
        dependencies: vec![],
        code_change: false,
        permissions: loom_core::ProjectAgentPermissions::default(),
        status: loom_core::DelegatedTaskStatus::Queued,
        created_at,
        updated_at: created_at,
    };
    let grandchild = AgentSessionSnapshot {
        id: grandchild_session_id,
        workspace_id: workspace.id,
        name: grandchild_task.child_name.clone(),
        state: AgentSessionState::Idle,
        created_at,
        updated_at: created_at,
    };
    persistence
        .create_project_child(
            RequestId::new(),
            &grandchild,
            backend.sessions().unwrap().next_sequence().next(),
            &grandchild_task,
        )
        .unwrap();
    let (_, created_event) = backend
        .sessions()
        .unwrap()
        .create_in_workspace_with_id(workspace.id, grandchild_session_id, grandchild.name.clone())
        .unwrap();
    backend.journal().unwrap().append_session(created_event);
    let filesystem = backend
        .create_session_filesystem(workspace.id, grandchild_session_id)
        .unwrap();
    backend
        .session_filesystems()
        .unwrap()
        .insert(grandchild_session_id, filesystem);
    backend
        .session_repositories()
        .unwrap()
        .insert(grandchild_session_id, BTreeMap::new());
    let child_manager_control = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::ControlProjectChild {
            project_id,
            manager_session_id: child_session_id,
            task_id: grandchild_task.task_id,
            action: ProjectChildControlAction::Cancel,
        },
    )));
    assert_eq!(
        control_task(grandchild_task.task_id, ProjectChildControlAction::Cancel)
            .result
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest,
        "the root manager cannot control a non-direct descendant"
    );
    assert_eq!(
        child_manager_control.result.unwrap_err().code,
        ErrorCode::AuthorizationDenied,
        "a child manager without parent-granted control permission cannot control its child"
    );
    assert!(matches!(
        control(ProjectChildControlAction::Cancel).result,
        Ok(ServerResponse::Project(ProjectResponse::ProjectChildControlled{ task, run: None }))
            if task.status == loom_core::DelegatedTaskStatus::Cancelled
    ));
    assert_eq!(
        control(ProjectChildControlAction::Continue)
            .result
            .unwrap_err()
            .code,
        ErrorCode::InvalidState
    );

    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    drop(model);
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn user_direction_reaches_a_parked_project_manager() {
    let temp = std::env::temp_dir().join(format!(
        "loom-project-manager-direction-e2e-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&temp).unwrap();
    let persistence_path = temp.join("state.sqlite");
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_endpoint = model.endpoint.clone();
    let model_id = ModelId::new("fixture/project-manager-direction");
    let backend = InProcessBackend::with_provider_registry_persistent(
        scripted_project_provider_registry(&model_endpoint, model_id.clone()),
        &persistence_path,
    )
    .unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Project manager direction e2e".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let root = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Manager".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };
    assert!(matches!(
        connection
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::SetWorkspaceConfigForWorkspace {
                    workspace_id: workspace.id,
                    config: WorkspaceConfig {
                        project_agent_concurrency: 1,
                        ..WorkspaceConfig::default()
                    },
                }
            ),))
            .result,
        Ok(ServerResponse::Workspace(
            WorkspaceResponse::WorkspaceConfigUpdated
        ))
    ));

    let root_run_id = match connection
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::StartSessionAgentRun {
                session_id: root,
                task: "Coordinate a bounded investigation with a sub-agent.".to_owned(),
                model: model_id.clone(),
                system_instructions: None,
                repository_instructions: None,
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected run response: {response:?}"),
    };

    // Turn one delegates a non-code child.
    let root_delegate = model.next_for_manager();
    assert!(request_has_tool(
        &root_delegate.request,
        "delegate_project_task"
    ));
    model.respond_with_tool(
        root_delegate,
        "delegate_project_task",
        serde_json::json!({
            "child_name": "investigator",
            "intent": "Complete a short investigation and report the finding.",
            "model_id": model_id,
        }),
    );

    // Turn two parks the manager on the child.
    let root_wait = model.next_for_manager();
    assert!(request_has_tool(
        &root_wait.request,
        "wait_for_project_children"
    ));
    let project_id = ProjectId::from_uuid(*root.as_uuid());
    let persistence = backend.persistence.as_ref().unwrap();
    let child_task = persistence
        .list_project_tasks(project_id)
        .unwrap()
        .into_iter()
        .find(|task| task.child_name == "investigator")
        .expect("root delegation should create the investigator task");
    model.respond_with_tool(
        root_wait,
        "wait_for_project_children",
        serde_json::json!({ "task_ids": [child_task.task_id] }),
    );

    assert_eq!(
        await_settled_run(&connection, root_run_id).state,
        AgentRunState::Paused
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while persistence
        .load_delegated_task(child_task.task_id)
        .unwrap()
        .unwrap()
        .status
        != loom_core::DelegatedTaskStatus::Running
    {
        assert!(
            Instant::now() < deadline,
            "parking the manager should release its slot and admit the child"
        );
        thread::sleep(Duration::from_millis(1));
    }
    let wait = persistence
        .list_project_manager_waits_by_child(child_task.task_id)
        .unwrap()
        .into_iter()
        .find(|wait| wait.manager_session_id == root)
        .expect("parked manager wait should be durable");
    assert_eq!(wait.status, loom_core::ProjectManagerWaitStatus::Waiting);

    let parked = await_settled_run(&connection, root_run_id);
    assert_eq!(parked.state, AgentRunState::Paused);

    // A user direction must reach the parked manager instead of being rejected.
    connection
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::SendAgentMessage {
                run_id: root_run_id,
                attempt_id: parked.attempt_id,
                expected_control_revision: parked.control_revision,
                message: "Change of plan: summarize what you have so far.".to_owned(),
            },
        )))
        .result
        .unwrap();

    let redirected = model.next_for_manager();
    let messages = redirected.request["messages"].as_array().unwrap();
    assert!(
        messages.iter().any(|message| {
            message["role"] == "user"
                && message["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("Change of plan"))
        }),
        "the manager should see the new user direction"
    );
    assert!(
        messages.iter().any(|message| {
            message["role"] == "tool"
                && message["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("superseded"))
        }),
        "the superseded wait should be reported back to the manager"
    );
    model.respond_with_tool(
        redirected,
        "ask_user",
        serde_json::json!({"prompt": "Which summary format should I use?"}),
    );

    assert_eq!(
        await_settled_run(&connection, root_run_id).state,
        AgentRunState::NeedsInput
    );
    await_project_manager_wait_status(
        persistence,
        wait.wait_id,
        loom_core::ProjectManagerWaitStatus::Abandoned,
    );

    // The child continues independently and can still finish.
    let child_run_id = persistence
        .load_latest_run_summary_for_session(child_task.target_session_id)
        .unwrap()
        .expect("admitted child should have a durable run")
        .snapshot
        .id;
    let child_turn = model.next_for_child();
    model.respond_with_text(child_turn, "Investigation complete.");
    assert_eq!(
        await_settled_run(&connection, child_run_id).state,
        AgentRunState::Completed
    );

    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    drop(model);
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn user_direction_queues_while_the_run_is_executing() {
    let temp = std::env::temp_dir().join(format!(
        "loom-active-direction-e2e-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&temp).unwrap();
    let persistence_path = temp.join("state.sqlite");
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_endpoint = model.endpoint.clone();
    let model_id = ModelId::new("fixture/active-direction");
    let backend = InProcessBackend::with_provider_registry_persistent(
        scripted_project_provider_registry(&model_endpoint, model_id.clone()),
        &persistence_path,
    )
    .unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Active direction e2e".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let root = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Manager".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };
    let root_run_id = match connection
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::StartSessionAgentRun {
                session_id: root,
                task: "Summarize the repository layout.".to_owned(),
                model: model_id.clone(),
                system_instructions: None,
                repository_instructions: None,
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected run response: {response:?}"),
    };

    // The first model request is in flight, so the run is executing. A queued
    // direction must not error.
    let active_turn = model.next_for_manager();
    let snapshot = match connection
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::GetAgentRun {
                run_id: root_run_id,
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Run(RunResponse::AgentRun(snapshot)) => snapshot,
        response => panic!("unexpected active run response: {response:?}"),
    };
    assert!(matches!(
        snapshot.state,
        AgentRunState::Planning | AgentRunState::Executing | AgentRunState::Evaluating
    ));
    connection
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::SendAgentMessage {
                run_id: root_run_id,
                attempt_id: snapshot.attempt_id,
                expected_control_revision: snapshot.control_revision,
                message: "Focus on the crates directory.".to_owned(),
            },
        )))
        .result
        .unwrap();

    // This step runs a read tool, then the worker delivers the queued direction
    // before issuing the next model request.
    model.respond_with_tool(active_turn, "list_files", serde_json::json!({}));
    let redirected = model.next_for_manager();
    let messages = redirected.request["messages"].as_array().unwrap();
    assert!(
        messages.iter().any(|message| {
            message["role"] == "user"
                && message["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("Focus on the crates directory"))
        }),
        "the queued direction should be delivered on the next model turn"
    );
    model.respond_with_text(redirected, "Repository layout summarized.");

    assert_eq!(
        await_settled_run(&connection, root_run_id).state,
        AgentRunState::Completed
    );

    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    drop(model);
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn child_completion_wakes_a_finished_manager() {
    let temp = std::env::temp_dir().join(format!("loom-child-wake-e2e-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&temp).unwrap();
    let persistence_path = temp.join("state.sqlite");
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_endpoint = model.endpoint.clone();
    let model_id = ModelId::new("fixture/child-wake");
    let backend = InProcessBackend::with_provider_registry_persistent(
        scripted_project_provider_registry(&model_endpoint, model_id.clone()),
        &persistence_path,
    )
    .unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Child wake e2e".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let root = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Manager".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };
    let root_run_id = match connection
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::StartSessionAgentRun {
                session_id: root,
                task: "Delegate a bounded task and continue with me meanwhile.".to_owned(),
                model: model_id.clone(),
                system_instructions: None,
                repository_instructions: None,
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected run response: {response:?}"),
    };
    let project_id = ProjectId::from_uuid(*root.as_uuid());
    let persistence = backend.persistence.as_ref().unwrap();

    // Turn one delegates a non-code child.
    let root_delegate = model.next_for_manager();
    assert!(request_has_tool(
        &root_delegate.request,
        "delegate_project_task"
    ));
    model.respond_with_tool(
        root_delegate,
        "delegate_project_task",
        serde_json::json!({
            "child_name": "worker",
            "intent": "Do a short piece of work.",
            "model_id": model_id,
        }),
    );

    // Turn two replies to the user and ends the turn. An active child no longer
    // blocks completion, which is what makes "continue here" work.
    let root_reply = model.next_for_manager();
    model.respond_with_text(root_reply, "Delegated. Continuing with you now.");
    assert_eq!(
        await_settled_run(&connection, root_run_id).state,
        AgentRunState::Completed
    );

    let child_task = persistence
        .list_project_tasks(project_id)
        .unwrap()
        .into_iter()
        .find(|task| task.child_name == "worker")
        .expect("delegation should create the worker task");
    assert_eq!(child_task.status, loom_core::DelegatedTaskStatus::Running);
    let child_run_id = persistence
        .load_latest_run_summary_for_session(child_task.target_session_id)
        .unwrap()
        .expect("child should have a durable run")
        .snapshot
        .id;

    // The child finishes without sending its own result; the server synthesizes
    // a durable result and wakes the finished manager.
    let child_turn = model.next_for_child();
    model.respond_with_text(child_turn, "Child work complete.");
    assert_eq!(
        await_settled_run(&connection, child_run_id).state,
        AgentRunState::Completed
    );

    let woken = model.next_for_manager();
    let messages = woken.request["messages"].as_array().unwrap();
    assert!(
        messages.iter().any(|message| {
            message["name"] == "loom_project_message"
                && message["content"].as_str().is_some_and(|content| {
                    content.contains("worker") && content.contains("Completed")
                })
        }),
        "the woken manager should see the child result"
    );
    model.respond_with_text(woken, "Thanks, I have the child result.");
    assert_eq!(
        await_settled_run(&connection, root_run_id).state,
        AgentRunState::Completed
    );

    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    drop(model);
    fs::remove_dir_all(temp).unwrap();
}

#[cfg(unix)]
#[test]
fn cancelling_a_child_running_a_sleep_command_stops_promptly() {
    let temp = std::env::temp_dir().join(format!(
        "loom-child-sleep-cancel-e2e-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&temp).unwrap();
    let persistence_path = temp.join("state.sqlite");
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_endpoint = model.endpoint.clone();
    let model_id = ModelId::new("fixture/child-sleep-cancel");
    let backend = InProcessBackend::with_provider_registry_persistent(
        scripted_project_provider_registry(&model_endpoint, model_id.clone()),
        &persistence_path,
    )
    .unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Child sleep cancel e2e".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let root = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Manager".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };
    let root_run_id = match connection
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::StartSessionAgentRun {
                session_id: root,
                task: "Delegate a long sleep task.".to_owned(),
                model: model_id.clone(),
                system_instructions: None,
                repository_instructions: None,
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected run response: {response:?}"),
    };
    let project_id = ProjectId::from_uuid(*root.as_uuid());
    let persistence = backend.persistence.as_ref().unwrap();

    let root_delegate = model.next_for_manager();
    assert!(request_has_tool(
        &root_delegate.request,
        "delegate_project_task"
    ));
    model.respond_with_tool(
        root_delegate,
        "delegate_project_task",
        serde_json::json!({
            "child_name": "sleeper",
            "intent": "Run a long sleep command.",
            "model_id": model_id,
        }),
    );

    // The child's first model request only arrives once its task, session, and
    // run exist, so wait for it before looking the task up.
    let child_turn = model.next_for_child();
    let child_task = persistence
        .list_project_tasks(project_id)
        .unwrap()
        .into_iter()
        .find(|task| task.child_name == "sleeper")
        .expect("delegation should create the sleeper task");
    let child_run_id = persistence
        .load_latest_run_summary_for_session(child_task.target_session_id)
        .unwrap()
        .expect("child should have a durable run")
        .snapshot
        .id;

    model.respond_with_tool(
        child_turn,
        "run_command",
        serde_json::json!({"command": "sleep", "args": ["30"], "timeout_ms": 30_000}),
    );

    // Wait until the child is actually blocked inside the command. The activity
    // is in the live run handle because the step has not checkpointed yet.
    let child_handle = connection
        .backend
        .runs()
        .unwrap()
        .get(&child_run_id)
        .cloned()
        .expect("child run should be registered");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let running = child_handle.state().activities.iter().any(|activity| {
            activity.status == loom_protocol::AgentActivityStatus::Started
                && matches!(
                    &activity.data,
                    loom_protocol::AgentActivityData::Command { command, .. }
                        if command == "sleep"
                )
        });
        if running {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the child should start its sleep command; state={:?}",
            child_handle.snapshot().state,
        );
        thread::sleep(Duration::from_millis(10));
    }

    let started = Instant::now();
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::ControlProjectChild {
                project_id,
                manager_session_id: root,
                task_id: child_task.task_id,
                action: ProjectChildControlAction::Cancel,
            },
        )))
        .result
        .unwrap();
    assert!(matches!(
        response,
        ServerResponse::Project(ProjectResponse::ProjectChildControlled { .. })
    ));
    assert_eq!(
        await_settled_run(&connection, child_run_id).state,
        AgentRunState::Cancelled
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "cancelling a child must terminate its running command promptly"
    );

    // Finish the manager's pending turn so teardown does not wait on a held
    // model request.
    let root_followup = model.next_for_manager();
    model.respond_with_text(root_followup, "The sleeper was cancelled.");
    let _ = await_settled_run(&connection, root_run_id);

    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    drop(model);
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn archiving_a_session_with_a_deferred_run_stops_it() {
    let persistence =
        std::env::temp_dir().join(format!("loom-deferred-archive-{}.db", uuid::Uuid::new_v4()));
    let (session_id, run_id) = {
        let backend = InProcessBackend::new_persistent(&persistence).unwrap();
        let connection = backend.connect();
        negotiate_m3(&connection);
        let workspace = match connection
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::CreateWorkspace {
                    name: "Deferred archive".to_owned(),
                },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
            response => panic!("unexpected workspace response: {response:?}"),
        };
        let session_id = match connection
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::CreateAgentSessionInWorkspace {
                    workspace_id: workspace.id,
                    name: "deferred".to_owned(),
                },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
            response => panic!("unexpected session response: {response:?}"),
        };
        // Require approval so the run parks in a non-terminal state that is
        // deferred on restore.
        assert!(
            connection
                .request(RequestEnvelope::new(ClientRequest::Session(
                    SessionRequest::SetSessionApprovalPolicy {
                        session_id,
                        policy: ApprovalPolicy::default(),
                        auto_approve_actions: Some(false),
                    },
                )))
                .result
                .is_ok()
        );
        let run_id = match connection
            .request(RequestEnvelope::new(ClientRequest::Run(
                RunRequest::StartSessionAgentRun {
                    session_id,
                    task: "create a demo file".to_owned(),
                    model: ModelId::new("deterministic/demo"),
                    system_instructions: None,
                    repository_instructions: None,
                },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
            response => panic!("unexpected run response: {response:?}"),
        };
        await_settled_run(&connection, run_id);
        backend.flush().unwrap();
        backend.shutdown().unwrap();
        (session_id, run_id)
    };
    let backend = InProcessBackend::new_persistent(&persistence).unwrap();
    assert!(
        backend.runs().unwrap().is_empty(),
        "a non-terminal run is deferred on restore, so no handle is registered"
    );
    let connection = backend.connect();
    negotiate_m3(&connection);
    let archived = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::ArchiveAgentSession { session_id },
    )));
    assert!(matches!(
        archived.result,
        Ok(ServerResponse::Session(
            SessionResponse::AgentSessionArchived(_)
        ))
    ));
    assert_eq!(
        backend
            .persistence
            .as_ref()
            .unwrap()
            .load_run_summary(run_id)
            .unwrap()
            .unwrap()
            .snapshot
            .state,
        AgentRunState::Cancelled,
        "the deferred run must be stopped before the session is archived"
    );
    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    let _ = fs::remove_file(&persistence);
}

#[test]
fn archiving_a_session_with_a_stale_active_state_recovers() {
    let persistence =
        std::env::temp_dir().join(format!("loom-stale-archive-{}.db", uuid::Uuid::new_v4()));
    let backend = InProcessBackend::new_persistent(&persistence).unwrap();
    let connection = backend.connect();
    negotiate_m3(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Stale archive".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let session_id = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "stale".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };
    // Simulate a session left active by an earlier crash: the state says
    // Executing but there is no run at all.
    connection
        .backend
        .sessions()
        .unwrap()
        .transition(session_id, AgentSessionState::Executing)
        .unwrap();
    let archived = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::ArchiveAgentSession { session_id },
    )));
    assert!(matches!(
        archived.result,
        Ok(ServerResponse::Session(
            SessionResponse::AgentSessionArchived(_)
        ))
    ));
    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    let _ = fs::remove_file(&persistence);
}

#[test]
fn durable_manager_wait_releases_workspace_slot_and_resumes_once() {
    let temp = std::env::temp_dir().join(format!(
        "loom-project-manager-wait-e2e-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&temp).unwrap();
    let persistence_path = temp.join("state.sqlite");
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_endpoint = model.endpoint.clone();
    let model_id = ModelId::new("fixture/project-manager-wait");
    let backend = InProcessBackend::with_provider_registry_persistent(
        scripted_project_provider_registry(&model_endpoint, model_id.clone()),
        &persistence_path,
    )
    .unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Project manager wait e2e".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let root = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Manager".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };
    assert!(matches!(
        connection
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::SetWorkspaceConfigForWorkspace {
                    workspace_id: workspace.id,
                    config: WorkspaceConfig {
                        project_agent_concurrency: 1,
                        ..WorkspaceConfig::default()
                    },
                }
            ),))
            .result,
        Ok(ServerResponse::Workspace(
            WorkspaceResponse::WorkspaceConfigUpdated
        ))
    ));

    let root_run_id = match connection
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::StartSessionAgentRun {
                session_id: root,
                task: "Coordinate a bounded investigation with a submanager.".to_owned(),
                model: model_id.clone(),
                system_instructions: None,
                repository_instructions: None,
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected run response: {response:?}"),
    };

    let root_delegate = model.next_for_manager();
    assert!(request_has_tool(
        &root_delegate.request,
        "delegate_project_task"
    ));
    model.respond_with_tool(
        root_delegate,
        "delegate_project_task",
        serde_json::json!({
            "child_name": "submanager",
            "intent": "Delegate one focused investigation, wait for it, and report the result.",
            "model_id": model_id,
            "permissions": { "delegation": true, "inspection": true }
        }),
    );
    // Keep the root's next turn open while the delegated manager runs.
    let root_followup = model.next_for_manager();

    let project_id = ProjectId::from_uuid(*root.as_uuid());
    let persistence = backend.persistence.as_ref().unwrap();
    let manager_task = persistence
        .list_project_tasks(project_id)
        .unwrap()
        .into_iter()
        .find(|task| task.child_name == "submanager")
        .expect("root delegation should create the submanager task");

    let manager_delegate = model.next_for_child();
    assert!(request_has_tool(
        &manager_delegate.request,
        "delegate_project_task"
    ));
    model.respond_with_tool(
        manager_delegate,
        "delegate_project_task",
        serde_json::json!({
            "child_name": "investigator",
            "intent": "Complete a short investigation and report the finding.",
            "model_id": model_id,
            "dependencies": []
        }),
    );

    let manager_wait = model.next_for_child();
    assert!(request_has_tool(
        &manager_wait.request,
        "wait_for_project_children"
    ));
    let grandchild_task = persistence
        .list_project_tasks(project_id)
        .unwrap()
        .into_iter()
        .find(|task| task.requester_session_id == manager_task.target_session_id)
        .expect("submanager delegation should create its child task");
    assert_eq!(
        grandchild_task.status,
        loom_core::DelegatedTaskStatus::Queued
    );
    let manager_run_id = persistence
        .load_latest_run_summary_for_session(manager_task.target_session_id)
        .unwrap()
        .expect("submanager run should have a durable checkpoint")
        .snapshot
        .id;
    model.respond_with_tool(
        manager_wait,
        "wait_for_project_children",
        serde_json::json!({ "task_ids": [grandchild_task.task_id] }),
    );

    assert_eq!(
        await_settled_run(&connection, manager_run_id).state,
        AgentRunState::Paused
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let (manager_task_after_park, grandchild_task_after_park) = loop {
        let manager_task = persistence
            .load_delegated_task(manager_task.task_id)
            .unwrap()
            .unwrap();
        let grandchild_task = persistence
            .load_delegated_task(grandchild_task.task_id)
            .unwrap()
            .unwrap();
        if manager_task.status == loom_core::DelegatedTaskStatus::Blocked
            && grandchild_task.status == loom_core::DelegatedTaskStatus::Running
        {
            break (manager_task, grandchild_task);
        }
        assert!(
            Instant::now() < deadline,
            "parked manager should release its slot and start its queued child; manager={:?}, child={:?}",
            manager_task.status,
            grandchild_task.status
        );
        thread::sleep(Duration::from_millis(1));
    };
    assert_eq!(
        manager_task_after_park.status,
        loom_core::DelegatedTaskStatus::Blocked
    );
    assert_eq!(
        grandchild_task_after_park.status,
        loom_core::DelegatedTaskStatus::Running
    );
    let wait = persistence
        .list_project_manager_waits_by_child(grandchild_task.task_id)
        .unwrap()
        .into_iter()
        .find(|wait| wait.manager_session_id == manager_task.target_session_id)
        .expect("parked submanager wait should be durable");
    assert_eq!(wait.status, loom_core::ProjectManagerWaitStatus::Waiting);

    let deadline = Instant::now() + Duration::from_secs(2);
    while Timestamp::now() <= wait.created_at {
        assert!(
            Instant::now() < deadline,
            "clock should advance past the durable wait timestamp before creating a newer task"
        );
        thread::sleep(Duration::from_millis(1));
    }
    let sibling = connection
        .create_project_child(
            RequestId::new(),
            root,
            "root-sibling".to_owned(),
            loom_core::DelegatedTaskSpec {
                intent: "Complete a short independent follow-up.".to_owned(),
                model_id: model_id.as_str().to_owned(),
                context_references: vec![],
                dependencies: vec![],
                code_change: false,
                permissions: loom_core::ProjectAgentPermissions::default(),
            },
        )
        .unwrap();
    let sibling_task = match sibling {
        ServerResponse::Project(ProjectResponse::ProjectChildCreated { task, .. }) => task,
        response => panic!("unexpected sibling task response: {response:?}"),
    };
    assert!(sibling_task.created_at > wait.created_at);
    assert_eq!(sibling_task.status, loom_core::DelegatedTaskStatus::Queued);

    let grandchild_turn = model.next_for_child();
    model.respond_with_text(
        grandchild_turn,
        "The investigation is complete: the finding is confirmed.",
    );
    assert_eq!(
        await_settled_run(
            &connection,
            persistence
                .load_latest_run_summary_for_session(grandchild_task.target_session_id)
                .unwrap()
                .expect("grandchild run should have a durable checkpoint")
                .snapshot
                .id
        )
        .state,
        AgentRunState::Completed
    );

    let resumed_manager_turn = model.next_for_child();
    let wait_call_id = wait.tool_call_id.to_string();
    let wait_results = resumed_manager_turn.request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| {
            message["role"] == "tool"
                && message["tool_call_id"] == wait_call_id
                && message["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("\"return_ready\":true"))
        })
        .count();
    assert_eq!(wait_results, 1, "the durable join should resume once");
    assert!(request_has_tool(
        &resumed_manager_turn.request,
        "wait_for_project_children"
    ));
    assert_eq!(
        persistence
            .load_delegated_task(manager_task.task_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Running
    );
    assert_eq!(
        persistence
            .load_delegated_task(sibling_task.task_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Queued,
        "the older ready join must claim the only workspace slot first"
    );
    assert_eq!(
        persistence
            .load_project_manager_wait(wait.wait_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::ProjectManagerWaitStatus::Resuming
    );
    model.respond_with_text(
        resumed_manager_turn,
        "The investigator confirmed the finding; the delegated work is complete.",
    );
    assert_eq!(
        await_settled_run(&connection, manager_run_id).state,
        AgentRunState::Completed
    );
    assert_eq!(
        await_project_manager_wait_status(
            persistence,
            wait.wait_id,
            loom_core::ProjectManagerWaitStatus::Consumed,
        )
        .status,
        loom_core::ProjectManagerWaitStatus::Consumed
    );
    let manager_join_results = persistence
        .load_run_messages(manager_run_id)
        .unwrap()
        .into_iter()
        .filter(|message| {
            message.role == loom_model::MessageRole::Tool
                && message.name.as_deref() == Some("wait_for_project_children")
                && message.tool_call_id == Some(wait.tool_call_id)
        })
        .count();
    assert_eq!(manager_join_results, 1);
    assert_eq!(
        persistence
            .load_delegated_task(grandchild_task.task_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Completed
    );
    let sibling_turn = model.next_for_child();
    assert_eq!(
        persistence
            .load_delegated_task(manager_task.task_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Completed
    );
    let sibling_run_id = persistence
        .load_latest_run_summary_for_session(sibling_task.target_session_id)
        .unwrap()
        .expect("root sibling should have started after the manager completed")
        .snapshot
        .id;
    assert_eq!(
        persistence
            .load_delegated_task(sibling_task.task_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Running
    );
    model.respond_with_text(sibling_turn, "The independent follow-up is complete.");
    assert_eq!(
        await_settled_run(&connection, sibling_run_id).state,
        AgentRunState::Completed
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = persistence
            .load_delegated_task(sibling_task.task_id)
            .unwrap()
            .unwrap()
            .status;
        if status == loom_core::DelegatedTaskStatus::Completed {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "completed sibling run should release its delegated task slot; status={status:?}"
        );
        thread::sleep(Duration::from_millis(1));
    }

    model.respond_with_text(
        root_followup,
        "The submanager completed the investigation and confirmed the finding.",
    );
    assert_eq!(
        await_settled_run(&connection, root_run_id).state,
        AgentRunState::Completed
    );

    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    drop(model);
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn durable_manager_wait_recovers_after_restart_and_resumes_once() {
    let temp = std::env::temp_dir().join(format!(
        "loom-project-manager-wait-restart-e2e-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&temp).unwrap();
    let persistence_path = temp.join("state.sqlite");
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_endpoint = model.endpoint.clone();
    let model_id = ModelId::new("fixture/project-manager-wait-restart");
    let provider_registry =
        || scripted_project_provider_registry(&model_endpoint, model_id.clone());
    let backend =
        InProcessBackend::with_provider_registry_persistent(provider_registry(), &persistence_path)
            .unwrap();
    // This exercises the persisted nested-delegation grant at depth two.

    let connection = backend.connect();
    negotiate(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Project manager wait restart e2e".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let root = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Project root".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };
    assert!(matches!(
        connection
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::SetWorkspaceConfigForWorkspace {
                    workspace_id: workspace.id,
                    config: WorkspaceConfig {
                        project_agent_concurrency: 1,
                        ..WorkspaceConfig::default()
                    },
                }
            ),))
            .result,
        Ok(ServerResponse::Workspace(
            WorkspaceResponse::WorkspaceConfigUpdated
        ))
    ));

    let (manager_task, manager_session) = match connection
        .create_project_child(
            RequestId::new(),
            root,
            "submanager".to_owned(),
            loom_core::DelegatedTaskSpec {
                intent: "Delegate one child, wait for it, and summarize its result.".to_owned(),
                model_id: model_id.as_str().to_owned(),
                context_references: vec![],
                dependencies: vec![],
                code_change: false,
                permissions: loom_core::ProjectAgentPermissions {
                    delegation: true,
                    inspection: true,
                    ..loom_core::ProjectAgentPermissions::default()
                },
            },
        )
        .unwrap()
    {
        ServerResponse::Project(ProjectResponse::ProjectChildCreated { task, child }) => {
            (task, child)
        }
        response => panic!("unexpected manager creation response: {response:?}"),
    };
    assert_eq!(manager_task.requester_session_id, root);
    assert_eq!(manager_session.depth, 2);

    let manager_delegate = model.next_for_child();
    assert!(request_has_tool(
        &manager_delegate.request,
        "delegate_project_task"
    ));

    // Create an older queued prerequisite while the manager holds the
    // single workspace slot. After the manager parks, it takes the slot.
    // The grandchild depends on it, so the wait and queued grandchild stay
    // pending while the prerequisite is paused during shutdown.
    let sibling = connection
        .create_project_child(
            RequestId::new(),
            root,
            "older-sibling".to_owned(),
            loom_core::DelegatedTaskSpec {
                intent: "Complete a short independent follow-up.".to_owned(),
                model_id: model_id.as_str().to_owned(),
                context_references: vec![],
                dependencies: vec![],
                code_change: false,
                permissions: loom_core::ProjectAgentPermissions::default(),
            },
        )
        .unwrap();
    let sibling_task = match sibling {
        ServerResponse::Project(ProjectResponse::ProjectChildCreated { task, .. }) => task,
        response => panic!("unexpected sibling task response: {response:?}"),
    };
    assert_eq!(sibling_task.status, loom_core::DelegatedTaskStatus::Queued);
    let timestamp_deadline = Instant::now() + Duration::from_secs(2);
    while Timestamp::now() <= sibling_task.created_at {
        assert!(
            Instant::now() < timestamp_deadline,
            "clock should advance before creating the joined grandchild"
        );
        thread::sleep(Duration::from_millis(1));
    }

    model.respond_with_tool(
        manager_delegate,
        "delegate_project_task",
        serde_json::json!({
            "child_name": "investigator",
            "intent": "Complete a bounded investigation and report the finding.",
            "model_id": model_id,
            "dependencies": [sibling_task.task_id]
        }),
    );
    let manager_wait = model.next_for_child();
    assert!(request_has_tool(
        &manager_wait.request,
        "wait_for_project_children"
    ));

    let project_id = ProjectId::from_uuid(*root.as_uuid());
    let persistence = backend.persistence.as_ref().unwrap();
    let grandchild_task = persistence
        .list_project_tasks(project_id)
        .unwrap()
        .into_iter()
        .find(|task| task.requester_session_id == manager_session.session_id)
        .expect("manager delegation should create the grandchild task");
    assert!(sibling_task.created_at < grandchild_task.created_at);
    assert_eq!(
        grandchild_task.status,
        loom_core::DelegatedTaskStatus::Queued
    );
    let manager_run_id = persistence
        .load_latest_run_summary_for_session(manager_session.session_id)
        .unwrap()
        .expect("manager run should have a durable checkpoint")
        .snapshot
        .id;
    model.respond_with_tool(
        manager_wait,
        "wait_for_project_children",
        serde_json::json!({ "task_ids": [grandchild_task.task_id] }),
    );
    assert_eq!(
        await_settled_run(&connection, manager_run_id).state,
        AgentRunState::Paused
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    let (manager_task_after_park, sibling_after_park, grandchild_after_park) = loop {
        let manager_task = persistence
            .load_delegated_task(manager_task.task_id)
            .unwrap()
            .unwrap();
        let sibling_task = persistence
            .load_delegated_task(sibling_task.task_id)
            .unwrap()
            .unwrap();
        let grandchild_task = persistence
            .load_delegated_task(grandchild_task.task_id)
            .unwrap()
            .unwrap();
        if manager_task.status == loom_core::DelegatedTaskStatus::Blocked
            && sibling_task.status == loom_core::DelegatedTaskStatus::Running
            && grandchild_task.status == loom_core::DelegatedTaskStatus::Queued
        {
            break (manager_task, sibling_task, grandchild_task);
        }
        assert!(
            Instant::now() < deadline,
            "parked manager should admit only the older sibling; manager={:?}, sibling={:?}, grandchild={:?}",
            manager_task.status,
            sibling_task.status,
            grandchild_task.status
        );
        thread::sleep(Duration::from_millis(1));
    };
    assert_eq!(
        manager_task_after_park.status,
        loom_core::DelegatedTaskStatus::Blocked
    );
    assert_eq!(
        sibling_after_park.status,
        loom_core::DelegatedTaskStatus::Running
    );
    assert_eq!(
        grandchild_after_park.status,
        loom_core::DelegatedTaskStatus::Queued
    );
    let wait = persistence
        .list_project_manager_waits_by_child(grandchild_task.task_id)
        .unwrap()
        .into_iter()
        .find(|wait| wait.manager_session_id == manager_session.session_id)
        .expect("parked manager wait should be durable");
    assert_eq!(wait.status, loom_core::ProjectManagerWaitStatus::Waiting);
    let manager_execution = persistence
        .load_run_execution_state(manager_run_id)
        .unwrap()
        .expect("manager continuation should be persisted");
    assert!(manager_execution.pending_project_join.is_some());
    let sibling_turn = model.next_for_child();
    assert!(!request_has_tool(
        &sibling_turn.request,
        "wait_for_project_children"
    ));
    let sibling_stream = hold_scripted_model_stream_until_cancelled(sibling_turn);
    drop(connection);
    backend.shutdown().unwrap();
    sibling_stream.join().unwrap();
    assert_eq!(
        persistence
            .load_project_manager_wait(wait.wait_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::ProjectManagerWaitStatus::Waiting
    );
    assert!(
        persistence
            .load_run_execution_state(manager_run_id)
            .unwrap()
            .unwrap()
            .pending_project_join
            .is_some()
    );
    assert!(
        persistence
            .load_latest_run_summary_for_session(grandchild_task.target_session_id)
            .unwrap()
            .is_none()
    );
    drop(backend);

    let reopened =
        InProcessBackend::with_provider_registry_persistent(provider_registry(), &persistence_path)
            .unwrap();
    assert!(
        reopened
            .supported_capabilities
            .contains(Capability::CreateNestedProjectChild)
    );
    let reopened_connection = reopened.connect();
    negotiate(&reopened_connection);
    let reopened_persistence = reopened.persistence.as_ref().unwrap();
    let sibling_run_id = reopened_persistence
        .load_latest_run_summary_for_session(sibling_task.target_session_id)
        .unwrap()
        .expect("prerequisite run should be durable after shutdown")
        .snapshot
        .id;
    assert_eq!(
        reopened_persistence
            .load_run_summary(manager_run_id)
            .unwrap()
            .unwrap()
            .snapshot
            .state,
        AgentRunState::Paused
    );
    assert!(
        reopened_persistence
            .load_run_execution_state(manager_run_id)
            .unwrap()
            .unwrap()
            .pending_project_join
            .is_some()
    );
    assert_eq!(
        reopened_persistence
            .load_project_manager_wait(wait.wait_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::ProjectManagerWaitStatus::Waiting
    );
    assert_eq!(
        reopened_persistence
            .load_run_summary(sibling_run_id)
            .unwrap()
            .unwrap()
            .snapshot
            .state,
        AgentRunState::Paused
    );
    assert_eq!(
        reopened_persistence
            .load_delegated_task(sibling_task.task_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Blocked
    );
    assert_eq!(
        reopened_persistence
            .load_delegated_task(grandchild_task.task_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Queued
    );

    // The grandchild remains queued because its prerequisite was paused.
    // Resume and finish that prerequisite, then complete the admitted
    // grandchild; its completion should resume the manager exactly once.
    let resumed_prerequisite = reopened_connection.request(RequestEnvelope::new(
        ClientRequest::Run(RunRequest::ResumeAgentRun {
            run_id: sibling_run_id,
        }),
    ));
    let ServerResponse::Run(RunResponse::AgentRun(resumed_prerequisite)) =
        resumed_prerequisite.result.unwrap()
    else {
        panic!("unexpected prerequisite resume response");
    };
    assert_ne!(resumed_prerequisite.state, AgentRunState::Paused);
    let prerequisite_turn = model.next_for_child();
    model.respond_with_text(
        prerequisite_turn,
        "The independent prerequisite is complete.",
    );
    assert_eq!(
        await_settled_run(&reopened_connection, sibling_run_id).state,
        AgentRunState::Completed
    );

    let grandchild_turn = model.next_for_child();
    let grandchild_run_id = reopened_persistence
        .load_latest_run_summary_for_session(grandchild_task.target_session_id)
        .unwrap()
        .expect("restart should admit the queued grandchild")
        .snapshot
        .id;
    model.respond_with_text(
        grandchild_turn,
        "The investigation is complete: the finding is confirmed.",
    );
    assert_eq!(
        await_settled_run(&reopened_connection, grandchild_run_id).state,
        AgentRunState::Completed
    );

    let resumed_manager_turn = model.next_for_child();
    let wait_call_id = wait.tool_call_id.to_string();
    let wait_results = resumed_manager_turn.request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| {
            message["role"] == "tool"
                && message["tool_call_id"] == wait_call_id
                && message["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("\"return_ready\":true"))
        })
        .count();
    assert_eq!(
        wait_results, 1,
        "the persisted wait result should replay once"
    );
    assert_eq!(
        reopened_persistence
            .load_project_manager_wait(wait.wait_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::ProjectManagerWaitStatus::Resuming
    );
    model.respond_with_text(
        resumed_manager_turn,
        "The investigator confirmed the finding; delegated work is complete.",
    );
    assert_eq!(
        await_settled_run(&reopened_connection, manager_run_id).state,
        AgentRunState::Completed
    );
    assert_eq!(
        await_project_manager_wait_status(
            reopened_persistence,
            wait.wait_id,
            loom_core::ProjectManagerWaitStatus::Consumed,
        )
        .status,
        loom_core::ProjectManagerWaitStatus::Consumed
    );
    let durable_wait_results = reopened_persistence
        .load_run_messages(manager_run_id)
        .unwrap()
        .into_iter()
        .filter(|message| {
            message.role == loom_model::MessageRole::Tool
                && message.name.as_deref() == Some("wait_for_project_children")
                && message.tool_call_id == Some(wait.tool_call_id)
        })
        .count();
    assert_eq!(durable_wait_results, 1);
    assert_eq!(
        reopened_persistence
            .load_delegated_task(grandchild_task.task_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Completed
    );

    drop(reopened_connection);
    reopened.shutdown().unwrap();
    drop(reopened);
    drop(model);
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn cancelled_prerequisite_blocks_child_and_releases_manager_wait_once() {
    dependency_failure_blocks_child_and_releases_manager_wait_once(false);
}

#[test]
fn failed_prerequisite_blocks_child_and_releases_manager_wait_once() {
    dependency_failure_blocks_child_and_releases_manager_wait_once(true);
}

fn dependency_failure_blocks_child_and_releases_manager_wait_once(prerequisite_fails: bool) {
    let temp = std::env::temp_dir().join(format!(
        "loom-project-manager-wait-cancelled-dependency-e2e-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&temp).unwrap();
    let persistence_path = temp.join("state.sqlite");
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_endpoint = model.endpoint.clone();
    let model_id = ModelId::new("fixture/project-manager-wait-cancelled-dependency");
    let provider_registry =
        || scripted_project_provider_registry(&model_endpoint, model_id.clone());
    let backend =
        InProcessBackend::with_provider_registry_persistent(provider_registry(), &persistence_path)
            .unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Failed dependency manager wait e2e".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let root = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Project root".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };
    assert!(matches!(
        connection
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::SetWorkspaceConfigForWorkspace {
                    workspace_id: workspace.id,
                    config: WorkspaceConfig {
                        project_agent_concurrency: 1,
                        ..WorkspaceConfig::default()
                    },
                }
            ),))
            .result,
        Ok(ServerResponse::Workspace(
            WorkspaceResponse::WorkspaceConfigUpdated
        ))
    ));

    let (manager_task, manager_session) = match connection
        .create_project_child(
            RequestId::new(),
            root,
            "submanager".to_owned(),
            loom_core::DelegatedTaskSpec {
                intent: "Delegate one task, wait for its result, and report it.".to_owned(),
                model_id: model_id.as_str().to_owned(),
                context_references: vec![],
                dependencies: vec![],
                code_change: false,
                permissions: loom_core::ProjectAgentPermissions {
                    delegation: true,
                    inspection: true,
                    ..loom_core::ProjectAgentPermissions::default()
                },
            },
        )
        .unwrap()
    {
        ServerResponse::Project(ProjectResponse::ProjectChildCreated { task, child }) => {
            (task, child)
        }
        response => panic!("unexpected manager creation response: {response:?}"),
    };
    assert_eq!(manager_task.requester_session_id, root);
    assert_eq!(manager_session.depth, 2);

    let manager_delegate = model.next_for_child();
    assert!(request_has_tool(
        &manager_delegate.request,
        "delegate_project_task"
    ));

    // The older direct child becomes the prerequisite and takes the only
    // slot when the manager parks. Cancelling it makes the dependent task
    // Blocked, which must release the manager's wait as return-ready.
    let prerequisite = connection
        .create_project_child(
            RequestId::new(),
            root,
            "prerequisite".to_owned(),
            loom_core::DelegatedTaskSpec {
                intent: "Complete the prerequisite investigation.".to_owned(),
                model_id: model_id.as_str().to_owned(),
                context_references: vec![],
                dependencies: vec![],
                code_change: false,
                permissions: loom_core::ProjectAgentPermissions::default(),
            },
        )
        .unwrap();
    let prerequisite_task = match prerequisite {
        ServerResponse::Project(ProjectResponse::ProjectChildCreated { task, .. }) => task,
        response => panic!("unexpected prerequisite task response: {response:?}"),
    };
    assert_eq!(
        prerequisite_task.status,
        loom_core::DelegatedTaskStatus::Queued
    );
    let timestamp_deadline = Instant::now() + Duration::from_secs(2);
    while Timestamp::now() <= prerequisite_task.created_at {
        assert!(
            Instant::now() < timestamp_deadline,
            "clock should advance before creating the dependent child"
        );
        thread::sleep(Duration::from_millis(1));
    }

    model.respond_with_tool(
        manager_delegate,
        "delegate_project_task",
        serde_json::json!({
            "child_name": "dependent",
            "intent": "Run only after the prerequisite task completes.",
            "model_id": model_id,
            "dependencies": [prerequisite_task.task_id]
        }),
    );
    let manager_wait = model.next_for_child();
    assert!(request_has_tool(
        &manager_wait.request,
        "wait_for_project_children"
    ));

    let project_id = ProjectId::from_uuid(*root.as_uuid());
    let persistence = backend.persistence.as_ref().unwrap();
    let dependent_task = persistence
        .list_project_tasks(project_id)
        .unwrap()
        .into_iter()
        .find(|task| task.requester_session_id == manager_session.session_id)
        .expect("manager delegation should create its dependent child");
    assert!(prerequisite_task.created_at < dependent_task.created_at);
    assert_eq!(
        dependent_task.status,
        loom_core::DelegatedTaskStatus::Queued
    );
    let manager_run_id = persistence
        .load_latest_run_summary_for_session(manager_session.session_id)
        .unwrap()
        .expect("manager run should have a durable checkpoint")
        .snapshot
        .id;
    model.respond_with_tool(
        manager_wait,
        "wait_for_project_children",
        serde_json::json!({ "task_ids": [dependent_task.task_id] }),
    );
    let settle_deadline = Instant::now() + Duration::from_secs(5);
    let manager_after_wait = loop {
        let response = connection.request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::GetAgentRun {
                run_id: manager_run_id,
            },
        )));
        let ServerResponse::Run(RunResponse::AgentRun(snapshot)) = response.result.unwrap() else {
            panic!("unexpected manager run response");
        };
        if !matches!(
            snapshot.state,
            AgentRunState::Planning | AgentRunState::Executing | AgentRunState::Evaluating
        ) {
            break snapshot;
        }
        assert!(
            Instant::now() < settle_deadline,
            "manager did not park; wait={:?}, execution={:?}, messages={:?}",
            persistence
                .list_project_manager_waits_by_child(dependent_task.task_id)
                .unwrap(),
            persistence
                .load_run_execution_state(manager_run_id)
                .unwrap(),
            persistence
                .load_run_messages(manager_run_id)
                .unwrap()
                .into_iter()
                .map(|message| (
                    message.role,
                    message.name,
                    message.tool_call_id,
                    message.content
                ))
                .collect::<Vec<_>>()
        );
        thread::sleep(Duration::from_millis(1));
    };
    assert_eq!(manager_after_wait.state, AgentRunState::Paused);

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let manager_status = persistence
            .load_delegated_task(manager_task.task_id)
            .unwrap()
            .unwrap()
            .status;
        let prerequisite_status = persistence
            .load_delegated_task(prerequisite_task.task_id)
            .unwrap()
            .unwrap()
            .status;
        let dependent_status = persistence
            .load_delegated_task(dependent_task.task_id)
            .unwrap()
            .unwrap()
            .status;
        if manager_status == loom_core::DelegatedTaskStatus::Blocked
            && prerequisite_status == loom_core::DelegatedTaskStatus::Running
            && dependent_status == loom_core::DelegatedTaskStatus::Queued
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "manager should park with only its prerequisite admitted; manager={manager_status:?}, prerequisite={prerequisite_status:?}, dependent={dependent_status:?}"
        );
        thread::sleep(Duration::from_millis(1));
    }
    let wait = persistence
        .list_project_manager_waits_by_child(dependent_task.task_id)
        .unwrap()
        .into_iter()
        .find(|wait| wait.manager_session_id == manager_session.session_id)
        .expect("manager wait should be durable");
    assert_eq!(wait.status, loom_core::ProjectManagerWaitStatus::Waiting);

    let prerequisite_turn = model.next_for_child();
    assert!(!request_has_tool(
        &prerequisite_turn.request,
        "wait_for_project_children"
    ));
    let prerequisite_stream = if prerequisite_fails {
        model.respond_with_failure(prerequisite_turn, 500, "scripted prerequisite failure");
        None
    } else {
        let stream = hold_scripted_model_stream_until_cancelled(prerequisite_turn);
        let cancel = connection.request(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::ControlProjectChild {
                project_id,
                manager_session_id: root,
                task_id: prerequisite_task.task_id,
                action: loom_protocol::ProjectChildControlAction::Cancel,
            },
        )));
        let ServerResponse::Project(ProjectResponse::ProjectChildControlled {
            task: cancelled_prerequisite,
            run: cancelled_run,
        }) = cancel.result.unwrap()
        else {
            panic!("unexpected prerequisite cancellation response");
        };
        assert_eq!(
            cancelled_prerequisite.status,
            loom_core::DelegatedTaskStatus::Cancelled
        );
        assert!(matches!(cancelled_run, Some(run)
            if run.state == AgentRunState::Cancelled));
        Some(stream)
    };

    let prerequisite_status = if prerequisite_fails {
        loom_core::DelegatedTaskStatus::Failed
    } else {
        loom_core::DelegatedTaskStatus::Cancelled
    };
    let prerequisite_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = persistence
            .load_delegated_task(prerequisite_task.task_id)
            .unwrap()
            .unwrap()
            .status;
        if status == prerequisite_status {
            break;
        }
        assert!(
            Instant::now() < prerequisite_deadline,
            "prerequisite should become {prerequisite_status:?}, got {status:?}"
        );
        thread::sleep(Duration::from_millis(1));
    }
    if let Some(stream) = prerequisite_stream {
        stream.join().unwrap();
    }

    let resumed_manager_turn = model.next_for_child();
    let wait_call_id = wait.tool_call_id.to_string();
    let wait_result_messages = resumed_manager_turn.request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| {
            message["role"] == "tool"
                && message["tool_call_id"] == wait_call_id
                && message["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("\"return_ready\":true"))
        })
        .collect::<Vec<_>>();
    assert_eq!(wait_result_messages.len(), 1);
    let wait_result: serde_json::Value =
        serde_json::from_str(wait_result_messages[0]["content"].as_str().unwrap()).unwrap();
    assert_eq!(wait_result["return_ready"], true);
    assert_eq!(
        wait_result["children"][0]["task_id"],
        dependent_task.task_id.to_string()
    );
    assert_eq!(wait_result["children"][0]["status"], "blocked");
    assert_eq!(
        persistence
            .load_delegated_task(dependent_task.task_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Blocked
    );
    assert_eq!(
        persistence
            .load_project_manager_wait(wait.wait_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::ProjectManagerWaitStatus::Resuming
    );

    // The blocked child is return-ready, but remains nonterminal for the
    // manager's completion guard. Hold its next provider turn open and
    // cancel the manager for cleanup after verifying the one replay.
    let manager_turn_stream = hold_scripted_model_stream_until_cancelled(resumed_manager_turn);
    let cancel_manager = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::ControlProjectChild {
            project_id,
            manager_session_id: root,
            task_id: manager_task.task_id,
            action: loom_protocol::ProjectChildControlAction::Cancel,
        },
    )));
    let ServerResponse::Project(ProjectResponse::ProjectChildControlled {
        task: cancelled_manager,
        run: cancelled_manager_run,
    }) = cancel_manager.result.unwrap()
    else {
        panic!("unexpected manager cancellation response");
    };
    assert_eq!(
        cancelled_manager.status,
        loom_core::DelegatedTaskStatus::Cancelled
    );
    assert!(matches!(cancelled_manager_run, Some(run)
        if run.id == manager_run_id && run.state == AgentRunState::Cancelled));
    manager_turn_stream.join().unwrap();
    assert_eq!(
        persistence
            .load_project_manager_wait(wait.wait_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::ProjectManagerWaitStatus::Abandoned
    );
    let durable_wait_results = persistence
        .load_run_messages(manager_run_id)
        .unwrap()
        .into_iter()
        .filter(|message| {
            message.role == loom_model::MessageRole::Tool
                && message.name.as_deref() == Some("wait_for_project_children")
                && message.tool_call_id == Some(wait.tool_call_id)
        })
        .count();
    assert_eq!(durable_wait_results, 1);
    assert!(
        persistence
            .load_latest_run_summary_for_session(dependent_task.target_session_id)
            .unwrap()
            .is_none()
    );

    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    drop(model);
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn project_coordination_exchange_and_transcripts_survive_restart() {
    let temp = std::env::temp_dir().join(format!(
        "loom-project-coordination-e2e-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&temp).unwrap();
    let persistence_path = temp.join("state.sqlite");
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_endpoint = model.endpoint.clone();
    let model_id = ModelId::new("fixture/project-coordination");
    let provider_registry =
        || scripted_project_provider_registry(&model_endpoint, model_id.clone());
    let backend =
        InProcessBackend::with_provider_registry_persistent(provider_registry(), &persistence_path)
            .unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Project coordination e2e".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let root = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Manager".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };
    let started = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRun {
            session_id: root,
            task: "Coordinate a short investigation with a child agent.".to_owned(),
            model: model_id.clone(),
            system_instructions: None,
            repository_instructions: None,
        },
    )));
    let root_run_id = match started.result.unwrap() {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected run response: {response:?}"),
    };

    // The model is driven by this test. Hold the manager's first follow-up
    // request while the child reports, so the next manager turn is forced
    // to cross an inbox boundary after both messages are durable.
    let manager_delegate = model.next_for_manager();
    assert!(request_has_tool(
        &manager_delegate.request,
        "delegate_project_task"
    ));
    model.respond_with_tool(
        manager_delegate,
        "delegate_project_task",
        serde_json::json!({
            "child_name": "investigator",
            "intent": "Investigate the question and report an initial finding and any uncertainty.",
            "model_id": model_id,
            "context_references": [],
            "dependencies": []
        }),
    );

    let child_progress_turn = model.next_for_child();
    assert!(request_has_tool(
        &child_progress_turn.request,
        "send_project_agent_message"
    ));
    let task = backend
        .persistence
        .as_ref()
        .unwrap()
        .list_project_tasks(ProjectId::from_uuid(*root.as_uuid()))
        .unwrap()
        .into_iter()
        .next()
        .expect("delegated task was committed before child scheduling");
    model.respond_with_tool(
        child_progress_turn,
        "send_project_agent_message",
        serde_json::json!({
            "target_session_id": root,
            "task_id": task.task_id,
            "kind": "progress",
            "body": "I have started the investigation and am checking the key assumption."
        }),
    );

    let child_question_turn = model.next_for_child();
    model.respond_with_tool(
        child_question_turn,
        "send_project_agent_message",
        serde_json::json!({
            "target_session_id": root,
            "task_id": task.task_id,
            "kind": "question",
            "body": "Should I prioritize the launch timeline or the reliability tradeoff?"
        }),
    );

    // A request after the question tool call proves the child message has
    // been accepted. Keep that child turn open until the manager's answer
    // and direction are recorded; it will need another turn to consume them.
    let child_waiting_for_manager = model.next_for_child();
    assert!(!request_has_project_message(
        &child_waiting_for_manager.request,
        "progress"
    ));
    assert!(!request_has_project_message(
        &child_waiting_for_manager.request,
        "question"
    ));

    let manager_poll = model.next_for_manager();
    assert!(!request_has_project_message(
        &manager_poll.request,
        "question"
    ));
    model.respond_with_tool(manager_poll, "list_project_children", serde_json::json!({}));

    let manager_with_child_messages = model.next_for_manager();
    assert!(request_has_project_message(
        &manager_with_child_messages.request,
        "progress"
    ));
    assert!(request_has_project_message(
        &manager_with_child_messages.request,
        "question"
    ));
    model.respond_with_tool(
        manager_with_child_messages,
        "send_project_agent_message",
        serde_json::json!({
            "target_session_id": task.target_session_id,
            "task_id": task.task_id,
            "kind": "answer",
            "body": "Prioritize reliability first; include the launch timeline as a secondary consideration."
        }),
    );

    let manager_direction_turn = model.next_for_manager();
    model.respond_with_tool(
        manager_direction_turn,
        "send_project_agent_message",
        serde_json::json!({
            "target_session_id": task.target_session_id,
            "task_id": task.task_id,
            "kind": "direction",
            "body": "Redirect the investigation toward the reliability tradeoff and state one practical next step."
        }),
    );

    // Keep the manager's next call open until the child has consumed both
    // messages and sent its result. The list call below advances the
    // manager to a boundary where the durable result can be delivered.
    let manager_waiting_for_result = model.next_for_manager();
    assert!(!request_has_project_message(
        &manager_waiting_for_result.request,
        "result"
    ));

    model.respond_with_tool(
        child_waiting_for_manager,
        "list_files",
        serde_json::json!({ "path": "." }),
    );
    let child_with_manager_messages = model.next_for_child();
    assert!(request_has_project_message(
        &child_with_manager_messages.request,
        "answer"
    ));
    assert!(request_has_project_message(
        &child_with_manager_messages.request,
        "direction"
    ));
    model.respond_with_tool(
        child_with_manager_messages,
        "send_project_agent_message",
        serde_json::json!({
            "target_session_id": root,
            "task_id": task.task_id,
            "kind": "result",
            "body": "Reliability is the priority; the next step is to validate the failure-recovery path before setting the launch date."
        }),
    );
    let child_final_turn = model.next_for_child();
    model.respond_with_text(
        child_final_turn,
        "Investigation complete. Reliability should be validated before setting the launch date.",
    );

    model.respond_with_tool(
        manager_waiting_for_result,
        "list_project_children",
        serde_json::json!({}),
    );
    let manager_with_result = model.next_for_manager();
    assert!(request_has_project_message(
        &manager_with_result.request,
        "result"
    ));
    model.respond_with_text(
        manager_with_result,
        "The child completed the investigation: validate reliability recovery first, then set the launch date.",
    );

    assert_eq!(
        await_settled_run(&connection, root_run_id).state,
        AgentRunState::Completed
    );
    let child = backend
        .persistence
        .as_ref()
        .unwrap()
        .load_project_snapshot(ProjectId::from_uuid(*root.as_uuid()))
        .unwrap()
        .unwrap()
        .agents
        .into_iter()
        .find(|agent| agent.session_id != root)
        .expect("child agent was created");
    let child_run_id = backend
        .persistence
        .as_ref()
        .unwrap()
        .load_latest_run_summary_for_session(child.session_id)
        .unwrap()
        .unwrap()
        .snapshot
        .id;
    assert_eq!(
        await_settled_run(&connection, child_run_id).state,
        AgentRunState::Completed
    );

    let project_id = ProjectId::from_uuid(*root.as_uuid());
    let durable = backend.persistence.as_ref().unwrap();
    let task = durable.load_delegated_task(task.task_id).unwrap().unwrap();
    assert_eq!(task.status, loom_core::DelegatedTaskStatus::Completed);
    let parent_inbox = durable
        .list_agent_messages(project_id, root, 0, 10)
        .unwrap();
    let child_inbox = durable
        .list_agent_messages(project_id, child.session_id, 0, 10)
        .unwrap();
    assert_eq!(
        parent_inbox
            .iter()
            .map(|message| message.kind)
            .collect::<Vec<_>>(),
        vec![
            loom_core::AgentMessageKind::Progress,
            loom_core::AgentMessageKind::Question,
            loom_core::AgentMessageKind::Result,
        ]
    );
    assert_eq!(
        child_inbox
            .iter()
            .map(|message| message.kind)
            .collect::<Vec<_>>(),
        vec![
            loom_core::AgentMessageKind::Answer,
            loom_core::AgentMessageKind::Direction,
        ]
    );
    assert!(parent_inbox.iter().all(|message| {
        message.project_id == project_id
            && message.task_id == Some(task.task_id)
            && message.sender_session_id == child.session_id
            && message.target_session_id == root
    }));
    assert!(child_inbox.iter().all(|message| {
        message.project_id == project_id
            && message.task_id == Some(task.task_id)
            && message.sender_session_id == root
            && message.target_session_id == child.session_id
    }));
    let child_cursor = durable
        .load_run_execution_state(child_run_id)
        .unwrap()
        .unwrap()
        .last_project_message_sequence;
    let manager_cursor = durable
        .load_run_execution_state(root_run_id)
        .unwrap()
        .unwrap()
        .last_project_message_sequence;
    assert_eq!(child_cursor, child_inbox.last().unwrap().project_sequence);
    assert_eq!(
        manager_cursor,
        parent_inbox.last().unwrap().project_sequence
    );
    let child_transcript = durable.load_run_messages(child_run_id).unwrap();
    assert!(child_transcript.iter().any(|message| {
        message.name.as_deref() == Some("loom_project_message")
            && message.content.contains("Prioritize reliability first")
    }));
    assert!(child_transcript.iter().any(|message| {
        message.name.as_deref() == Some("loom_project_message")
            && message.content.contains("Redirect the investigation")
    }));
    let manager_transcript = durable.load_run_messages(root_run_id).unwrap();
    assert!(manager_transcript.iter().any(|message| {
        message.name.as_deref() == Some("loom_project_message")
            && message.content.contains("checking the key assumption")
    }));
    assert!(manager_transcript.iter().any(|message| {
        message.name.as_deref() == Some("loom_project_message")
            && message
                .content
                .contains("Should I prioritize the launch timeline")
    }));
    assert!(manager_transcript.iter().any(|message| {
        message.name.as_deref() == Some("loom_project_message")
            && message.content.contains("failure-recovery path")
    }));
    let root_runtime_config = durable
        .load_run_runtime_config(root_run_id)
        .unwrap()
        .unwrap();
    assert!(root_runtime_config.project_delegation_enabled);
    assert!(root_runtime_config.project_messaging_enabled);
    assert!(root_runtime_config.project_inspection_enabled);
    assert!(root_runtime_config.project_child_control_enabled);
    let child_runtime_config = durable
        .load_run_runtime_config(child_run_id)
        .unwrap()
        .unwrap();
    assert!(child_runtime_config.project_messaging_enabled);
    assert!(!child_runtime_config.project_child_control_enabled);

    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);

    let reopened =
        InProcessBackend::with_provider_registry_persistent(provider_registry(), &persistence_path)
            .unwrap();
    let reopened_connection = reopened.connect();
    negotiate(&reopened_connection);
    let project = reopened_connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::GetProjectSnapshot { project_id },
    )));
    let ServerResponse::Project(ProjectResponse::ProjectSnapshot(project)) =
        project.result.unwrap()
    else {
        panic!("unexpected recovered project response");
    };
    assert_eq!(project.agents.len(), 2);
    assert!(
        project
            .agents
            .iter()
            .all(|agent| agent.state == AgentSessionState::Completed)
    );
    assert!(project.agents.iter().any(|agent| {
        agent.session_id == child.session_id
            && agent.parent_session_id == Some(root)
            && agent.depth == 2
    }));
    assert_eq!(
        reopened
            .persistence
            .as_ref()
            .unwrap()
            .load_delegated_task(task.task_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Completed
    );
    let recovered_parent_messages = reopened
        .persistence
        .as_ref()
        .unwrap()
        .list_agent_messages(project_id, root, 0, 10)
        .unwrap();
    let recovered_child_messages = reopened
        .persistence
        .as_ref()
        .unwrap()
        .list_agent_messages(project_id, child.session_id, 0, 10)
        .unwrap();
    assert_eq!(recovered_parent_messages, parent_inbox);
    assert_eq!(recovered_child_messages, child_inbox);
    assert_eq!(
        reopened
            .persistence
            .as_ref()
            .unwrap()
            .load_run_execution_state(child_run_id)
            .unwrap()
            .unwrap()
            .last_project_message_sequence,
        child_cursor
    );
    assert_eq!(
        reopened
            .persistence
            .as_ref()
            .unwrap()
            .load_run_execution_state(root_run_id)
            .unwrap()
            .unwrap()
            .last_project_message_sequence,
        manager_cursor
    );
    assert_eq!(
        reopened
            .persistence
            .as_ref()
            .unwrap()
            .load_run_messages(child_run_id)
            .unwrap(),
        child_transcript
    );
    assert_eq!(
        reopened
            .persistence
            .as_ref()
            .unwrap()
            .load_run_messages(root_run_id)
            .unwrap(),
        manager_transcript
    );
    let recovered_root_runtime_config = reopened
        .persistence
        .as_ref()
        .unwrap()
        .load_run_runtime_config(root_run_id)
        .unwrap()
        .unwrap();
    assert!(recovered_root_runtime_config.project_delegation_enabled);
    assert!(recovered_root_runtime_config.project_messaging_enabled);
    assert!(recovered_root_runtime_config.project_inspection_enabled);
    assert!(recovered_root_runtime_config.project_child_control_enabled);
    let recovered_child_runtime_config = reopened
        .persistence
        .as_ref()
        .unwrap()
        .load_run_runtime_config(child_run_id)
        .unwrap()
        .unwrap();
    assert!(recovered_child_runtime_config.project_messaging_enabled);
    assert!(!recovered_child_runtime_config.project_child_control_enabled);
    let child_detail = reopened_connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunSnapshot {
            run_id: child_run_id,
        },
    )));
    let ServerResponse::Run(RunResponse::AgentRunSnapshot(child_detail)) =
        child_detail.result.unwrap()
    else {
        panic!("unexpected recovered child run response");
    };
    assert_eq!(child_detail.run.state, AgentRunState::Completed);
    assert!(child_detail.messages.iter().any(|message| {
        message.name.as_deref() == Some("loom_project_message")
            && message.content.contains("Prioritize reliability first")
    }));
    assert!(child_detail.messages.iter().any(|message| {
        message.name.as_deref() == Some("loom_project_message")
            && message.content.contains("Redirect the investigation")
    }));
    let root_detail = reopened_connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunSnapshot {
            run_id: root_run_id,
        },
    )));
    let ServerResponse::Run(RunResponse::AgentRunSnapshot(root_detail)) =
        root_detail.result.unwrap()
    else {
        panic!("unexpected recovered manager run response");
    };
    assert_eq!(root_detail.run.state, AgentRunState::Completed);
    assert!(root_detail.messages.iter().any(|message| {
        message.name.as_deref() == Some("loom_project_message")
            && message.content.contains("checking the key assumption")
    }));

    drop(reopened_connection);
    reopened.shutdown().unwrap();
    drop(reopened);
    drop(model);
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn cancelling_project_child_cascades_deepest_first_and_survives_restart() {
    fn hold_model_stream_until_cancelled(request: ScriptedModelRequest) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let mut stream = request.stream;
            if stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .is_err()
            {
                return;
            }
            let _ = stream.flush();
            for _ in 0..3_000 {
                if stream
                    .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"holding\"}}]}\n\n")
                    .is_err()
                    || stream.flush().is_err()
                {
                    return;
                }
                thread::sleep(Duration::from_millis(20));
            }
        })
    }

    let temp = std::env::temp_dir().join(format!(
        "loom-project-cancel-cascade-e2e-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&temp).unwrap();
    let persistence_path = temp.join("state.sqlite");
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_endpoint = model.endpoint.clone();
    let model_id = ModelId::new("fixture/project-cancel-cascade");
    let provider_registry =
        || scripted_project_provider_registry(&model_endpoint, model_id.clone());
    let backend =
        InProcessBackend::with_provider_registry_persistent(provider_registry(), &persistence_path)
            .unwrap();

    let connection = backend.connect();
    negotiate(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Project cancellation cascade e2e".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let root = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Project root".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };
    assert!(matches!(
        connection
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::SetWorkspaceConfigForWorkspace {
                    workspace_id: workspace.id,
                    config: WorkspaceConfig {
                        project_agent_concurrency: 2,
                        ..WorkspaceConfig::default()
                    },
                }
            ),))
            .result,
        Ok(ServerResponse::Workspace(
            WorkspaceResponse::WorkspaceConfigUpdated
        ))
    ));

    let (manager_task, manager_session) = match connection
        .create_project_child(
            RequestId::new(),
            root,
            "manager".to_owned(),
            loom_core::DelegatedTaskSpec {
                intent: "Delegate one bounded investigation, then wait for direction.".to_owned(),
                model_id: model_id.as_str().to_owned(),
                context_references: vec![],
                dependencies: vec![],
                code_change: false,
                permissions: loom_core::ProjectAgentPermissions {
                    delegation: true,
                    ..loom_core::ProjectAgentPermissions::default()
                },
            },
        )
        .unwrap()
    {
        ServerResponse::Project(ProjectResponse::ProjectChildCreated { task, child }) => {
            (task, child)
        }
        response => panic!("unexpected manager creation response: {response:?}"),
    };
    assert_eq!(manager_task.requester_session_id, root);
    assert_eq!(manager_session.depth, 2);

    let manager_delegate = model.next_for_child();
    assert!(request_has_tool(
        &manager_delegate.request,
        "delegate_project_task"
    ));
    let sibling_task = match connection
        .create_project_child(
            RequestId::new(),
            root,
            "sibling".to_owned(),
            loom_core::DelegatedTaskSpec {
                intent: "Hold one independent task while the manager delegates.".to_owned(),
                model_id: model_id.as_str().to_owned(),
                context_references: vec![],
                dependencies: vec![],
                code_change: false,
                permissions: loom_core::ProjectAgentPermissions::default(),
            },
        )
        .unwrap()
    {
        ServerResponse::Project(ProjectResponse::ProjectChildCreated { task, .. }) => task,
        response => panic!("unexpected sibling task response: {response:?}"),
    };
    assert_eq!(sibling_task.status, loom_core::DelegatedTaskStatus::Running);
    let sibling_turn = model.next_for_child();
    assert!(!request_has_tool(
        &sibling_turn.request,
        "delegate_project_task"
    ));
    let sibling_stream = hold_model_stream_until_cancelled(sibling_turn);

    model.respond_with_tool(
        manager_delegate,
        "delegate_project_task",
        serde_json::json!({
            "child_name": "grandchild",
            "intent": "Run a short investigation and report the finding.",
            "model_id": model_id,
            "permissions": {}
        }),
    );

    let project_id = ProjectId::from_uuid(*root.as_uuid());
    let persistence = backend.persistence.as_ref().unwrap();
    // Both workspace slots are occupied, so the nested child remains
    // queued while the manager is still active.
    let manager_turn = model.next_for_child();
    assert!(request_has_tool(
        &manager_turn.request,
        "delegate_project_task"
    ));
    let manager_stream = hold_model_stream_until_cancelled(manager_turn);
    let grandchild_task = persistence
        .list_project_tasks(project_id)
        .unwrap()
        .into_iter()
        .find(|task| task.requester_session_id == manager_session.session_id)
        .expect("manager delegation should create the grandchild task");
    let grandchild_agent = persistence
        .load_project_snapshot(project_id)
        .unwrap()
        .unwrap()
        .agents
        .into_iter()
        .find(|agent| agent.session_id == grandchild_task.target_session_id)
        .expect("grandchild should belong to the project hierarchy");
    assert_eq!(
        grandchild_agent.parent_session_id,
        Some(manager_session.session_id)
    );
    assert_eq!(grandchild_agent.depth, 3);
    let manager_run_id = persistence
        .load_latest_run_summary_for_session(manager_session.session_id)
        .unwrap()
        .expect("manager run should be durable")
        .snapshot
        .id;
    assert_eq!(
        grandchild_task.requester_session_id,
        manager_session.session_id
    );
    assert_eq!(
        grandchild_task.status,
        loom_core::DelegatedTaskStatus::Queued
    );
    assert!(
        persistence
            .load_latest_run_summary_for_session(grandchild_task.target_session_id)
            .unwrap()
            .is_none()
    );

    let sequence_before_cancel = backend.journal().unwrap().latest_sequence(None);
    backend
        .project_cancellation_failpoint
        .store(usize::MAX, Ordering::SeqCst);
    let cancel_response = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::ControlProjectChild {
            project_id,
            manager_session_id: root,
            task_id: manager_task.task_id,
            action: ProjectChildControlAction::Cancel,
        },
    )));
    assert_eq!(
        cancel_response.result.unwrap_err().code,
        ErrorCode::RecoveryRequired
    );
    assert_eq!(
        persistence
            .list_pending_project_cancellation_cascades()
            .unwrap()
            .len(),
        1
    );

    let updates = backend
        .journal()
        .unwrap()
        .events_since(None, sequence_before_cancel)
        .into_iter()
        .filter_map(|event| match event.event {
            ServerEvent::ProjectTaskUpdated { task }
                if task.status == loom_core::DelegatedTaskStatus::Cancelled
                    && (task.task_id == manager_task.task_id
                        || task.task_id == grandchild_task.task_id) =>
            {
                Some(task.task_id)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(updates.is_empty());
    assert_eq!(
        persistence
            .load_delegated_task(grandchild_task.task_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Queued
    );

    drop(connection);
    backend.shutdown().unwrap();
    manager_stream.join().unwrap();
    sibling_stream.join().unwrap();
    drop(backend);
    drop(model);

    let reopened =
        InProcessBackend::with_provider_registry_persistent(provider_registry(), &persistence_path)
            .unwrap();
    let reopened_persistence = reopened.persistence.as_ref().unwrap();
    assert!(
        reopened_persistence
            .list_pending_project_cancellation_cascades()
            .unwrap()
            .is_empty(),
        "startup must finish the persisted cancellation before returning"
    );
    let recovered_cancel_updates = reopened
        .journal()
        .unwrap()
        .events_since(None, sequence_before_cancel)
        .into_iter()
        .filter_map(|event| match event.event {
            ServerEvent::ProjectTaskUpdated { task }
                if task.status == loom_core::DelegatedTaskStatus::Cancelled
                    && (task.task_id == manager_task.task_id
                        || task.task_id == grandchild_task.task_id) =>
            {
                Some(task.task_id)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        recovered_cancel_updates,
        vec![grandchild_task.task_id, manager_task.task_id],
        "startup should replay the captured cascade in deepest-first order"
    );
    let recovered = reopened_persistence
        .load_project_snapshot(project_id)
        .unwrap()
        .unwrap();
    assert_eq!(recovered.agents.len(), 4);
    assert_eq!(recovered.tasks.len(), 3);
    for task_id in [manager_task.task_id, grandchild_task.task_id] {
        assert_eq!(
            reopened_persistence
                .load_delegated_task(task_id)
                .unwrap()
                .unwrap()
                .status,
            loom_core::DelegatedTaskStatus::Cancelled
        );
    }
    let recovered_manager_run = reopened_persistence
        .load_latest_run_summary_for_session(manager_session.session_id)
        .unwrap()
        .unwrap()
        .snapshot;
    assert_eq!(recovered_manager_run.id, manager_run_id);
    assert_eq!(recovered_manager_run.state, AgentRunState::Cancelled);
    assert!(
        reopened_persistence
            .load_latest_run_summary_for_session(grandchild_task.target_session_id)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        reopened_persistence
            .load_delegated_task(sibling_task.task_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Blocked
    );
    drop(reopened);
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn delegated_children_require_durable_storage_on_ephemeral_backends() {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    let discovered = connection
        .request(RequestEnvelope::new(ClientRequest::Control(
            ControlRequest::DiscoverCapabilities,
        )))
        .result
        .unwrap();
    assert!(matches!(
        discovered,
        ServerResponse::Control(ControlResponse::Capabilities(result))
            if !result.capabilities.contains(Capability::CreateProjectChild)
                && !result.capabilities.contains(Capability::SendProjectAgentMessage)
    ));
    negotiate(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Ephemeral project".into(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let root = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Manager".into(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };
    assert!(
        !connection
            .project_delegation_enabled_for_session(root)
            .unwrap(),
        "delegation tools should be unavailable without durable storage"
    );
    fs::remove_dir_all(&backend.session_root_base).unwrap();
}

/// Serves an event stream that keeps a completion open until the client
/// gives up, so a run can be controlled while the model is still working.
fn slow_model_endpoint() -> (String, std::sync::mpsc::Receiver<()>) {
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    thread::spawn(move || {
        use std::io::{Read, Write};

        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut request = [0_u8; 8192];
        let _ = stream.read(&mut request);
        let _ = stream.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        );
        let _ =
            stream.write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"thinking\"}}]}\n\n");
        let _ = stream.flush();
        let _ = sender.send(());
        // Keep the completion open; the run must be stoppable anyway.
        for _ in 0..600 {
            if stream
                .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\".\"}}]}\n\n")
                .is_err()
            {
                return;
            }
            let _ = stream.flush();
            thread::sleep(Duration::from_millis(20));
        }
    });
    (format!("http://{address}/v1/chat/completions"), receiver)
}

fn gated_model_endpoint(
    first_content: &str,
) -> (
    String,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Sender<()>,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (first_sender, first_receiver) = std::sync::mpsc::channel();
    let (second_sender, second_receiver) = std::sync::mpsc::channel();
    let (second_gate_sender, second_gate_receiver) = std::sync::mpsc::channel();
    let (finish_gate_sender, finish_gate_receiver) = std::sync::mpsc::channel();
    let first_event = format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}}}}]}}\n\n",
        serde_json::to_string(first_content).unwrap()
    );
    thread::spawn(move || {
        use std::io::{Read, Write};

        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut request = [0_u8; 8192];
        let _ = stream.read(&mut request);
        if stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .is_err()
            || stream.write_all(first_event.as_bytes()).is_err()
            || stream.flush().is_err()
        {
            return;
        }
        let _ = first_sender.send(());
        if second_gate_receiver.recv().is_err() {
            return;
        }
        if stream
            .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"second\"}}]}\n\n")
            .is_err()
            || stream.flush().is_err()
        {
            return;
        }
        let _ = second_sender.send(());
        if finish_gate_receiver.recv().is_err() {
            return;
        }
        let _ = stream.write_all(b"data: [DONE]\n\n");
        let _ = stream.flush();
    });
    (
        format!("http://{address}/v1/chat/completions"),
        first_receiver,
        second_gate_sender,
        second_receiver,
        finish_gate_sender,
    )
}

struct ScriptedModelRequest {
    request: serde_json::Value,
    stream: TcpStream,
}

fn hold_scripted_model_stream_until_cancelled(
    request: ScriptedModelRequest,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut stream = request.stream;
        if stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .is_err()
        {
            return;
        }
        let _ = stream.flush();
        for _ in 0..3_000 {
            if stream
                .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"holding\"}}]}\n\n")
                .is_err()
                || stream.flush().is_err()
            {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
    })
}

struct ScriptedOpenAiEndpoint {
    endpoint: String,
    address: std::net::SocketAddr,
    requests: std::sync::mpsc::Receiver<ScriptedModelRequest>,
    pending: VecDeque<ScriptedModelRequest>,
    stopped: Arc<AtomicBool>,
    accept_worker: Option<thread::JoinHandle<()>>,
}

impl ScriptedOpenAiEndpoint {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (sender, requests) = std::sync::mpsc::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stopped = Arc::clone(&stopped);
        let accept_worker = thread::spawn(move || {
            while !worker_stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let sender = sender.clone();
                        thread::spawn(move || {
                            let Ok(body) = read_scripted_http_request_body(&mut stream) else {
                                return;
                            };
                            let Ok(request) = serde_json::from_slice(&body) else {
                                return;
                            };
                            let _ = sender.send(ScriptedModelRequest { request, stream });
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            endpoint: format!("http://{address}/v1/chat/completions"),
            address,
            requests,
            pending: VecDeque::new(),
            stopped,
            accept_worker: Some(accept_worker),
        }
    }

    fn next_for_manager(&mut self) -> ScriptedModelRequest {
        self.next_for_role(true)
    }

    fn next_for_child(&mut self) -> ScriptedModelRequest {
        self.next_for_role(false)
    }

    fn next_for_role(&mut self, manager: bool) -> ScriptedModelRequest {
        if let Some(index) = self
            .pending
            .iter()
            .position(|request| scripted_request_is_manager(&request.request) == manager)
        {
            return self.pending.remove(index).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for scripted model request"
            );
            let request = self
                .requests
                .recv_timeout(remaining)
                .expect("scripted model request did not arrive");
            if scripted_request_is_manager(&request.request) == manager {
                return request;
            }
            self.pending.push_back(request);
        }
    }

    fn respond_with_tool(
        &self,
        request: ScriptedModelRequest,
        name: &str,
        arguments: serde_json::Value,
    ) {
        let response = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "id": "fixture-call",
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": serde_json::to_string(&arguments).unwrap()
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        write_scripted_http_response(request.stream, response);
    }

    fn respond_with_text(&self, request: ScriptedModelRequest, content: &str) {
        let response = serde_json::json!({
            "choices": [{
                "message": {"role": "assistant", "content": content},
                "finish_reason": "stop"
            }]
        });
        write_scripted_http_response(request.stream, response);
    }

    fn respond_with_failure(&self, request: ScriptedModelRequest, status: u16, message: &str) {
        let body = serde_json::json!({"error": {"message": message}}).to_string();
        let reason = match status {
            500 => "Internal Server Error",
            503 => "Service Unavailable",
            _ => "Scripted Failure",
        };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let mut stream = request.stream;
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    }
}

impl Drop for ScriptedOpenAiEndpoint {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
        if let Some(worker) = self.accept_worker.take() {
            let _ = worker.join();
        }
    }
}

fn read_scripted_http_request_body(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut headers = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        stream.read_exact(&mut byte)?;
        headers.push(byte[0]);
        if headers.ends_with(b"\r\n\r\n") {
            break;
        }
        if headers.len() > 64 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "scripted model request headers were too large",
            ));
        }
    }
    let headers = String::from_utf8_lossy(&headers);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "scripted model request omitted content length",
            )
        })?;
    let mut body = vec![0; content_length];
    stream.read_exact(&mut body)?;
    Ok(body)
}

fn write_scripted_http_response(mut stream: TcpStream, body: serde_json::Value) {
    let body = serde_json::to_vec(&body).unwrap();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
    stream.write_all(&body).unwrap();
    stream.flush().unwrap();
}

fn scripted_project_provider_registry(endpoint: &str, model_id: ModelId) -> ProviderRegistry {
    let provider_id = ProviderId::new("scripted-project");
    let descriptor = ModelDescriptor {
        id: model_id,
        provider: provider_id.clone(),
        display_name: "Scripted project coordination model".to_owned(),
        context_window: Some(16_384),
        max_input_tokens: None,
        max_output_tokens: None,
        capabilities: ModelCapabilities {
            streaming: false,
            tool_calling: true,
            vision: false,
            json_mode: false,
        },
    };
    let providers = ProviderRegistry::new();
    providers
        .register(ProviderConfig::openai_compatible(
            provider_id,
            "Scripted project model",
            endpoint,
            descriptor,
            None,
        ))
        .unwrap();
    providers
}

fn scripted_request_is_manager(request: &serde_json::Value) -> bool {
    request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message["role"] == "system")
        .any(|message| {
            message["content"].as_str().is_some_and(|content| {
                content.contains("You are the project manager for this project")
            })
        })
}

fn request_has_tool(request: &serde_json::Value, name: &str) -> bool {
    request["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|tool| tool["function"]["name"] == name)
}

fn request_has_project_message(request: &serde_json::Value, kind: &str) -> bool {
    request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|message| {
            message["name"] == "loom_project_message"
                && message["content"]
                    .as_str()
                    .is_some_and(|content| content.contains(&format!("({kind}")))
        })
}

#[test]
fn streamed_message_fragments_batch_until_the_time_threshold() {
    let (endpoint, first_delta, release_second, second_delta, finish) =
        gated_model_endpoint("first");
    let persistence =
        std::env::temp_dir().join(format!("loom-server-batched-{}.db", WorkspaceId::new()));
    let backend = InProcessBackend::with_openai_compatible_persistent(
        endpoint,
        "key",
        ModelId::new("slow/model"),
        &persistence,
    )
    .unwrap();
    let session_root_base = backend.session_root_base.clone();
    let connection = backend.connect();
    negotiate_m3(&connection);
    let workspace = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "Batched transcript workspace".to_owned(),
        },
    )));
    let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
        workspace.result.unwrap()
    else {
        panic!("unexpected workspace response");
    };
    let session = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "batched transcript".to_owned(),
        },
    )));
    let session_id = match session.result.unwrap() {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    let started = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRun {
            session_id,
            task: "stream a response".to_owned(),
            model: ModelId::new("slow/model"),
            system_instructions: None,
            repository_instructions: None,
        },
    )));
    let run_id = match started.result.unwrap() {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    first_delta
        .recv_timeout(Duration::from_secs(10))
        .expect("first model delta");

    let handle = backend.runs().unwrap().get(&run_id).cloned().unwrap();
    for _ in 0..200 {
        if handle.message_fragments.lock().unwrap().pending_bytes == "first".len() {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        handle.message_fragments.lock().unwrap().pending_bytes,
        "first".len()
    );
    assert!(
        backend
            .persistence
            .as_ref()
            .unwrap()
            .load_run_messages(run_id)
            .unwrap()
            .iter()
            .all(|message| message.content != "first")
    );

    thread::sleep(MESSAGE_FRAGMENT_BATCH_INTERVAL + Duration::from_millis(2));
    let mut flushed_prefix = None;
    for _ in 0..200 {
        flushed_prefix = backend
            .persistence
            .as_ref()
            .unwrap()
            .load_run_messages(run_id)
            .unwrap()
            .into_iter()
            .find(|message| message.role == loom_model::MessageRole::Assistant)
            .map(|message| message.content);
        if flushed_prefix.as_deref() == Some("first") {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(flushed_prefix.as_deref(), Some("first"));

    release_second.send(()).unwrap();
    second_delta
        .recv_timeout(Duration::from_secs(10))
        .expect("second model delta");
    let transcript = backend.persistence.as_ref().unwrap();
    let mut persisted_content = None;
    for _ in 0..200 {
        persisted_content = transcript
            .load_run_messages(run_id)
            .unwrap()
            .into_iter()
            .find(|message| message.role == loom_model::MessageRole::Assistant)
            .map(|message| message.content);
        if persisted_content.as_deref() == Some("firstsecond") {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(persisted_content.as_deref(), Some("firstsecond"));
    assert_eq!(handle.message_fragments.lock().unwrap().pending_bytes, 0);

    finish.send(()).unwrap();
    await_settled_run(&connection, run_id);
    drop(connection);
    drop(backend);
    fs::remove_dir_all(session_root_base).unwrap();
    let _ = fs::remove_file(&persistence);
    let _ = fs::remove_file(persistence.with_extension("db-shm"));
    let _ = fs::remove_file(persistence.with_extension("db-wal"));
}

#[test]
fn streamed_message_fragments_flush_at_the_byte_threshold_without_splitting_utf8() {
    let content = format!(
        "{}é",
        "a".repeat(MESSAGE_FRAGMENT_BATCH_BYTES.saturating_sub(1))
    );
    let (endpoint, first_delta, release_second, second_delta, finish) =
        gated_model_endpoint(&content);
    let persistence =
        std::env::temp_dir().join(format!("loom-server-large-delta-{}.db", WorkspaceId::new()));
    let backend = InProcessBackend::with_openai_compatible_persistent(
        endpoint,
        "key",
        ModelId::new("slow/model"),
        &persistence,
    )
    .unwrap();
    let session_root_base = backend.session_root_base.clone();
    let connection = backend.connect();
    negotiate_m3(&connection);
    let workspace = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "Large transcript workspace".to_owned(),
        },
    )));
    let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
        workspace.result.unwrap()
    else {
        panic!("unexpected workspace response");
    };
    let session = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "large transcript".to_owned(),
        },
    )));
    let session_id = match session.result.unwrap() {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    let started = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRun {
            session_id,
            task: "stream a large response".to_owned(),
            model: ModelId::new("slow/model"),
            system_instructions: None,
            repository_instructions: None,
        },
    )));
    let run_id = match started.result.unwrap() {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    first_delta
        .recv_timeout(Duration::from_secs(10))
        .expect("large model delta");

    let transcript = backend.persistence.as_ref().unwrap();
    let mut persisted_content = None;
    for _ in 0..200 {
        persisted_content = transcript
            .load_run_messages(run_id)
            .unwrap()
            .into_iter()
            .find(|message| message.role == loom_model::MessageRole::Assistant)
            .map(|message| message.content);
        if persisted_content.as_deref() == Some(content.as_str()) {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(persisted_content.as_deref(), Some(content.as_str()));

    release_second.send(()).unwrap();
    second_delta
        .recv_timeout(Duration::from_secs(10))
        .expect("second model delta");
    finish.send(()).unwrap();
    await_settled_run(&connection, run_id);
    drop(connection);
    drop(backend);
    fs::remove_dir_all(session_root_base).unwrap();
    let _ = fs::remove_file(&persistence);
    let _ = fs::remove_file(persistence.with_extension("db-shm"));
    let _ = fs::remove_file(persistence.with_extension("db-wal"));
}

#[test]
fn a_running_model_call_can_be_interrupted_without_blocking_the_request() {
    let (endpoint, started) = slow_model_endpoint();
    let backend =
        InProcessBackend::with_openai_compatible(endpoint, "key", ModelId::new("slow/model"));
    let connection = backend.connect();
    negotiate_m3(&connection);
    let workspace = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "Interruptible workspace".to_owned(),
        },
    )));
    let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
        workspace.result.unwrap()
    else {
        panic!("unexpected workspace response");
    };
    let session = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "interruptible run".to_owned(),
        },
    )));
    let session_id = match session.result.unwrap() {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    let started_run = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRun {
            session_id,
            task: "stream for a long time".to_owned(),
            model: ModelId::new("slow/model"),
            system_instructions: None,
            repository_instructions: None,
        },
    )));
    let run_id = match started_run.result.unwrap() {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    started
        .recv_timeout(Duration::from_secs(10))
        .expect("model stream started");

    // A second connection controls the run while the first one's model call
    // is still open.
    let observer = backend.connect();
    negotiate_m3(&observer);
    // The delta is journaled while the completion is still open, so a second
    // client sees it before the run ends.
    let mut streamed = false;
    for _ in 0..1_000 {
        let events = observer.request(RequestEnvelope::new(ClientRequest::Events(
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
            panic!("unexpected events response");
        };
        if events.iter().any(|event| {
            matches!(
                &event.event,
                ServerEvent::Agent {
                    event: AgentEvent::AssistantMessageDelta { text, .. }
                } if text == "thinking"
            )
        }) {
            streamed = true;
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    assert!(
        streamed,
        "an assistant delta was not journaled mid-completion"
    );

    let before = Instant::now();
    let interrupted = observer.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::InterruptAgentRun { run_id },
    )));
    let elapsed = before.elapsed();
    let ServerResponse::Run(RunResponse::AgentRun(snapshot)) = interrupted.result.unwrap() else {
        panic!("unexpected interrupt response");
    };
    assert_eq!(snapshot.state, AgentRunState::Cancelled);
    assert!(
        elapsed < Duration::from_secs(5),
        "interrupt waited {elapsed:?} for the model call"
    );
    fs::remove_dir_all(&backend.session_root_base).unwrap();
}

#[test]
fn a_running_model_call_can_be_paused_and_resumed() {
    let (endpoint, started) = slow_model_endpoint();
    let backend =
        InProcessBackend::with_openai_compatible(endpoint, "key", ModelId::new("slow/model"));
    let connection = backend.connect();
    negotiate_m3(&connection);
    let workspace = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "Pausable workspace".to_owned(),
        },
    )));
    let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
        workspace.result.unwrap()
    else {
        panic!("unexpected workspace response");
    };
    let session = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "pausable run".to_owned(),
        },
    )));
    let session_id = match session.result.unwrap() {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    let started_run = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRun {
            session_id,
            task: "stream for a long time".to_owned(),
            model: ModelId::new("slow/model"),
            system_instructions: None,
            repository_instructions: None,
        },
    )));
    let run_id = match started_run.result.unwrap() {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    started
        .recv_timeout(Duration::from_secs(10))
        .expect("model stream started");
    let before = Instant::now();
    let paused = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::PauseAgentRun { run_id },
    )));
    let elapsed = before.elapsed();
    let ServerResponse::Run(RunResponse::AgentRun(snapshot)) = paused.result.unwrap() else {
        panic!("unexpected pause response");
    };
    assert_eq!(snapshot.state, AgentRunState::Paused);
    assert!(
        elapsed < Duration::from_secs(5),
        "pause waited {elapsed:?} for the model call"
    );
    let current = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRun { run_id },
    )));
    let ServerResponse::Run(RunResponse::AgentRun(snapshot)) = current.result.unwrap() else {
        panic!("unexpected run response");
    };
    assert_eq!(snapshot.state, AgentRunState::Paused);
    fs::remove_dir_all(&backend.session_root_base).unwrap();
}

#[test]
fn retryable_mutation_idempotency_survives_backend_restart() {
    let path =
        std::env::temp_dir().join(format!("loom-server-idempotency-{}.db", WorkspaceId::new()));
    let request_id = loom_core::RequestId::new();
    let (workspace_id, first) = {
        let backend = InProcessBackend::new_persistent(&path).unwrap();
        let connection = backend.connect();
        negotiate(&connection);
        let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Idempotency workspace".to_owned(),
            },
        )));
        let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
            created.result.unwrap()
        else {
            panic!("unexpected workspace response");
        };
        let request = RequestEnvelope::with_request_id(
            request_id,
            ClientRequest::Workspace(WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "durable idempotency".to_owned(),
            }),
        );
        let response = connection.request(request);
        backend.shutdown().unwrap();
        (workspace.id, response)
    };
    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let request = RequestEnvelope::with_request_id(
        request_id,
        ClientRequest::Workspace(WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id,
            name: "durable idempotency".to_owned(),
        }),
    );
    let second = connection.request(request);
    assert_eq!(first, second);
    assert!(matches!(
        first.result,
        Ok(ServerResponse::Session(
            SessionResponse::AgentSessionCreated(_)
        ))
    ));
    let expired_request_id = request_id_with_issued_at(
        current_unix_millis()
            .saturating_sub(IDEMPOTENCY_RETENTION.as_millis() as u64)
            .saturating_sub(1),
    );
    let expired = connection.request(RequestEnvelope::with_request_id(
        expired_request_id,
        ClientRequest::Workspace(WorkspaceRequest::CreateWorkspace {
            name: "must not be replayed".to_owned(),
        }),
    ));
    assert_eq!(
        expired.result.unwrap_err().code,
        ErrorCode::DeadlineExceeded
    );
    let workspaces = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::ListWorkspaces,
    )));
    assert!(matches!(
        workspaces.result,
        Ok(ServerResponse::Workspace(WorkspaceResponse::Workspaces{ workspaces })) if workspaces.len() == 1
    ));
    fs::remove_file(path).unwrap();
}

#[test]
fn retryable_mutation_save_failure_does_not_cache_success() {
    let path = std::env::temp_dir().join(format!(
        "loom-server-idempotency-failure-{}.db",
        WorkspaceId::new()
    ));
    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let request_id = loom_core::RequestId::new();
    let request = ClientRequest::Workspace(WorkspaceRequest::CreateWorkspace {
        name: "failure boundary".to_owned(),
    });
    backend.fail_next_state_save.store(true, Ordering::SeqCst);
    let failed = connection.request(RequestEnvelope::with_request_id(
        request_id,
        request.clone(),
    ));
    assert_eq!(failed.result.unwrap_err().code, ErrorCode::Internal);
    assert!(
        backend
            .idempotency_store
            .cached_response(request_id, &request)
            .unwrap()
            .is_none()
    );

    // Handler state can already have changed when a save fails. Fail-stop
    // prevents a retry from dispatching against that partially mutated
    // in-memory state; reopening restores the last committed disk state.
    let retried = connection.request(RequestEnvelope::with_request_id(request_id, request));
    assert_eq!(retried.result.unwrap_err().code, ErrorCode::Persistence);
    let listed = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::ListWorkspaces,
    )));
    assert_eq!(listed.result.unwrap_err().code, ErrorCode::Persistence);
    backend.shutdown().unwrap();
    drop(connection);
    drop(backend);
    let reopened = InProcessBackend::new_persistent(&path).unwrap();
    let connection = reopened.connect();
    negotiate(&connection);
    let listed = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::ListWorkspaces,
    )));
    assert!(
        matches!(listed.result, Ok(ServerResponse::Workspace(WorkspaceResponse::Workspaces{ workspaces })) if workspaces.is_empty())
    );
    drop(connection);
    drop(reopened);
    fs::remove_file(path).unwrap();
}

#[test]
fn reconnect_feed_payloads_load_lazily_and_keep_pruned_cursor_after_restart() {
    let path = std::env::temp_dir().join(format!("loom-server-feed-{}.db", WorkspaceId::new()));
    let (session_id, session_root_base, previous_stream_epoch) = {
        let backend = InProcessBackend::new_persistent(&path).unwrap();
        backend.set_event_retention(1).unwrap();
        let connection = backend.connect();
        negotiate(&connection);
        let workspace = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Lazy feed workspace".to_owned(),
            },
        )));
        let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
            workspace.result.unwrap()
        else {
            panic!("unexpected workspace response");
        };
        let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Lazy feed session".to_owned(),
            },
        )));
        let ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) =
            created.result.unwrap()
        else {
            panic!("unexpected session response");
        };
        for name in ["renamed once", "renamed twice"] {
            connection
                .request(RequestEnvelope::new(ClientRequest::Session(
                    SessionRequest::RenameAgentSession {
                        session_id: session.id,
                        name: name.to_owned(),
                    },
                )))
                .result
                .unwrap();
        }
        backend.flush().unwrap();
        let recovered = (
            session.id,
            backend.session_root_base.clone(),
            backend.node_id.clone(),
        );
        backend.shutdown().unwrap();
        recovered
    };

    let backend = InProcessBackend::new_persistent(&path).unwrap();
    assert!(backend.journal().unwrap().events.is_empty());
    let connection = backend.connect();
    negotiate(&connection);
    let stale = connection.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetSessionEvents {
            session_id: Some(session_id),
            workspace_id: None,
            after_sequence: Some(EventSequence::new(1)),
            stream_epoch: None,
        },
    )));
    let ServerResponse::Events(EventsResponse::SessionEventsSnapshot {
        events,
        oldest_sequence,
        latest_sequence,
        ..
    }) = stale.result.unwrap()
    else {
        panic!("expected a stale-cursor snapshot");
    };
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sequence, EventSequence::new(3));
    assert_eq!(oldest_sequence, EventSequence::new(3));
    assert_eq!(latest_sequence, EventSequence::new(3));

    let changed_epoch = connection.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetSessionEvents {
            session_id: Some(session_id),
            workspace_id: None,
            after_sequence: Some(EventSequence::new(3)),
            stream_epoch: Some(previous_stream_epoch.clone()),
        },
    )));
    let ServerResponse::Events(EventsResponse::SessionEventsSnapshot {
        events,
        latest_sequence,
        stream_epoch: Some(current_epoch),
        ..
    }) = changed_epoch.result.unwrap()
    else {
        panic!("expected a snapshot after the feed epoch changed");
    };
    assert_ne!(current_epoch, previous_stream_epoch);
    assert_eq!(latest_sequence, EventSequence::new(3));
    assert_eq!(events.len(), 1);

    let current = connection.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetSessionEvents {
            session_id: Some(session_id),
            workspace_id: None,
            after_sequence: Some(EventSequence::new(2)),
            stream_epoch: None,
        },
    )));
    let ServerResponse::Events(EventsResponse::SessionEvents { events, .. }) =
        current.result.unwrap()
    else {
        panic!("expected retained session events");
    };
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sequence, EventSequence::new(3));

    fs::remove_dir_all(session_root_base).unwrap();
    fs::remove_file(path).unwrap();
}

#[test]
fn workspace_reconnect_feed_isolated_and_pruned_cursors_resync_to_workspace_snapshot() {
    let path = std::env::temp_dir().join(format!(
        "loom-server-workspace-feed-{}.db",
        WorkspaceId::new()
    ));
    let (workspace_a, workspace_b, session_a, session_b, previous_epoch) = {
        let backend = InProcessBackend::new_persistent(&path).unwrap();
        backend.set_event_retention(1).unwrap();
        let connection = backend.connect();
        negotiate(&connection);
        let create_workspace = |name: &str| {
            let response = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::CreateWorkspace {
                    name: name.to_owned(),
                },
            )));
            let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
                response.result.unwrap()
            else {
                panic!("unexpected workspace response");
            };
            workspace.id
        };
        let workspace_a = create_workspace("Workspace feed A");
        let workspace_b = create_workspace("Workspace feed B");
        let create_session = |workspace_id, name: &str| {
            let response = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::CreateAgentSessionInWorkspace {
                    workspace_id,
                    name: name.to_owned(),
                },
            )));
            let ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) =
                response.result.unwrap()
            else {
                panic!("unexpected session response");
            };
            session.id
        };
        let session_a = create_session(workspace_a, "A");
        let session_b = create_session(workspace_b, "B");
        for (session_id, label) in [(session_a, "A"), (session_b, "B")] {
            for revision in 1..=2 {
                connection
                    .request(RequestEnvelope::new(ClientRequest::Session(
                        SessionRequest::RenameAgentSession {
                            session_id,
                            name: format!("{label} {revision}"),
                        },
                    )))
                    .result
                    .unwrap();
            }
        }

        for workspace_id in [workspace_a, workspace_b] {
            let response = connection.request(RequestEnvelope::new(ClientRequest::Events(
                EventsRequest::GetSessionEvents {
                    session_id: None,
                    workspace_id: Some(workspace_id),
                    after_sequence: None,
                    stream_epoch: None,
                },
            )));
            let ServerResponse::Events(EventsResponse::WorkspaceEventsSnapshot {
                workspace_id: returned_workspace,
                sessions,
                events,
                ..
            }) = response.result.unwrap()
            else {
                panic!("expected a snapshot after in-memory feed pruning");
            };
            assert_eq!(returned_workspace, workspace_id);
            assert_eq!(sessions.len(), 1);
            assert!(events.iter().all(|event| matches!(event,
                        WorkspaceFeedEvent::Session(event) if event.session_id == sessions[0].id)));
        }

        let ambiguous = connection.request(RequestEnvelope::new(ClientRequest::Events(
            EventsRequest::GetSessionEvents {
                session_id: Some(session_a),
                workspace_id: Some(workspace_a),
                after_sequence: None,
                stream_epoch: None,
            },
        )));
        assert!(ambiguous.result.is_err());
        let unknown = connection.request(RequestEnvelope::new(ClientRequest::Events(
            EventsRequest::GetSessionEvents {
                session_id: None,
                workspace_id: Some(WorkspaceId::new()),
                after_sequence: None,
                stream_epoch: None,
            },
        )));
        assert!(unknown.result.is_err());

        backend.flush().unwrap();
        let recovered = (
            workspace_a,
            workspace_b,
            session_a,
            session_b,
            backend.node_id.clone(),
        );
        backend.shutdown().unwrap();
        recovered
    };

    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let stale = connection.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetSessionEvents {
            session_id: None,
            workspace_id: Some(workspace_a),
            after_sequence: Some(EventSequence::new(1)),
            stream_epoch: None,
        },
    )));
    let ServerResponse::Events(EventsResponse::WorkspaceEventsSnapshot {
        workspace_id,
        sessions,
        events,
        oldest_sequence,
        latest_sequence,
        stream_epoch: Some(current_epoch),
    }) = stale.result.unwrap()
    else {
        panic!("expected a workspace snapshot after persisted pruning");
    };
    assert_eq!(workspace_id, workspace_a);
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, session_a);
    assert!(events.iter().all(|event| matches!(event,
        WorkspaceFeedEvent::Session(event) if event.session_id == session_a)));
    assert!(!events.iter().any(|event| matches!(event,
        WorkspaceFeedEvent::Session(event) if event.session_id == session_b)));
    assert!(oldest_sequence <= latest_sequence);
    assert_ne!(current_epoch, previous_epoch);

    let events_b = connection.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetSessionEvents {
            session_id: None,
            workspace_id: Some(workspace_b),
            after_sequence: None,
            stream_epoch: None,
        },
    )));
    let ServerResponse::Events(EventsResponse::WorkspaceEventsSnapshot {
        sessions,
        events,
        latest_sequence: global_cursor,
        stream_epoch: Some(current_epoch),
        ..
    }) = events_b.result.unwrap()
    else {
        panic!("expected a workspace snapshot after persisted pruning");
    };
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, session_b);
    assert!(events.iter().all(|event| matches!(event,
        WorkspaceFeedEvent::Session(event) if event.session_id == session_b)));

    let advanced_cursor = connection.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetSessionEvents {
            session_id: None,
            workspace_id: Some(workspace_a),
            after_sequence: Some(global_cursor),
            stream_epoch: Some(current_epoch),
        },
    )));
    assert!(matches!(
        advanced_cursor.result.unwrap(),
        ServerResponse::Events(EventsResponse::WorkspaceEvents{ events, .. }) if events.is_empty()
    ));

    fs::remove_file(path).unwrap();
}

#[test]
fn workspace_rename_and_config_changes_are_durable_workspace_events() {
    let path = std::env::temp_dir().join(format!(
        "loom-server-workspace-mutations-{}.db",
        WorkspaceId::new()
    ));
    let workspace_id = {
        let backend = InProcessBackend::new_persistent(&path).unwrap();
        let connection = backend.connect();
        negotiate(&connection);
        let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Before rename".to_owned(),
            },
        )));
        let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
            created.result.unwrap()
        else {
            panic!("unexpected workspace response");
        };
        connection
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::RenameWorkspace {
                    workspace_id: workspace.id,
                    name: "After rename".to_owned(),
                },
            )))
            .result
            .unwrap();
        let config = WorkspaceConfig {
            revision: 7,
            ..WorkspaceConfig::default()
        };
        connection
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::SetWorkspaceConfigForWorkspace {
                    workspace_id: workspace.id,
                    config,
                },
            )))
            .result
            .unwrap();
        backend.flush().unwrap();
        let workspace_id = workspace.id;
        backend.shutdown().unwrap();
        workspace_id
    };

    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let response = connection.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetSessionEvents {
            session_id: None,
            workspace_id: Some(workspace_id),
            after_sequence: Some(EventSequence::default()),
            stream_epoch: None,
        },
    )));
    let ServerResponse::Events(EventsResponse::WorkspaceEvents {
        workspace_id: returned,
        events,
        ..
    }) = response.result.unwrap()
    else {
        panic!("expected workspace event batch");
    };
    assert_eq!(returned, workspace_id);
    assert_eq!(events.len(), 2);
    assert!(
        matches!(events[0], WorkspaceFeedEvent::Workspace(WorkspaceEventEnvelope {
        event: WorkspaceEvent::Renamed { ref name }, ..
    }) if name == "After rename")
    );
    assert!(matches!(
        events[1],
        WorkspaceFeedEvent::Workspace(WorkspaceEventEnvelope {
            event: WorkspaceEvent::ConfigChanged { revision: 7 },
            ..
        })
    ));
    fs::remove_file(path).unwrap();
}

#[test]
fn workspace_event_requests_require_workspace_event_capability() {
    let path = std::env::temp_dir().join(format!(
        "loom-server-workspace-feed-capability-{}.db",
        WorkspaceId::new()
    ));
    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let writer = backend.connect();
    negotiate(&writer);
    let created = writer.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "Before".to_owned(),
        },
    )));
    let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
        created.result.unwrap()
    else {
        panic!("unexpected workspace response");
    };
    writer
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::RenameWorkspace {
                workspace_id: workspace.id,
                name: "After".to_owned(),
            },
        )))
        .result
        .unwrap();

    let limited = backend.connect();
    let limited_capabilities = CapabilitySet::new([Capability::SubscribeSessionEvents]);
    let negotiated = limited.request(RequestEnvelope::with_version(
        CURRENT_PROTOCOL_VERSION,
        ClientRequest::Control(ControlRequest::Negotiate {
            client_version: CURRENT_PROTOCOL_VERSION,
            capabilities: limited_capabilities,
        }),
    ));
    assert!(matches!(
        negotiated.result,
        Ok(ServerResponse::Control(ControlResponse::Negotiated(_)))
    ));
    let unsupported_events = limited.request(RequestEnvelope::with_version(
        CURRENT_PROTOCOL_VERSION,
        ClientRequest::Events(EventsRequest::GetSessionEvents {
            session_id: None,
            workspace_id: Some(workspace.id),
            after_sequence: Some(EventSequence::default()),
            stream_epoch: None,
        }),
    ));
    assert_eq!(
        unsupported_events.result.unwrap_err().code,
        ErrorCode::CapabilityDenied
    );

    let current = backend.connect();
    negotiate(&current);
    let current_events = current.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetSessionEvents {
            session_id: None,
            workspace_id: Some(workspace.id),
            after_sequence: Some(EventSequence::default()),
            stream_epoch: None,
        },
    )));
    assert!(matches!(current_events.result.unwrap(),
        ServerResponse::Events(EventsResponse::WorkspaceEvents{ events, .. })
            if matches!(events.as_slice(), [WorkspaceFeedEvent::Workspace(_)])));
    fs::remove_file(path).unwrap();
}

#[test]
fn transcript_page_content_is_bounded_and_marks_truncation() {
    let large = vec![b'x'; MAX_AGENT_RUN_TRANSCRIPT_MESSAGE_BYTES as usize + 1];
    let (content, truncated) = bounded_transcript_content(
        &large[..MAX_AGENT_RUN_TRANSCRIPT_MESSAGE_BYTES as usize],
        MAX_AGENT_RUN_TRANSCRIPT_MESSAGE_BYTES as u64 + 1,
    );
    assert!(truncated);
    assert!(content.starts_with(&"x".repeat(MAX_AGENT_RUN_TRANSCRIPT_MESSAGE_BYTES as usize)));
    assert!(content.ends_with("\n...[message truncated]"));

    let (content, truncated) = bounded_transcript_content(b"short", 5);
    assert!(!truncated);
    assert_eq!(content, "short");
    assert_eq!(bounded_transcript_content(&[], 0), (String::new(), false));
}

#[allow(dead_code)]
fn _keep_tool_id_in_scope(_: ToolCallId) {}
