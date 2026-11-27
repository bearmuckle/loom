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

    for name in ["send", "approve", "deny", "archive"] {
        let arguments = match name {
            "archive" => json!({ "session": "s1" }),
            "send" => json!({ "run": "r1", "text": "x" }),
            _ => json!({ "run": "r1", "tool_call": "t1" }),
        };
        assert!(
            tools::call_tool(&state, &json!({ "name": name, "arguments": arguments }))
                .await
                .is_err()
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
    assert_eq!(list_body["result"]["tools"].as_array().unwrap().len(), 9);

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
