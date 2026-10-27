//! In-process tests: providers.

#[allow(unused_imports)]
use super::support::*;
use super::*;

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
fn an_operator_removes_an_api_key_through_the_protocol() {
    let root = std::env::temp_dir().join(format!("loom-api-key-remove-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let path = root.join("backend.sqlite");
    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let connection = backend.connect();
    negotiate_m3(&connection);
    let provider_id = ProviderId::new("openai-compatible");

    let configured = connection.request(RequestEnvelope::new(ClientRequest::Provider(
        ProviderRequest::ConfigureApiKeyProvider {
            provider_id: provider_id.clone(),
            api_key: "openai-key-to-drop".to_owned(),
        },
    )));
    assert!(
        matches!(
            &configured.result,
            Ok(ServerResponse::Provider(
                ProviderResponse::ProviderConfigured
            ))
        ),
        "{:?}",
        configured.result
    );

    let removed = connection.request(RequestEnvelope::new(ClientRequest::Provider(
        ProviderRequest::RemoveApiKeyProvider {
            provider_id: provider_id.clone(),
        },
    )));
    assert!(matches!(
        removed.result,
        Ok(ServerResponse::Provider(
            ProviderResponse::ProviderConfigured
        ))
    ));
    let provider = backend
        .provider_registry()
        .list_providers()
        .unwrap()
        .into_iter()
        .find(|provider| provider.id == provider_id)
        .unwrap();
    assert!(provider.credential_id.is_none());
    drop(connection);
    drop(backend);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn github_access_tokens_are_not_persisted_in_sqlite() {
    let root = std::env::temp_dir().join(format!("loom-github-token-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let path = root.join("backend.sqlite");
    let copilot_secret = "gho_copilot_idempotency_secret";
    let repository_secret = "gho_repository_idempotency_secret";

    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let connection = backend.connect();
    negotiate_m3(&connection);

    for request in [
        ClientRequest::Provider(ProviderRequest::ConfigureGitHubCopilot {
            access_token: copilot_secret.to_owned(),
        }),
        ClientRequest::Provider(ProviderRequest::ConfigureGitHubRepository {
            access_token: repository_secret.to_owned(),
        }),
    ] {
        let response = connection.request(RequestEnvelope::new(request));
        assert!(matches!(
            response.result,
            Ok(ServerResponse::Provider(
                ProviderResponse::ProviderConfigured
            ))
        ));
    }
    drop(connection);
    drop(backend);

    // The durable idempotency journal must not carry the raw access token.
    let persistence = FilePersistence::open(&path).unwrap();
    for record in persistence.load_idempotency_records().unwrap().values() {
        let request = record.request.to_string();
        assert!(!request.contains(copilot_secret));
        assert!(!request.contains(repository_secret));
    }
    drop(persistence);

    // No part of the SQLite database, including the WAL, may contain it either.
    let mut database = fs::read(&path).unwrap();
    let wal = PathBuf::from(format!("{}-wal", path.display()));
    if let Ok(bytes) = fs::read(&wal) {
        database.extend(bytes);
    }
    for secret in [copilot_secret, repository_secret] {
        assert!(
            !database
                .windows(secret.len())
                .any(|window| window == secret.as_bytes()),
            "raw GitHub access token must not be persisted in SQLite"
        );
    }

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
            activity_detail: loom_protocol::ActivityDetail::Full,
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

/// Registers an API-key provider that serves `model_id` through the scripted
/// endpoint, authenticated with `api_key`, so a test can tell from each model
/// request which key the run used.
fn scripted_api_key_provider(
    registry: &ProviderRegistry,
    endpoint: &str,
    provider_id: &str,
    model_id: &ModelId,
    api_key: &str,
) -> ProviderId {
    let provider_id = ProviderId::new(provider_id);
    registry
        .register(ProviderConfig::openai_compatible(
            provider_id.clone(),
            "Scripted API-key model",
            endpoint,
            ModelDescriptor {
                id: model_id.clone(),
                provider: provider_id.clone(),
                display_name: "Scripted API-key model".to_owned(),
                context_window: Some(16_384),
                max_input_tokens: None,
                max_output_tokens: None,
                capabilities: ModelCapabilities {
                    streaming: false,
                    tool_calling: true,
                    vision: false,
                    json_mode: false,
                },
            },
            None,
        ))
        .unwrap();
    registry
        .configure_api_key_provider(&provider_id, api_key.to_owned())
        .unwrap();
    provider_id
}

fn create_key_session(connection: &InProcessConnection, name: &str) -> AgentSessionId {
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: name.to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: name.to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    }
}

fn start_key_run(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
    model: &ModelId,
) -> loom_core::RunId {
    let started = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRun {
            session_id,
            task: "Inspect the workspace.".to_owned(),
            model: model.clone(),
            system_instructions: None,
            repository_instructions: None,
        },
    )));
    match started.result.unwrap() {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected run response: {response:?}"),
    }
}

fn run_snapshot(
    connection: &InProcessConnection,
    run_id: loom_core::RunId,
) -> loom_agent::AgentRunSnapshot {
    let run = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRun { run_id },
    )));
    match run.result.unwrap() {
        ServerResponse::Run(RunResponse::AgentRun(snapshot)) => snapshot,
        response => panic!("unexpected run response: {response:?}"),
    }
}

fn run_projection(
    connection: &InProcessConnection,
    run_id: loom_core::RunId,
) -> loom_protocol::AgentRunSnapshotProjection {
    let run = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunSnapshot {
            activity_detail: loom_protocol::ActivityDetail::Full,
            run_id,
        },
    )));
    match run.result.unwrap() {
        ServerResponse::Run(RunResponse::AgentRunSnapshot(projection)) => *projection,
        response => panic!("unexpected run snapshot response: {response:?}"),
    }
}

fn replace_api_key(connection: &InProcessConnection, provider_id: &ProviderId, api_key: &str) {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Provider(
        ProviderRequest::ConfigureApiKeyProvider {
            provider_id: provider_id.clone(),
            api_key: api_key.to_owned(),
        },
    )));
    assert!(
        matches!(
            response.result,
            Ok(ServerResponse::Provider(
                ProviderResponse::ProviderConfigured
            ))
        ),
        "the replaced key must be reported as configured"
    );
}

fn continue_key_run(connection: &InProcessConnection, run_id: loom_core::RunId) {
    let snapshot = run_snapshot(connection, run_id);
    let continued = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::SendAgentMessage {
            run_id,
            attempt_id: snapshot.attempt_id,
            expected_control_revision: snapshot.control_revision,
            message: "Say goodbye.".to_owned(),
        },
    )));
    assert!(
        matches!(
            continued.result,
            Ok(ServerResponse::Run(RunResponse::AgentRun(_)))
        ),
        "the run must accept the follow-up message"
    );
}

/// The next scripted request for `model_id`, remembering the requests of other
/// runs parked meanwhile.
fn next_request_for(
    endpoint: &mut ScriptedOpenAiEndpoint,
    model_id: &ModelId,
) -> ScriptedModelRequest {
    if let Some(index) = endpoint
        .pending
        .iter()
        .position(|request| request.request["model"] == model_id.as_str())
    {
        return endpoint.pending.remove(index).unwrap();
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for the scripted model request"
        );
        let request = endpoint
            .requests
            .recv_timeout(remaining)
            .expect("scripted model request did not arrive");
        if request.request["model"] == model_id.as_str() {
            return request;
        }
        endpoint.pending.push_back(request);
    }
}

/// A key replaced while a run's turn is in flight must be the one the run
/// authenticates its next model request with.
#[test]
fn a_replaced_api_key_reaches_a_run_whose_turn_is_in_flight() {
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_id = ModelId::new("fixture/key-in-flight");
    let providers = ProviderRegistry::new();
    let provider_id = scripted_api_key_provider(
        &providers,
        &model.endpoint,
        "scripted-keys",
        &model_id,
        "key-a",
    );
    let backend = InProcessBackend::with_provider_registry(providers);
    let connection = backend.connect();
    negotiate(&connection);
    let session_id = create_key_session(&connection, "Key rotation");
    let run_id = start_key_run(&connection, session_id, &model_id);

    let first_turn = next_request_for(&mut model, &model_id);
    assert_eq!(first_turn.authorization.as_deref(), Some("Bearer key-a"));
    // The turn is still open, so the replacement is accepted for the next
    // request instead of replacing the provider under the call in flight.
    replace_api_key(&connection, &provider_id, "key-b");
    model.respond_with_tool(first_turn, "list_files", serde_json::json!({}));

    let next_turn = next_request_for(&mut model, &model_id);
    assert_eq!(next_turn.authorization.as_deref(), Some("Bearer key-b"));
    model.respond_with_text(next_turn, "Workspace inspected.");

    let settled = await_settled_run(&connection, run_id);
    assert_eq!(settled.state, AgentRunState::Completed);
    fs::remove_dir_all(&backend.session_root_base).unwrap();
}

/// A run waiting for approval when the key is replaced must use the new key for
/// the request it makes after it continues.
#[test]
fn a_run_awaiting_approval_uses_the_replaced_key_when_it_continues() {
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_id = ModelId::new("fixture/key-awaiting-approval");
    let providers = ProviderRegistry::new();
    let provider_id = scripted_api_key_provider(
        &providers,
        &model.endpoint,
        "scripted-keys",
        &model_id,
        "key-a",
    );
    let backend = InProcessBackend::with_provider_registry(providers);
    let connection = backend.connect();
    negotiate(&connection);
    let session_id = create_key_session(&connection, "Key rotation while waiting");
    // A policy that gates workspace writes leaves the run waiting for approval
    // instead of finishing its first turn.
    let gated = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::SetSessionApprovalPolicy {
            session_id,
            policy: ApprovalPolicy::default(),
            auto_approve_actions: Some(false),
        },
    )));
    assert!(matches!(
        gated.result,
        Ok(ServerResponse::Session(SessionResponse::ApprovalPolicy(_)))
    ));
    let run_id = start_key_run(&connection, session_id, &model_id);

    let first_turn = next_request_for(&mut model, &model_id);
    assert_eq!(first_turn.authorization.as_deref(), Some("Bearer key-a"));
    model.respond_with_tool(
        first_turn,
        "apply_patch",
        serde_json::json!({
            "path": "key-rotation.txt",
            "old_text": "",
            "new_text": "created\n"
        }),
    );
    let waiting = await_settled_run(&connection, run_id);
    assert_eq!(waiting.state, AgentRunState::AwaitingApproval);

    replace_api_key(&connection, &provider_id, "key-b");

    let projection = run_projection(&connection, run_id);
    let call = projection
        .pending_approval
        .expect("the run waits for a tool approval");
    let approved = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::ApproveAgentAction {
            run_id,
            attempt_id: projection.run.attempt_id,
            expected_control_revision: projection.run.control_revision,
            tool_call_id: call.id,
        },
    )));
    assert!(approved.result.is_ok());

    let next_turn = next_request_for(&mut model, &model_id);
    assert_eq!(next_turn.authorization.as_deref(), Some("Bearer key-b"));
    model.respond_with_text(next_turn, "Change applied.");
    let settled = await_settled_run(&connection, run_id);
    assert_eq!(settled.state, AgentRunState::Completed);
    fs::remove_dir_all(&backend.session_root_base).unwrap();
}

/// A run pinned to a participant-owned credential must keep it: replacing the
/// worker-wide key never replaces another person's account.
#[test]
fn a_run_with_a_pinned_credential_keeps_it_after_a_worker_key_replacement() {
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_id = ModelId::new("fixture/key-pinned");
    let providers = ProviderRegistry::new();
    let provider_id = scripted_api_key_provider(
        &providers,
        &model.endpoint,
        "scripted-keys",
        &model_id,
        "key-a",
    );
    let backend = InProcessBackend::with_provider_registry(providers);
    let connection = backend.connect();
    negotiate(&connection);
    let session_id = create_key_session(&connection, "Key rotation with a pin");
    let owner = loom_core::ParticipantIdentity {
        id: loom_core::ParticipantId::new(),
        display_name: "Owner".to_owned(),
    };
    let store = backend.providers.instance_credentials().unwrap();
    let credential = backend
        .provider_credentials
        .sign_in(
            store.as_ref(),
            provider_id.as_str(),
            &owner,
            "participant-key".to_owned(),
        )
        .unwrap();
    backend
        .provider_credentials
        .select(store.as_ref(), session_id, credential.id, None)
        .unwrap();
    let run_id = start_key_run(&connection, session_id, &model_id);

    let first_turn = next_request_for(&mut model, &model_id);
    assert_eq!(
        first_turn.authorization.as_deref(),
        Some("Bearer participant-key")
    );
    model.respond_with_text(first_turn, "Hello.");
    assert_eq!(
        await_settled_run(&connection, run_id).state,
        AgentRunState::Completed
    );
    let pin = backend
        .provider_credentials
        .run_pin(run_id)
        .expect("the run pinned the selected credential");
    assert_eq!(pin.credential_id, Some(credential.id));

    replace_api_key(&connection, &provider_id, "key-b");
    // The refresh decision itself: the pinned run is not one of the runs the
    // worker-wide key re-authenticates.
    assert_eq!(
        backend.reauthenticate_provider_runs(&provider_id).unwrap(),
        0
    );

    continue_key_run(&connection, run_id);
    let next_turn = next_request_for(&mut model, &model_id);
    assert_eq!(
        next_turn.authorization.as_deref(),
        Some("Bearer participant-key")
    );
    model.respond_with_text(next_turn, "Goodbye.");
    assert_eq!(
        await_settled_run(&connection, run_id).state,
        AgentRunState::Completed
    );
    fs::remove_dir_all(&backend.session_root_base).unwrap();
}

/// Replacing one provider's key leaves a run on another provider authenticated
/// with its own key.
#[test]
fn a_run_on_another_provider_keeps_its_key() {
    let mut model = ScriptedOpenAiEndpoint::start();
    let rotated_model = ModelId::new("fixture/key-rotated");
    let other_model = ModelId::new("fixture/key-untouched");
    let providers = ProviderRegistry::new();
    let rotated = scripted_api_key_provider(
        &providers,
        &model.endpoint,
        "scripted-keys-rotated",
        &rotated_model,
        "key-a",
    );
    let untouched = scripted_api_key_provider(
        &providers,
        &model.endpoint,
        "scripted-keys-untouched",
        &other_model,
        "key-other",
    );
    assert_ne!(untouched, rotated);
    let backend = InProcessBackend::with_provider_registry(providers);
    let connection = backend.connect();
    negotiate(&connection);
    let rotated_session = create_key_session(&connection, "Rotated key");
    let other_session = create_key_session(&connection, "Untouched key");
    let rotated_run = start_key_run(&connection, rotated_session, &rotated_model);
    let other_run = start_key_run(&connection, other_session, &other_model);

    let rotated_turn = next_request_for(&mut model, &rotated_model);
    assert_eq!(rotated_turn.authorization.as_deref(), Some("Bearer key-a"));
    model.respond_with_text(rotated_turn, "First run done.");
    let other_turn = next_request_for(&mut model, &other_model);
    assert_eq!(
        other_turn.authorization.as_deref(),
        Some("Bearer key-other")
    );
    model.respond_with_text(other_turn, "Second run done.");
    assert_eq!(
        await_settled_run(&connection, rotated_run).state,
        AgentRunState::Completed
    );
    assert_eq!(
        await_settled_run(&connection, other_run).state,
        AgentRunState::Completed
    );

    replace_api_key(&connection, &rotated, "key-b");
    // Only the run on the rotated provider is re-authenticated; the run on the
    // other provider is not one of them.
    assert_eq!(backend.reauthenticate_provider_runs(&rotated).unwrap(), 1);

    continue_key_run(&connection, rotated_run);
    let rotated_next = next_request_for(&mut model, &rotated_model);
    assert_eq!(rotated_next.authorization.as_deref(), Some("Bearer key-b"));
    model.respond_with_text(rotated_next, "Rotated key used.");
    continue_key_run(&connection, other_run);
    let other_next = next_request_for(&mut model, &other_model);
    assert_eq!(
        other_next.authorization.as_deref(),
        Some("Bearer key-other")
    );
    model.respond_with_text(other_next, "Own key kept.");
    await_settled_run(&connection, rotated_run);
    await_settled_run(&connection, other_run);
    fs::remove_dir_all(&backend.session_root_base).unwrap();
}

/// A run started after the replacement resolves the configured key at start.
#[test]
fn a_run_started_after_the_replacement_uses_the_new_key() {
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_id = ModelId::new("fixture/key-after-replacement");
    let providers = ProviderRegistry::new();
    let provider_id = scripted_api_key_provider(
        &providers,
        &model.endpoint,
        "scripted-keys",
        &model_id,
        "key-a",
    );
    let backend = InProcessBackend::with_provider_registry(providers);
    let connection = backend.connect();
    negotiate(&connection);

    replace_api_key(&connection, &provider_id, "key-b");

    let session_id = create_key_session(&connection, "Key after replacement");
    let run_id = start_key_run(&connection, session_id, &model_id);
    let first_turn = next_request_for(&mut model, &model_id);
    assert_eq!(first_turn.authorization.as_deref(), Some("Bearer key-b"));
    model.respond_with_text(first_turn, "Hello.");
    assert_eq!(
        await_settled_run(&connection, run_id).state,
        AgentRunState::Completed
    );
    fs::remove_dir_all(&backend.session_root_base).unwrap();
}

/// A replacement that cannot reach the registered runs is reported as an error
/// stating the key was replaced, never as a silent success.
#[test]
fn a_failed_re_authentication_is_reported_instead_of_a_silent_success() {
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_id = ModelId::new("fixture/key-unreachable");
    let providers = ProviderRegistry::new();
    let provider_id = scripted_api_key_provider(
        &providers,
        &model.endpoint,
        "scripted-keys",
        &model_id,
        "key-a",
    );
    let backend = InProcessBackend::with_provider_registry(providers);
    let connection = backend.connect();
    negotiate(&connection);
    let session_id = create_key_session(&connection, "Unreachable key");
    let run_id = start_key_run(&connection, session_id, &model_id);
    let first_turn = next_request_for(&mut model, &model_id);
    assert_eq!(first_turn.authorization.as_deref(), Some("Bearer key-a"));
    model.respond_with_text(first_turn, "Hello.");
    assert_eq!(
        await_settled_run(&connection, run_id).state,
        AgentRunState::Completed
    );

    // The run's provider can no longer be rebuilt from the configuration, so
    // the re-authentication of that run fails after the key was stored.
    backend
        .providers
        .configurations
        .lock()
        .unwrap()
        .get_mut(&provider_id)
        .unwrap()
        .endpoint = None;
    let replaced = connection.request(RequestEnvelope::new(ClientRequest::Provider(
        ProviderRequest::ConfigureApiKeyProvider {
            provider_id: provider_id.clone(),
            api_key: "key-b".to_owned(),
        },
    )));
    let error = replaced.result.unwrap_err();
    assert_eq!(error.code, ErrorCode::Internal);
    assert!(error.message.contains("was replaced"), "{error:?}");
    assert!(
        error.message.contains("could not be re-authenticated"),
        "{error:?}"
    );
    assert!(
        !error.message.contains("key-b"),
        "a key must not be reported"
    );
    fs::remove_dir_all(&backend.session_root_base).unwrap();
}

/// A live run whose model left the catalog keeps the provider it holds, and the
/// replacement still succeeds for the runs that can be rebuilt.
#[test]
fn a_run_whose_model_left_the_catalog_keeps_its_provider() {
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_id = ModelId::new("fixture/key-model-gone");
    let providers = ProviderRegistry::new();
    let provider_id = scripted_api_key_provider(
        &providers,
        &model.endpoint,
        "scripted-keys",
        &model_id,
        "key-a",
    );
    let backend = InProcessBackend::with_provider_registry(providers);
    let connection = backend.connect();
    negotiate(&connection);
    let session_id = create_key_session(&connection, "Key with a retired model");
    let run_id = start_key_run(&connection, session_id, &model_id);
    let first_turn = next_request_for(&mut model, &model_id);
    assert_eq!(first_turn.authorization.as_deref(), Some("Bearer key-a"));
    model.respond_with_text(first_turn, "Hello.");
    assert_eq!(
        await_settled_run(&connection, run_id).state,
        AgentRunState::Completed
    );

    let mut retired = backend
        .providers
        .configurations
        .lock()
        .unwrap()
        .get(&provider_id)
        .cloned()
        .unwrap();
    retired.models[0].id = ModelId::new("fixture/key-model-current");
    backend.providers.register(retired).unwrap();

    replace_api_key(&connection, &provider_id, "key-b");
    assert_eq!(
        backend.reauthenticate_provider_runs(&provider_id).unwrap(),
        0
    );

    continue_key_run(&connection, run_id);
    let next_turn = next_request_for(&mut model, &model_id);
    assert_eq!(next_turn.authorization.as_deref(), Some("Bearer key-a"));
    model.respond_with_text(next_turn, "Goodbye.");
    assert_eq!(
        await_settled_run(&connection, run_id).state,
        AgentRunState::Completed
    );
    fs::remove_dir_all(&backend.session_root_base).unwrap();
}
