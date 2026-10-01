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
