//! Streamable HTTP transport: a stateless `POST /mcp` endpoint that answers
//! with `application/json`, bearer-auth middleware, and a TLS listener that
//! mirrors the worker's `--tls-cert` / `--tls-key` shape. There are no
//! server-initiated notifications, so responses are single-shot JSON and
//! callers poll `status` / the event resources as `loom-cli` documents.

use std::{io, net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    Router,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;

use crate::config::{AppState, Config, server_tls_config, validate_bind};
use crate::jsonrpc;

/// The router an HTTP listener serves. Only `/mcp` requires the bearer token;
/// `/health` stays open for operators and overlays.
pub fn app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/mcp", post(handle_mcp))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_bearer,
        ))
        .route("/health", get(handle_health))
        .with_state(state)
}

async fn handle_health() -> &'static str {
    "ok\n"
}

async fn require_bearer(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    if !authenticate(&state, &headers) {
        return json_response(
            StatusCode::UNAUTHORIZED,
            &json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": {
                    "code": -32001,
                    "message": "unauthorized: present the operator bearer token",
                },
            }),
        );
    }
    next.run(request).await
}

/// A listening socket that completes a TLS handshake with every accepted
/// connection, so `axum::serve` handles HTTPS with the same router as the
/// plaintext listener.
struct TlsListener {
    listener: TcpListener,
    acceptor: TlsAcceptor,
}

impl TlsListener {
    fn new(listener: TcpListener, acceptor: TlsAcceptor) -> Self {
        Self { listener, acceptor }
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, address) = match self.listener.accept().await {
                Ok(accepted) => accepted,
                Err(error) => {
                    log::warn!("MCP TLS listener could not accept a connection: {error}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };
            // A client that never finishes the handshake must not hold the
            // accept loop.
            match timeout(Duration::from_secs(10), self.acceptor.accept(stream)).await {
                Ok(Ok(stream)) => return (stream, address),
                Ok(Err(error)) => log::warn!("TLS handshake with {address} failed: {error}"),
                Err(_) => log::warn!("TLS handshake with {address} timed out"),
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }
}

/// A running HTTP listener.
pub struct RunningServer {
    pub local_addr: SocketAddr,
    pub task: tokio::task::JoinHandle<()>,
}

/// Validates the bind policy, binds, and serves the router. The task runs until
/// aborted or the listener fails.
pub async fn serve(config: Config) -> Result<RunningServer, String> {
    validate_bind(&config)?;
    let listener = TcpListener::bind(config.bind).await.map_err(|error| {
        format!(
            "could not bind the MCP listener to {}: {error}",
            config.bind
        )
    })?;
    let local_addr = listener.local_addr().map_err(|error| error.to_string())?;
    let state = Arc::new(AppState {
        config: config.clone(),
    });
    let router = app(state);
    let task = tokio::spawn(async move {
        if let Some(tls) = &config.tls {
            match server_tls_config(tls) {
                Ok(server_config) => {
                    let acceptor = TlsAcceptor::from(server_config);
                    let listener = TlsListener::new(listener, acceptor);
                    let _ = axum::serve(listener, router).await;
                }
                Err(error) => log::error!("could not build the MCP TLS listener: {error}"),
            }
        } else {
            let _ = axum::serve(listener, router).await;
        }
    });
    Ok(RunningServer { local_addr, task })
}

/// Whether the caller's `Authorization: Bearer <token>` matches the operator
/// token.
fn authenticate(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return false;
    };
    state.config.authenticates(token)
}

async fn handle_mcp(State(state): State<Arc<AppState>>, body: String) -> Response {
    let value: Value = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(_) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": { "code": -32700, "message": "parse error" },
                }),
            );
        }
    };
    let is_batch = value.is_array();
    let messages: Vec<Value> = if is_batch {
        value.as_array().cloned().unwrap_or_default()
    } else {
        vec![value]
    };
    let mut responses = Vec::new();
    for message in &messages {
        if let Some(response) = jsonrpc::handle_message(state.as_ref(), message).await {
            responses.push(response);
        }
    }
    if responses.is_empty() {
        StatusCode::ACCEPTED.into_response()
    } else if is_batch {
        json_response(StatusCode::OK, &Value::Array(responses))
    } else {
        json_response(StatusCode::OK, &responses[0])
    }
}

fn json_response(status: StatusCode, value: &Value) -> Response {
    let mut response = (status, value.to_string()).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    response
}
