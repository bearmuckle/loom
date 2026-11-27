//! End-to-end tests: a real in-process worker served over the WebSocket
//! transport, driven through `loom-mcp`'s runner, tools, resources, and its
//! streamable HTTP endpoint.

use std::sync::Arc;

use loom_core::{Capability, CapabilitySet};
use loom_mcp::{AppState, Config, HttpLimits, Secret, http, resources, runner, tools};
use loom_protocol::{
    CURRENT_PROTOCOL_VERSION, ClientRequest, ControlRequest, ControlResponse, RequestEnvelope,
    ServerResponse, SessionResponse, WorkspaceRequest, WorkspaceResponse,
};
use loom_server::{
    AuthTokenStore, AuthorizationScope, InProcessBackend, RemoteServer, RemoteServerConfig,
    RunningRemoteServer,
};
use serde_json::{Value, json};

struct Worker {
    _backend: Arc<InProcessBackend>,
    server: RunningRemoteServer,
    url: String,
    token: String,
}

async fn spawn_worker() -> Worker {
    let backend = InProcessBackend::new();
    let auth = Arc::new(AuthTokenStore::new());
    let issued = auth
        .insert("mcp-test", AuthorizationScope::all())
        .expect("insert test token");
    let server = RemoteServer::new(backend.clone(), auth, RemoteServerConfig::local_ephemeral())
        .bind()
        .await
        .expect("bind test worker");
    Worker {
        _backend: backend,
        url: server.websocket_url().to_owned(),
        token: issued.token,
        server,
    }
}

fn config(url: &str, token: &str, read_only: bool) -> Config {
    Config {
        connect: url.to_owned(),
        token: Secret::new(token.to_owned()).unwrap(),
        ca_file: None,
        allow_insecure_remote: false,
        max_frame_bytes: None,
        bind: "127.0.0.1:0".parse().unwrap(),
        tls: None,
        read_only,
        allow_mutating: false,
        allowed_hosts: Vec::new(),
    }
}

fn negotiate(connection: &loom_server::InProcessConnection) {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Control(
        ControlRequest::Negotiate {
            client_version: CURRENT_PROTOCOL_VERSION,
            capabilities: CapabilitySet::new([
                Capability::JsonProtocol,
                Capability::CreateAgentSession,
                Capability::ManageWorkspaces,
                Capability::ReadAgentSession,
            ]),
        },
    )));
    assert!(
        matches!(
            response.result,
            Ok(ServerResponse::Control(ControlResponse::Negotiated(_)))
        ),
        "negotiation failed: {:?}",
        response.result
    );
}

fn create_workspace(
    connection: &loom_server::InProcessConnection,
    name: &str,
) -> loom_protocol::WorkspaceRecord {
    match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: name.to_owned(),
            },
        )))
        .result
    {
        Ok(ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(record))) => record,
        other => panic!("unexpected workspace creation response: {other:?}"),
    }
}

fn create_session(
    connection: &loom_server::InProcessConnection,
    workspace_id: loom_core::WorkspaceId,
    name: &str,
) -> loom_core::AgentSessionSnapshot {
    match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id,
                name: name.to_owned(),
            },
        )))
        .result
    {
        Ok(ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot))) => snapshot,
        other => panic!("unexpected session creation response: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn read_tools_run_against_a_real_worker_without_secrets() {
    let worker = spawn_worker().await;
    let config = config(&worker.url, &worker.token, false);

    let providers = runner::run_verb(&config, &["providers".to_owned(), "list".to_owned()]).await;
    assert_eq!(providers.exit_code, 0, "{}", providers.stderr);
    assert!(!providers.stdout.contains(&worker.token), "token leaked");
    assert!(!providers.stdout.contains("sk-"), "provider key leaked");
    assert!(
        !format!("{:?}", config).contains(&worker.token),
        "token in debug"
    );

    let models = runner::run_verb(&config, &["models".to_owned(), "list".to_owned()]).await;
    assert_eq!(models.exit_code, 0, "{}", models.stderr);
    assert!(!models.stdout.contains("sk-"), "provider key leaked");

    let ls = runner::run_verb(&config, &["ls".to_owned()]).await;
    assert_eq!(ls.exit_code, 0, "{}", ls.stderr);

    worker.server.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn status_ls_and_events_resources_read_session_state() {
    let worker = spawn_worker().await;
    let connection = worker._backend.connect();
    negotiate(&connection);
    let workspace = create_workspace(&connection, "mcp workspace");
    let session = create_session(&connection, workspace.id, "mcp session");
    let config = config(&worker.url, &worker.token, false);

    let ls = runner::run_verb(
        &config,
        &[
            "ls".to_owned(),
            "--workspace".to_owned(),
            workspace.id.to_string(),
        ],
    )
    .await;
    assert_eq!(ls.exit_code, 0, "{}", ls.stderr);
    assert!(ls.stdout.contains(&session.id.to_string()), "{}", ls.stdout);

    let status = runner::run_verb(
        &config,
        &[
            "status".to_owned(),
            "--session".to_owned(),
            session.id.to_string(),
        ],
    )
    .await;
    assert_eq!(status.exit_code, 0, "{}", status.stderr);
    assert!(status.stdout.contains("idle"), "{}", status.stdout);

    let state = AppState {
        config,
        limits: HttpLimits::default(),
    };
    let read = resources::read_resource(
        &state,
        &json!({ "uri": format!("loom://sessions/{}/events", session.id) }),
    )
    .await
    .expect("events resource");
    let text = read["contents"][0]["text"].as_str().unwrap();
    let content: Value = serde_json::from_str(text).unwrap();
    assert_eq!(content["session"], session.id.to_string());
    assert!(
        !content["events"].as_array().unwrap().is_empty(),
        "a fresh session journals its creation: {content}"
    );

    worker.server.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn read_only_mode_rejects_mutating_tools() {
    let worker = spawn_worker().await;
    let state = AppState {
        config: config(&worker.url, &worker.token, true),
        limits: HttpLimits::default(),
    };

    let blocked = tools::call_tool(
        &state,
        &json!({ "name": "run", "arguments": { "workspace": "w1", "task": "x" } }),
    )
    .await;
    let error = blocked.expect_err("run must be rejected in read-only mode");
    assert!(error.message.contains("disabled"), "{error:?}");

    for name in [
        "send",
        "approve",
        "deny",
        "archive",
        "copilot_login_start",
        "copilot_login_status",
    ] {
        let arguments = match name {
            "archive" => json!({ "session": "s1" }),
            "send" => json!({ "run": "r1", "text": "x" }),
            "copilot_login_start" => json!({ "provider": "github-copilot", "no_wait": true }),
            "copilot_login_status" => json!({ "login_id": "login-1" }),
            _ => json!({ "run": "r1", "tool_call": "t1" }),
        };
        assert!(
            tools::call_tool(&state, &json!({ "name": name, "arguments": arguments }))
                .await
                .is_err(),
            "{name} must be rejected in read-only mode"
        );
    }

    let read = tools::call_tool(
        &state,
        &json!({ "name": "providers_list", "arguments": {} }),
    )
    .await
    .expect("read tools stay available");
    assert_eq!(read["isError"], false);
    assert_eq!(read["structuredContent"]["exit_code"], 0);

    worker.server.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn streamable_http_endpoint_authenticates_and_serves_tools() {
    let worker = spawn_worker().await;
    let mut config = config(&worker.url, &worker.token, false);
    config.allow_mutating = true;
    let running = http::serve(config).await.expect("serve MCP over HTTP");
    let endpoint = format!("http://{}/mcp", running.local_addr);
    let client = reqwest::Client::new();

    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "protocolVersion": "2025-06-18" },
    });

    let unauthorized = client
        .post(&endpoint)
        .json(&initialize)
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), 401);

    let authorized = client
        .post(&endpoint)
        .bearer_auth(&worker.token)
        .json(&initialize)
        .send()
        .await
        .unwrap();
    assert_eq!(authorized.status(), 200);
    let init_body: Value = authorized.json().await.unwrap();
    assert_eq!(init_body["result"]["serverInfo"]["name"], "loom-mcp");

    let list = client
        .post(&endpoint)
        .bearer_auth(&worker.token)
        .json(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }))
        .send()
        .await
        .unwrap();
    assert_eq!(list.status(), 200);
    let list_body: Value = list.json().await.unwrap();
    let listed_tools = list_body["result"]["tools"].as_array().unwrap();
    assert_eq!(listed_tools.len(), 11);
    for mutating_name in ["copilot_login_start", "copilot_login_status"] {
        let tool = listed_tools
            .iter()
            .find(|tool| tool["name"] == mutating_name)
            .unwrap_or_else(|| panic!("missing {mutating_name}"));
        assert_eq!(tool["annotations"]["destructiveHint"], true, "{tool}");
        let serialized = tool.to_string();
        assert!(!serialized.contains("access_token"), "{serialized}");
        assert!(
            !serialized.contains("ConfigureGitHubCopilot"),
            "{serialized}"
        );
    }

    let call = client
        .post(&endpoint)
        .bearer_auth(&worker.token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "providers_list", "arguments": {} },
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(call.status(), 200);
    let call_body: Value = call.json().await.unwrap();
    assert_eq!(call_body["result"]["isError"], false);
    assert_eq!(call_body["result"]["structuredContent"]["exit_code"], 0);
    assert!(
        !call_body.to_string().contains(&worker.token),
        "token leaked"
    );

    running.shutdown();
    let _ = running.task.await;
    worker.server.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn streamable_http_rejects_bad_hosts_origins_media_types_and_shapes() {
    let worker = spawn_worker().await;
    let running = http::serve(config(&worker.url, &worker.token, false))
        .await
        .expect("serve MCP over HTTP");
    let endpoint = format!("http://{}/mcp", running.local_addr);
    let client = reqwest::Client::new();
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "protocolVersion": "2025-06-18" },
    });

    // Hosts outside the bind-derived allowlist are rejected before auth.
    let rebind = client
        .post(&endpoint)
        .header("Host", "attacker.example")
        .bearer_auth(&worker.token)
        .json(&initialize)
        .send()
        .await
        .unwrap();
    assert_eq!(rebind.status(), 400);

    let origin = client
        .post(&endpoint)
        .header("Origin", "https://attacker.example")
        .bearer_auth(&worker.token)
        .json(&initialize)
        .send()
        .await
        .unwrap();
    assert_eq!(origin.status(), 400);

    // A non-JSON content type is refused with 415.
    let text_plain = client
        .post(&endpoint)
        .bearer_auth(&worker.token)
        .header("Content-Type", "text/plain")
        .body(initialize.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(text_plain.status(), 415);

    // Scalar / empty-object / empty-batch bodies are invalid requests, not 202s.
    for body in ["123", "[]", "{}"] {
        let response = client
            .post(&endpoint)
            .bearer_auth(&worker.token)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{body}");
        let value: Value = response.json().await.unwrap();
        assert_eq!(value["error"]["code"], -32600, "{body}");
        assert_eq!(value["id"], Value::Null, "{body}");
    }

    // A batch over the cap is refused as an invalid request.
    let mut batch = Vec::new();
    for id in 0..=32 {
        batch.push(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "ping",
        }));
    }
    let response = client
        .post(&endpoint)
        .bearer_auth(&worker.token)
        .header("Content-Type", "application/json")
        .body(Value::Array(batch).to_string())
        .send()
        .await
        .unwrap();
    let value: Value = response.json().await.unwrap();
    assert_eq!(value["error"]["code"], -32600);

    running.shutdown();
    let _ = running.task.await;
    worker.server.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn mutating_tools_are_opt_in_and_archive_runs_end_to_end() {
    let worker = spawn_worker().await;
    let connection = worker._backend.connect();
    negotiate(&connection);
    let workspace = create_workspace(&connection, "mcp mutating workspace");
    let session = create_session(&connection, workspace.id, "mcp mutating session");

    let mut config = config(&worker.url, &worker.token, false);
    let running = http::serve(config.clone())
        .await
        .expect("serve MCP over HTTP");
    let endpoint = format!("http://{}/mcp", running.local_addr);
    let client = reqwest::Client::new();

    // Mutating tools are hidden by default.
    let list = client
        .post(&endpoint)
        .bearer_auth(&worker.token)
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))
        .send()
        .await
        .unwrap();
    let list_body: Value = list.json().await.unwrap();
    let names = list_body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(!names.contains(&"archive"), "{names:?}");

    // Calling one is invalid params until the operator opts in.
    let blocked = tools::call_tool(
        &AppState {
            config: config.clone(),
            limits: HttpLimits::default(),
        },
        &json!({ "name": "archive", "arguments": { "session": session.id.to_string() } }),
    )
    .await;
    assert!(blocked.is_err());

    config.allow_mutating = true;
    let state = AppState {
        config,
        limits: HttpLimits::default(),
    };
    let archived = tools::call_tool(
        &state,
        &json!({ "name": "archive", "arguments": { "session": session.id.to_string() } }),
    )
    .await
    .expect("archive succeeds once mutating tools are enabled");
    assert_eq!(archived["isError"], false, "{archived}");
    assert_eq!(archived["structuredContent"]["exit_code"], 0);

    running.shutdown();
    let _ = running.task.await;
    worker.server.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn copilot_login_tools_run_against_stub_worker_without_tokens_and_refuse_insecure_url() {
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{
        Router,
        extract::{
            State,
            ws::{Message, WebSocket, WebSocketUpgrade},
        },
        response::IntoResponse,
        routing::get,
    };
    use loom_protocol::{
        GitHubCopilotLoginStatus, NegotiationResult, ProviderRequest, ProviderResponse,
        ResponseEnvelope,
    };

    #[derive(Clone)]
    struct StubState {
        status: Arc<Mutex<GitHubCopilotLoginStatus>>,
        status_polls: Arc<AtomicUsize>,
    }

    let stub_state = StubState {
        status: Arc::new(Mutex::new(GitHubCopilotLoginStatus::Pending)),
        status_polls: Arc::new(AtomicUsize::new(0)),
    };

    async fn handle_socket(mut socket: WebSocket, state: StubState) {
        while let Some(Ok(message)) = socket.recv().await {
            let bytes = match message {
                Message::Text(text) => text.as_bytes().to_vec(),
                Message::Binary(bin) => bin.to_vec(),
                Message::Close(_) => break,
                _ => continue,
            };
            let request = loom_protocol::decode_request(&bytes).expect("decode request");
            let response = match request.request {
                ClientRequest::Control(ControlRequest::Negotiate { capabilities, .. }) => {
                    ResponseEnvelope::success(
                        request.request_id,
                        ServerResponse::Control(ControlResponse::Negotiated(NegotiationResult {
                            protocol_version: CURRENT_PROTOCOL_VERSION,
                            capabilities,
                        })),
                    )
                }
                ClientRequest::Provider(ProviderRequest::StartGitHubCopilotLogin) => {
                    ResponseEnvelope::success(
                        request.request_id,
                        ServerResponse::Provider(ProviderResponse::GitHubCopilotLoginStarted {
                            login_id: "login-mcp-1".to_owned(),
                            user_code: "USER-CODE".to_owned(),
                            verification_uri: "https://example.test/device".to_owned(),
                            expires_in: 60,
                            interval: 1,
                        }),
                    )
                }
                ClientRequest::Provider(ProviderRequest::GetGitHubCopilotLoginStatus {
                    ..
                }) => {
                    state.status_polls.fetch_add(1, Ordering::SeqCst);
                    let current = state.status.lock().unwrap().clone();
                    ResponseEnvelope::success(
                        request.request_id,
                        ServerResponse::Provider(ProviderResponse::GitHubCopilotLoginStatus {
                            status: current,
                        }),
                    )
                }
                other => panic!("unexpected request to copilot stub worker: {other:?}"),
            };
            let encoded = loom_protocol::encode_response(&response).expect("encode response");
            let text = String::from_utf8(encoded).expect("utf-8 response");
            if socket.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    }

    async fn ws_handler(ws: WebSocketUpgrade, State(state): State<StubState>) -> impl IntoResponse {
        ws.on_upgrade(move |socket| handle_socket(socket, state))
    }

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .with_state(stub_state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral stub worker");
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let stub_task = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await;
    });

    let stub_url = format!("ws://{addr}/ws");
    let fake_bearer = "stub-operator-bearer-token";
    let mut cfg = config(&stub_url, fake_bearer, false);
    cfg.allow_mutating = true;
    let state = AppState {
        config: cfg,
        limits: HttpLimits::default(),
    };

    let assert_clean = |value: &Value| {
        let serialized = value.to_string();
        for forbidden in [
            fake_bearer,
            "access_token",
            "ConfigureGitHubCopilot",
            "credential_id",
            "api_key",
        ] {
            assert!(
                !serialized.contains(forbidden),
                "MCP tool result must never contain '{forbidden}': {serialized}"
            );
        }
    };

    // 1. Start with default `{}`: defaults no_wait to true, returns public
    // url/user_code/login_id immediately, and performs zero status polls.
    let started = tools::call_tool(
        &state,
        &json!({
            "name": "copilot_login_start",
            "arguments": {},
        }),
    )
    .await
    .expect("copilot_login_start succeeds with default arguments");
    assert_eq!(started["isError"], false, "{started}");
    assert_eq!(started["structuredContent"]["exit_code"], 0);
    assert_eq!(
        started["structuredContent"]["data"],
        json!({
            "provider": "github-copilot",
            "login_id": "login-mcp-1",
            "url": "https://example.test/device",
            "user_code": "USER-CODE",
            "expires_in": 60,
            "interval": 1,
            "status": "pending",
        })
    );
    assert_eq!(
        stub_state.status_polls.load(Ordering::SeqCst),
        0,
        "copilot_login_start with default arguments must not poll status"
    );
    assert_clean(&started);

    // Explicitly passing `no_wait: false` is rejected before contacting the worker.
    let wait_rejected = tools::call_tool(
        &state,
        &json!({
            "name": "copilot_login_start",
            "arguments": { "no_wait": false },
        }),
    )
    .await
    .expect_err("copilot_login_start must reject no_wait: false");
    assert!(
        wait_rejected.message.contains("copilot_login_status"),
        "{wait_rejected:?}"
    );
    assert_eq!(stub_state.status_polls.load(Ordering::SeqCst), 0);

    // 2. Poll pending -> exit 4 with status: pending.
    let pending = tools::call_tool(
        &state,
        &json!({
            "name": "copilot_login_status",
            "arguments": { "login_id": "login-mcp-1" },
        }),
    )
    .await
    .expect("copilot_login_status pending");
    assert_eq!(pending["structuredContent"]["exit_code"], 4);
    assert_eq!(
        pending["structuredContent"]["data"],
        json!({ "login_id": "login-mcp-1", "status": "pending" })
    );
    assert_clean(&pending);

    // 3. Flip to configured -> exit 0 with status: configured.
    *stub_state.status.lock().unwrap() = GitHubCopilotLoginStatus::Configured;
    let configured = tools::call_tool(
        &state,
        &json!({
            "name": "copilot_login_status",
            "arguments": { "login_id": "login-mcp-1" },
        }),
    )
    .await
    .expect("copilot_login_status configured");
    assert_eq!(configured["isError"], false, "{configured}");
    assert_eq!(configured["structuredContent"]["exit_code"], 0);
    assert_eq!(
        configured["structuredContent"]["data"],
        json!({ "login_id": "login-mcp-1", "status": "configured" })
    );
    assert_clean(&configured);

    // 4. Flip to failed -> exit 1 with status: failed and message.
    *stub_state.status.lock().unwrap() = GitHubCopilotLoginStatus::Failed {
        message: "device code denied".to_owned(),
    };
    let failed = tools::call_tool(
        &state,
        &json!({
            "name": "copilot_login_status",
            "arguments": { "login_id": "login-mcp-1" },
        }),
    )
    .await
    .expect("copilot_login_status failed");
    assert_eq!(failed["isError"], true, "{failed}");
    assert_eq!(failed["structuredContent"]["exit_code"], 1);
    assert_eq!(
        failed["structuredContent"]["data"],
        json!({
            "login_id": "login-mcp-1",
            "status": "failed",
            "message": "device code denied",
        })
    );
    assert_clean(&failed);

    // 5. Insecure non-loopback ws:// is refused even when allow_insecure_remote is true.
    let mut insecure_cfg = config("ws://worker.example/ws", fake_bearer, false);
    insecure_cfg.allow_mutating = true;
    insecure_cfg.allow_insecure_remote = true;
    let insecure_state = AppState {
        config: insecure_cfg,
        limits: HttpLimits::default(),
    };
    for (name, arguments) in [
        (
            "copilot_login_start",
            json!({ "provider": "github-copilot", "no_wait": true }),
        ),
        ("copilot_login_status", json!({ "login_id": "login-mcp-1" })),
    ] {
        let refused = tools::call_tool(
            &insecure_state,
            &json!({ "name": name, "arguments": arguments }),
        )
        .await
        .expect("insecure refusal returns structured error result");
        assert_eq!(refused["isError"], true, "{name}: {refused}");
        assert_eq!(
            refused["structuredContent"]["exit_code"], 2,
            "{name}: {refused}"
        );
        assert_eq!(
            refused["structuredContent"]["error"]["code"], "invalid_request",
            "{name}: {refused}"
        );
        assert_clean(&refused);
    }

    let _ = shutdown_tx.send(());
    let _ = stub_task.await;
}
