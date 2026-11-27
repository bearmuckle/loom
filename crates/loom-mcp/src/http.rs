//! Streamable HTTP transport: a stateless `POST /mcp` endpoint that answers
//! with `application/json`, bearer-auth middleware, and a TLS listener that
//! mirrors the worker's `--tls-cert` / `--tls-key` shape. There are no
//! server-initiated notifications, so responses are single-shot JSON and
//! callers poll `status` / the event resources as `loom-cli` documents.

use std::{
    future::Future,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use axum::{
    Router,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_rustls::{TlsAcceptor, server::TlsStream};

use crate::config::{
    AppState, Config, HttpLimits, allowed_hosts, normalize_host, server_tls_config, validate_bind,
};
use crate::jsonrpc;

/// How long one TLS handshake may take in a per-connection task.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

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
    if let Some(response) = reject_bad_host_or_origin(&state, &headers) {
        return response;
    }
    if !authenticate(&state, &headers) {
        let mut response = json_response(
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
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            header::HeaderValue::from_static("Bearer"),
        );
        return response;
    }
    next.run(request).await
}

/// Rejects a `Host`/`Origin` that is not in the bind-derived allowlist, which
/// is what stops a browser page from rebinding the operator's loopback
/// listener to an attacker-controlled hostname.
fn reject_bad_host_or_origin(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    let hosts = allowed_hosts(&state.config);
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(|value| normalize_host(host_from_header(value)))
        .unwrap_or_default();
    if !hosts.contains(&host) {
        return Some(bad_host_or_origin());
    }
    if let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    {
        let scheme = origin
            .split_once("://")
            .map(|(scheme, _)| scheme)
            .unwrap_or("");
        if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
            return Some(bad_host_or_origin());
        }
        let Some(origin_host) = origin_host(origin) else {
            return Some(bad_host_or_origin());
        };
        let origin_host = normalize_host(origin_host);
        if !hosts.contains(&origin_host) {
            return Some(bad_host_or_origin());
        }
    }
    None
}

fn bad_host_or_origin() -> Response {
    json_response(
        StatusCode::BAD_REQUEST,
        &json!({
            "jsonrpc": "2.0",
            "id": Value::Null,
            "error": { "code": -32600, "message": "untrusted Host or Origin header" },
        }),
    )
}

/// Extracts the host portion of a `Host` header, keeping IPv6 brackets for
/// normalization to strip.
fn host_from_header(value: &str) -> &str {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix('[')
        && let Some(close) = rest.find(']')
    {
        return &value[..=1 + close];
    }
    value.rsplit_once(':').map_or(value, |(host, _)| host)
}

/// Extracts the host portion of an `Origin` header.
fn origin_host(origin: &str) -> Option<&str> {
    let rest = origin.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    Some(host_from_header(authority))
}

/// A stream that completes the TLS handshake on the first read or write, in
/// the per-connection task `axum::serve` spawns, instead of holding the accept
/// loop while a client stalls.
struct LazyTlsStream {
    state: TlsState,
}

enum TlsState {
    Handshaking(Pin<Box<dyn Future<Output = io::Result<TlsStream<tokio::net::TcpStream>>> + Send>>),
    Established(Box<TlsStream<tokio::net::TcpStream>>),
    Failed(String),
}

impl LazyTlsStream {
    fn new(stream: tokio::net::TcpStream, acceptor: TlsAcceptor) -> Self {
        let handshake = async move {
            match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                Ok(Ok(stream)) => Ok(stream),
                Ok(Err(error)) => Err(error),
                Err(_) => Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "TLS handshake timed out",
                )),
            }
        };
        Self {
            state: TlsState::Handshaking(Box::pin(handshake)),
        }
    }

    fn poll_ready(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<&mut TlsStream<tokio::net::TcpStream>>> {
        let this = self.get_mut();
        loop {
            if let TlsState::Established(ref mut stream) = this.state {
                return Poll::Ready(Ok(stream.as_mut()));
            }
            if let TlsState::Failed(ref message) = this.state {
                return Poll::Ready(Err(io::Error::other(message.clone())));
            }
            let state = std::mem::replace(&mut this.state, TlsState::Failed(String::new()));
            let TlsState::Handshaking(mut handshake) = state else {
                unreachable!("poll_ready reached a non-handshaking state");
            };
            match handshake.as_mut().poll(cx) {
                Poll::Ready(Ok(stream)) => {
                    this.state = TlsState::Established(Box::new(stream));
                }
                Poll::Ready(Err(error)) => {
                    let message = error.to_string();
                    this.state = TlsState::Failed(message.clone());
                    return Poll::Ready(Err(io::Error::other(message)));
                }
                Poll::Pending => {
                    this.state = TlsState::Handshaking(handshake);
                    return Poll::Pending;
                }
            }
        }
    }
}

impl AsyncRead for LazyTlsStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.poll_ready(cx) {
            Poll::Ready(Ok(stream)) => unsafe { Pin::new_unchecked(stream) }.poll_read(cx, buf),
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for LazyTlsStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.poll_ready(cx) {
            Poll::Ready(Ok(stream)) => unsafe { Pin::new_unchecked(stream) }.poll_write(cx, buf),
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match &mut this.state {
            TlsState::Established(stream) => {
                unsafe { Pin::new_unchecked(stream.as_mut()) }.poll_flush(cx)
            }
            _ => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match &mut this.state {
            TlsState::Established(stream) => {
                unsafe { Pin::new_unchecked(stream.as_mut()) }.poll_shutdown(cx)
            }
            _ => Poll::Ready(Ok(())),
        }
    }
}

/// A listening socket that accepts the raw TCP stream and defers the TLS
/// handshake to the per-connection task, so `axum::serve` handles HTTPS with
/// the same router as the plaintext listener.
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
    type Io = LazyTlsStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.listener.accept().await {
                Ok((stream, address)) => {
                    return (LazyTlsStream::new(stream, self.acceptor.clone()), address);
                }
                Err(error) => {
                    log::warn!("MCP TLS listener could not accept a connection: {error}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
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
    shutdown: watch::Sender<bool>,
}

impl RunningServer {
    /// Stops accepting new connections and lets in-flight requests finish.
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }
}

/// Validates the bind policy, builds and validates TLS before binding, binds,
/// and serves the router. The task runs until aborted or the listener fails.
pub async fn serve(config: Config) -> Result<RunningServer, String> {
    validate_bind(&config)?;
    let acceptor = match &config.tls {
        Some(tls) => Some(TlsAcceptor::from(server_tls_config(tls).map_err(
            |error| format!("could not build the MCP TLS listener: {error}"),
        )?)),
        None => None,
    };
    let listener = TcpListener::bind(config.bind).await.map_err(|error| {
        format!(
            "could not bind the MCP listener to {}: {error}",
            config.bind
        )
    })?;
    let local_addr = listener.local_addr().map_err(|error| error.to_string())?;
    let state = Arc::new(AppState {
        config: config.clone(),
        limits: HttpLimits::default(),
    });
    let router = app(state);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        let shutdown = async move {
            let mut rx = shutdown_rx;
            while !*rx.borrow() {
                if rx.changed().await.is_err() {
                    break;
                }
            }
        };
        let result = match acceptor {
            Some(acceptor) => {
                let listener = TlsListener::new(listener, acceptor);
                axum::serve(listener, router)
                    .with_graceful_shutdown(shutdown)
                    .await
            }
            None => {
                axum::serve(listener, router)
                    .with_graceful_shutdown(shutdown)
                    .await
            }
        };
        if let Err(error) = result {
            log::error!("MCP HTTP listener failed: {error}");
        }
    });
    Ok(RunningServer {
        local_addr,
        task,
        shutdown: shutdown_tx,
    })
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

async fn handle_mcp(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if !content_type_is_json(&headers) {
        return json_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            &json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": { "code": -32700, "message": "Content-Type must be application/json" },
            }),
        );
    }
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
    if messages.is_empty() {
        return json_response(
            StatusCode::OK,
            &json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": { "code": -32600, "message": "an empty batch is not a request" },
            }),
        );
    }
    if messages.len() > state.limits.max_batch_requests {
        return json_response(
            StatusCode::OK,
            &json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": { "code": -32600, "message": "batch is too large" },
            }),
        );
    }
    let permit = match state.limits.concurrent.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return json_response(
                StatusCode::TOO_MANY_REQUESTS,
                &json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": { "code": -32000, "message": "too many concurrent requests" },
                }),
            );
        }
    };
    let request_timeout = state.limits.request_timeout;
    let process = async move {
        let _permit = permit;
        process_messages(&state, &messages, is_batch).await
    };
    match tokio::time::timeout(request_timeout, process).await {
        Ok(response) => response,
        Err(_) => json_response(
            StatusCode::GATEWAY_TIMEOUT,
            &json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": { "code": -32000, "message": "request timed out" },
            }),
        ),
    }
}

async fn process_messages(state: &AppState, messages: &[Value], is_batch: bool) -> Response {
    let mut responses = Vec::new();
    for message in messages {
        if let Some(response) = jsonrpc::handle_message(state, message).await {
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

fn content_type_is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
}

fn json_response(status: StatusCode, value: &Value) -> Response {
    let mut response = (status, value.to_string()).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    response
}
