//! Native WebSocket client transport for a Loom worker.
//!
//! This is the client half of the remote protocol, kept here so a native client
//! can reach a worker without linking the backend crates. It mirrors the
//! transport in `loom-server` (which keeps its own copy for the server's tests
//! and the CLI): frame-size limits, TLS with an optional extra CA, and the
//! plaintext-to-remote opt-in behave identically.

use std::{collections::VecDeque, fmt, fs, io, net::IpAddr, path::Path, sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use loom_core::{ErrorCode, LoomError, RequestId, Result};
use loom_protocol::{RequestEnvelope, ResponseEnvelope, ServerEventEnvelope, decode_event};
use tokio::time::timeout;
use tokio_tungstenite::{
    Connector, MaybeTlsStream, connect_async_tls_with_config,
    tungstenite::{
        Error as TungsteniteError, Message as ClientMessage, client::IntoClientRequest,
        http::header::AUTHORIZATION,
    },
};

/// The largest single WebSocket frame either side accepts by default (4 MiB).
pub const DEFAULT_MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
/// The flag that carries the frame-size limit, named in parse errors.
pub const MAX_FRAME_BYTES_FLAG: &str = "--max-frame-bytes";
/// Environment variable that sets the frame-size limit when the flag is absent.
pub const MAX_FRAME_BYTES_ENV: &str = "LOOM_MAX_FRAME_BYTES";
/// How long the client waits for the next frame while awaiting a response.
const CLIENT_FRAME_DEADLINE: Duration = Duration::from_secs(30);
/// Most events buffered while a request is in flight.
const CLIENT_EVENT_BUFFER_CAPACITY: usize = 1024;

/// Whether this build's WebSocket transport can negotiate TLS (`wss://`).
pub const WEBSOCKET_TLS_SUPPORTED: bool = true;

/// Parses a frame-size limit in bytes.
///
/// `source` names the flag or environment variable the value came from, so an
/// operator is told which input to fix.
pub fn parse_max_frame_bytes(value: &str, source: &str) -> Result<usize> {
    let bytes = value.trim().parse::<usize>().map_err(|_| {
        LoomError::invalid_request(format!(
            "{source} must be a whole number of bytes such as 4194304; got '{value}'"
        ))
    })?;
    if bytes == 0 {
        return Err(LoomError::invalid_request(format!(
            "{source} must be greater than zero"
        )));
    }
    Ok(bytes)
}

/// Resolves the frame-size limit from an explicit flag and its environment
/// fallback, defaulting to [`DEFAULT_MAX_FRAME_BYTES`].
///
/// An explicit flag always wins over the variable it falls back to, exactly
/// like the archive-retention inputs. The standalone server, `loom --serve`, and
/// the native client all resolve their limit here, so the flag and the variable
/// mean the same thing on either side of the transport.
pub fn resolve_max_frame_bytes(flag: Option<&str>, env: Option<&str>) -> Result<usize> {
    match (flag, env) {
        (Some(value), _) => parse_max_frame_bytes(value, MAX_FRAME_BYTES_FLAG),
        (None, Some(value)) => parse_max_frame_bytes(value, MAX_FRAME_BYTES_ENV),
        (None, None) => Ok(DEFAULT_MAX_FRAME_BYTES),
    }
}

/// The diagnostic for a frame above the configured limit.
///
/// It names what the frame was, the request it belongs to, the received size,
/// and the configured limit, because the failure is a size bound rather than
/// corruption and the caller cannot otherwise tell which result to narrow.
fn frame_too_large_error(
    context: &str,
    request_id: RequestId,
    received: usize,
    limit: usize,
) -> LoomError {
    LoomError::payload_too_large(format!(
        "{context} of {received} bytes for request {request_id} is above the configured frame \
         size limit of {limit} bytes"
    ))
}

/// Decides which request an inbound frame above the configured limit fails.
///
/// A frame that decodes as the response the caller awaits belongs to that
/// request, so only that request fails. Anything else cannot be that answer, so
/// it is drained and the wait continues; see
/// [`WebSocketConnection::oversized_inbound_frame`] for the rule and its
/// reasoning.
fn oversized_inbound_frame_action(frame: &[u8], request_id: RequestId) -> OversizedFrameAction {
    match loom_protocol::decode_response(frame) {
        Ok(response) if response.request_id == request_id => {
            OversizedFrameAction::FailAwaitedRequest
        }
        Ok(_) | Err(_) => OversizedFrameAction::Drain,
    }
}

/// How an over-limit inbound frame is treated while a request is awaited.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OversizedFrameAction {
    /// Fail the awaited request with the payload-too-large diagnostic.
    FailAwaitedRequest,
    /// Drop the frame and keep waiting for the awaited response.
    Drain,
}
#[derive(Clone)]
pub struct WebSocketTransport {
    url: String,
    bearer_token: String,
    max_frame_bytes: usize,
    /// Rustls configuration that trusts the OS roots plus an operator-supplied
    /// CA. `None` uses the default OS trust roots.
    tls_client_config: Option<Arc<rustls::ClientConfig>>,
    /// Explicit opt-in that permits plaintext `ws://` to a non-loopback host.
    allow_insecure_remote: bool,
}

impl fmt::Debug for WebSocketTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebSocketTransport")
            .field("url", &self.url)
            .field("bearer_token", &"<redacted>")
            .field("max_frame_bytes", &self.max_frame_bytes)
            .field("ca_certificate", &self.tls_client_config.is_some())
            .field("allow_insecure_remote", &self.allow_insecure_remote)
            .finish()
    }
}

impl WebSocketTransport {
    pub fn new(url: impl Into<String>, bearer_token: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            bearer_token: bearer_token.into(),
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            tls_client_config: None,
            allow_insecure_remote: false,
        }
    }

    pub fn with_max_frame_bytes(mut self, max_frame_bytes: usize) -> Result<Self> {
        if max_frame_bytes == 0 {
            return Err(LoomError::invalid_request(
                "WebSocket frame size must be greater than zero",
            ));
        }
        self.max_frame_bytes = max_frame_bytes;
        Ok(self)
    }

    /// Trusts the certificates in this PEM document in addition to the OS trust
    /// roots. The supplied CA is never a substitute for the OS roots, so an
    /// unrelated certificate cannot be accepted on its behalf.
    pub fn with_ca_certificate_pem(mut self, certificate_pem: impl AsRef<[u8]>) -> Result<Self> {
        self.tls_client_config = Some(client_tls_config(certificate_pem.as_ref())?);
        Ok(self)
    }

    /// Trusts the certificates in this PEM file in addition to the OS trust
    /// roots.
    pub fn with_ca_certificate_file(self, certificate: impl AsRef<Path>) -> Result<Self> {
        let certificate = certificate.as_ref();
        let pem = fs::read(certificate).map_err(|error| {
            LoomError::invalid_request(format!(
                "could not read the TLS CA file '{}': {error}",
                certificate.display()
            ))
        })?;
        self.with_ca_certificate_pem(pem)
    }

    /// Allows plaintext `ws://` to a host that is not the local machine. The
    /// bearer token and every protocol frame travel in the clear, so this must
    /// be an explicit operator decision; the connection warns when it is used.
    pub fn with_insecure_remote_allowed(mut self, allow_insecure_remote: bool) -> Self {
        self.allow_insecure_remote = allow_insecure_remote;
        self
    }

    pub async fn connect(&self) -> Result<WebSocketConnection> {
        if let Some(host) = insecure_remote_host(&self.url) {
            if !self.allow_insecure_remote {
                return Err(LoomError::invalid_request(format!(
                    "refusing to send the worker access token over plaintext ws:// to '{host}'; \
                     use wss:// with a trusted certificate, connect to a loopback worker, or \
                     pass --allow-insecure-remote to accept an unencrypted connection"
                )));
            }
            log::warn!(
                "connecting to '{host}' over plaintext ws:// because insecure remote access was \
                 explicitly allowed; the worker access token and all protocol traffic are not \
                 protected in transit"
            );
        }
        let mut request = self.url.clone().into_client_request().map_err(|_| {
            LoomError::invalid_request(
                "invalid WebSocket URL; provide a valid ws:// or wss:// endpoint",
            )
        })?;
        let value = format!("Bearer {}", self.bearer_token)
            .parse()
            .map_err(|_| LoomError::invalid_request("bearer token contains invalid characters"))?;
        request.headers_mut().insert(AUTHORIZATION, value);
        let connector = self.tls_client_config.clone().map(Connector::Rustls);
        let (socket, _) = connect_async_tls_with_config(request, None, false, connector)
            .await
            .map_err(websocket_connect_error)?;
        Ok(WebSocketConnection {
            socket,
            max_frame_bytes: self.max_frame_bytes,
            events: VecDeque::new(),
        })
    }
}

pub struct WebSocketConnection {
    socket: tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    max_frame_bytes: usize,
    /// Events read while a request was in flight, oldest first and bounded by
    /// [`CLIENT_EVENT_BUFFER_CAPACITY`].
    events: VecDeque<ServerEventEnvelope>,
}

impl WebSocketConnection {
    pub async fn request(&mut self, request: RequestEnvelope) -> Result<ResponseEnvelope> {
        let request_id = request.request_id;
        let bytes = loom_protocol::encode_request(&request).map_err(|error| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                format!("could not encode remote request: {error}"),
                false,
            )
        })?;
        if bytes.len() > self.max_frame_bytes {
            return Err(frame_too_large_error(
                "remote request",
                request_id,
                bytes.len(),
                self.max_frame_bytes,
            ));
        }
        self.socket
            .send(ClientMessage::Text(
                String::from_utf8(bytes)
                    .map_err(|error| {
                        LoomError::new(
                            ErrorCode::MalformedPayload,
                            format!("remote request was not UTF-8: {error}"),
                            false,
                        )
                    })?
                    .into(),
            ))
            .await
            .map_err(socket_error)?;
        loop {
            // A half-open socket never yields a frame, so each individual wait
            // is bounded instead of blocking the calling thread forever.
            let next = timeout(CLIENT_FRAME_DEADLINE, self.socket.next()).await;
            let message = match next {
                Ok(Some(message)) => message,
                Ok(None) => {
                    return Err(LoomError::new(
                        ErrorCode::RequestCancelled,
                        "remote server disconnected",
                        true,
                    ));
                }
                Err(_) => {
                    return Err(LoomError::new(
                        ErrorCode::DeadlineExceeded,
                        "remote server sent no frame within the read deadline; the connection may \
                         be stalled",
                        true,
                    ));
                }
            };
            match message.map_err(socket_error)? {
                ClientMessage::Text(text) => {
                    if text.len() > self.max_frame_bytes {
                        // The check runs before decoding, so it sees every frame
                        // on the socket, including events and other requests'
                        // responses. An over-limit frame therefore only fails
                        // the request it decodes as the response for; anything
                        // else is drained and the wait continues, so concurrent
                        // traffic cannot fail a request that did not cause it.
                        if let Some(error) =
                            self.oversized_inbound_frame(text.as_bytes(), request_id)
                        {
                            return Err(error);
                        }
                        continue;
                    }
                    if let Ok(response) = loom_protocol::decode_response(text.as_bytes()) {
                        if response.request_id == request_id {
                            return Ok(response);
                        }
                        continue;
                    }
                    if let Ok(event) = decode_event(text.as_bytes()) {
                        self.buffer_event(event);
                        continue;
                    }
                    return Err(LoomError::malformed_payload(
                        "remote text frame is not a Loom response or event",
                    ));
                }
                ClientMessage::Binary(bytes) => {
                    if bytes.len() > self.max_frame_bytes {
                        // Same rule as the text arm above.
                        if let Some(error) = self.oversized_inbound_frame(&bytes, request_id) {
                            return Err(error);
                        }
                        continue;
                    }
                    if let Ok(response) = loom_protocol::decode_response(&bytes) {
                        if response.request_id == request_id {
                            return Ok(response);
                        }
                        continue;
                    }
                    if let Ok(event) = decode_event(&bytes) {
                        self.buffer_event(event);
                        continue;
                    }
                    return Err(LoomError::malformed_payload(
                        "remote binary frame is not a Loom response or event",
                    ));
                }
                ClientMessage::Ping(payload) => {
                    self.socket
                        .send(ClientMessage::Pong(payload))
                        .await
                        .map_err(socket_error)?;
                }
                ClientMessage::Pong(_) => {}
                ClientMessage::Close(_) => {
                    return Err(LoomError::new(
                        ErrorCode::RequestCancelled,
                        "remote server closed the connection",
                        true,
                    ));
                }
                ClientMessage::Frame(_) => {}
            }
        }
    }

    /// Decides what an inbound frame above the configured limit does to the
    /// request being awaited.
    ///
    /// The size check runs before decoding, so it applies to every frame on the
    /// socket, including events and other requests' responses. Deciding by
    /// content keeps an over-limit frame from failing a request it has nothing
    /// to do with:
    ///
    /// - a frame that decodes as the awaited response fails exactly that
    ///   request with [`ErrorCode::PayloadTooLarge`], naming the received size,
    ///   the configured limit, and the request, because its answer can never be
    ///   delivered;
    /// - anything else — an event, another request's response, or a frame that
    ///   does not decode at all — is drained and the wait continues. A drained
    ///   event is dropped rather than buffered, because buffering it would
    ///   defeat the limit; events carry a sequence, so a caller that drains the
    ///   stream can detect the gap.
    ///
    /// Returns the error the awaited request fails with, or `None` when the
    /// frame was drained.
    fn oversized_inbound_frame(&self, frame: &[u8], request_id: RequestId) -> Option<LoomError> {
        match oversized_inbound_frame_action(frame, request_id) {
            OversizedFrameAction::FailAwaitedRequest => Some(frame_too_large_error(
                "remote response",
                request_id,
                frame.len(),
                self.max_frame_bytes,
            )),
            OversizedFrameAction::Drain => {
                log::warn!(
                    "dropped a remote frame of {} bytes while waiting for request {request_id}: it \
                     is above the configured frame size limit of {} bytes and is not that \
                     request's response",
                    frame.len(),
                    self.max_frame_bytes
                );
                None
            }
        }
    }

    /// Buffers an event read while a request was in flight.
    ///
    /// The buffer is capped: when a session streams faster than the caller
    /// drains it, the oldest events are dropped so the process cannot grow
    /// without limit. Events carry a sequence, so a resuming caller can detect
    /// the gap and re-sync instead of silently skipping it.
    fn buffer_event(&mut self, event: ServerEventEnvelope) {
        let capacity = CLIENT_EVENT_BUFFER_CAPACITY;
        if self.events.len() >= capacity {
            let dropped = self.events.len() - capacity / 2;
            self.events.drain(..dropped);
            log::warn!(
                "dropped {dropped} buffered remote events: more than {capacity} events arrived \
                 while a request was in flight"
            );
        }
        self.events.push_back(event);
    }

    pub async fn close(&mut self) -> Result<()> {
        self.socket
            .send(ClientMessage::Close(None))
            .await
            .map_err(socket_error)
    }
}

fn socket_error(error: impl std::fmt::Display) -> LoomError {
    LoomError::new(
        ErrorCode::RequestCancelled,
        format!("WebSocket transport failed: {error}"),
        true,
    )
}

/// Builds a rustls client configuration that trusts the OS roots and, in
/// addition, every certificate in `ca_pem`.
///
/// The supplied CA is added to the OS roots rather than replacing them, so
/// trusting a private CA never widens trust to unrelated certificates that the
/// OS already rejects.
fn client_tls_config(ca_pem: &[u8]) -> Result<Arc<rustls::ClientConfig>> {
    let mut roots = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    if !native.errors.is_empty() {
        log::warn!(
            "could not load every OS root certificate: {:?}",
            native.errors
        );
    }
    let (native_added, native_ignored) = roots.add_parsable_certificates(native.certs);
    log::debug!("trusting {native_added} OS root certificate(s); ignored {native_ignored}");
    let mut reader = io::BufReader::new(ca_pem);
    let certificates = rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| {
            LoomError::invalid_request(format!("could not parse the TLS CA PEM: {error}"))
        })?;
    if certificates.is_empty() {
        return Err(LoomError::invalid_request(
            "the TLS CA PEM contains no certificates",
        ));
    }
    let (ca_added, _) = roots.add_parsable_certificates(certificates);
    if ca_added == 0 {
        return Err(LoomError::invalid_request(
            "the TLS CA PEM contains no usable certificates",
        ));
    }
    Ok(Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}

/// The host of a plaintext `ws://` endpoint that is not the local machine.
///
/// `wss://` endpoints carry TLS and loopback endpoints never leave the machine,
/// so neither needs the insecure-remote opt-in. An unparseable URL is left to
/// the connection attempt to reject.
fn insecure_remote_host(url: &str) -> Option<String> {
    let uri = url
        .parse::<tokio_tungstenite::tungstenite::http::Uri>()
        .ok()?;
    if !uri
        .scheme_str()
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("ws"))
    {
        return None;
    }
    let host = uri.host()?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
    {
        return None;
    }
    Some(host.to_owned())
}

/// Substrings that identify a rustls certificate or handshake failure, which
/// tokio-tungstenite reports as an opaque I/O error.
const TLS_FAILURE_MARKERS: [&str; 7] = [
    "invalid peer certificate",
    "certificate",
    "invalid dns name",
    "no certificates presented",
    "peer sent no certificates",
    "received fatal alert",
    "handshake failure",
];

fn websocket_connect_error(error: TungsteniteError) -> LoomError {
    if matches!(
        error,
        TungsteniteError::Http(ref response)
            if response.status().as_u16() == 401 || response.status().as_u16() == 403
    ) {
        LoomError::new(
            ErrorCode::AuthenticationFailed,
            "WebSocket authentication was rejected",
            false,
        )
    } else if matches!(
        error,
        TungsteniteError::Url(
            tokio_tungstenite::tungstenite::error::UrlError::TlsFeatureNotEnabled
        )
    ) {
        LoomError::new(
            ErrorCode::Internal,
            "this WebSocket client was built without TLS support and cannot use a wss:// endpoint; \
             connect to a loopback worker over ws:// or install a build with the rustls TLS feature",
            false,
        )
    } else if let Some(detail) = tls_failure_detail(&error) {
        LoomError::new(
            ErrorCode::Internal,
            format!(
                "TLS handshake with the worker failed ({detail}); verify that the worker's \
                 certificate is valid for its hostname and trust its issuing CA with --ca or \
                 LOOM_TLS_CA"
            ),
            false,
        )
    } else if error.to_string().to_ascii_lowercase().contains("timed out") {
        LoomError::new(
            ErrorCode::DeadlineExceeded,
            "worker connection timed out",
            true,
        )
    } else if error
        .to_string()
        .to_ascii_lowercase()
        .contains("connection refused")
    {
        LoomError::new(
            ErrorCode::ProviderUnavailable,
            "connection refused by remote worker",
            true,
        )
    } else {
        LoomError::new(
            ErrorCode::Internal,
            "could not open the worker WebSocket; check the URL and network access",
            true,
        )
    }
}

/// The rustls failure behind a connection error, when there is one.
fn tls_failure_detail(error: &TungsteniteError) -> Option<String> {
    match error {
        TungsteniteError::Tls(tls) => Some(tls.to_string()),
        TungsteniteError::Io(io_error) => {
            let message = io_error.to_string();
            let lowercase = message.to_ascii_lowercase();
            TLS_FAILURE_MARKERS
                .iter()
                .any(|marker| lowercase.contains(marker))
                .then_some(message)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use loom_protocol::{ClientRequest, ControlRequest, encode_response};

    use super::*;

    const TEST_CA_PEM: &[u8] = include_bytes!("../../loom-server/tests/fixtures/tls/ca-cert.pem");

    #[test]
    fn the_frame_limit_resolves_from_a_flag_then_the_environment() {
        assert_eq!(
            resolve_max_frame_bytes(None, None).unwrap(),
            DEFAULT_MAX_FRAME_BYTES
        );
        assert_eq!(resolve_max_frame_bytes(Some(" 8192 "), None).unwrap(), 8192);
        assert_eq!(resolve_max_frame_bytes(None, Some("4096")).unwrap(), 4096);
        assert_eq!(
            resolve_max_frame_bytes(Some("8192"), Some("4096")).unwrap(),
            8192
        );
        let error = resolve_max_frame_bytes(None, Some("many")).unwrap_err();
        assert!(error.message.contains(MAX_FRAME_BYTES_ENV), "{error}");
        let error = resolve_max_frame_bytes(Some("0"), None).unwrap_err();
        assert!(error.message.contains(MAX_FRAME_BYTES_FLAG), "{error}");
        assert!(
            WebSocketTransport::new("ws://127.0.0.1:1/ws", "token")
                .with_max_frame_bytes(0)
                .is_err()
        );
    }

    #[test]
    fn oversized_inbound_frames_fail_only_the_request_they_answer() {
        let awaited = RequestId::new();
        let response = encode_response(&ResponseEnvelope::failure(
            awaited,
            LoomError::invalid_request("await"),
        ))
        .unwrap();
        assert_eq!(
            oversized_inbound_frame_action(&response, awaited),
            OversizedFrameAction::FailAwaitedRequest
        );
        assert_eq!(
            oversized_inbound_frame_action(&response, RequestId::new()),
            OversizedFrameAction::Drain
        );
        assert_eq!(
            oversized_inbound_frame_action(b"{not-json", awaited),
            OversizedFrameAction::Drain
        );
    }

    #[test]
    fn plaintext_remote_hosts_need_the_insecure_opt_in() {
        assert_eq!(
            insecure_remote_host("ws://worker.example:8765/ws").as_deref(),
            Some("worker.example")
        );
        assert_eq!(insecure_remote_host("wss://worker.example/ws"), None);
        assert_eq!(insecure_remote_host("ws://localhost:8765/ws"), None);
        assert_eq!(insecure_remote_host("ws://[::1]:8765/ws"), None);
        assert_eq!(insecure_remote_host("not a websocket URL"), None);
    }

    #[test]
    fn connect_errors_are_classified() {
        for (message, code) in [
            ("timed out", ErrorCode::DeadlineExceeded),
            ("connection refused", ErrorCode::ProviderUnavailable),
            ("unexpected network failure", ErrorCode::Internal),
        ] {
            assert_eq!(
                websocket_connect_error(TungsteniteError::Io(io::Error::other(message))).code,
                code
            );
        }
        let disabled = websocket_connect_error(TungsteniteError::Url(
            tokio_tungstenite::tungstenite::error::UrlError::TlsFeatureNotEnabled,
        ));
        assert!(disabled.message.contains("without TLS support"));
        let issuer = websocket_connect_error(TungsteniteError::Io(io::Error::other(
            "invalid peer certificate: UnknownIssuer",
        )));
        assert!(issuer.message.contains("TLS handshake"), "{issuer}");
    }

    #[test]
    fn ca_certificates_are_validated() {
        assert!(client_tls_config(TEST_CA_PEM).is_ok());
        assert!(client_tls_config(b"").is_err());
        let transport = WebSocketTransport::new("wss://worker.example/ws", "token")
            .with_ca_certificate_pem(TEST_CA_PEM)
            .unwrap();
        assert!(format!("{transport:?}").contains("<redacted>"));
        assert!(!format!("{transport:?}").contains("token\""));
    }

    /// Requests, oversized frames, events, and close against a real server.
    #[test]
    fn requests_round_trip_against_a_real_server() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let auth = Arc::new(loom_server::AuthTokenStore::new());
            let token = auth
                .insert("transport-test", loom_server::AuthorizationScope::all())
                .unwrap();
            let server = loom_server::RemoteServer::new(
                loom_server::InProcessBackend::new(),
                auth,
                loom_server::RemoteServerConfig::local_ephemeral(),
            )
            .bind()
            .await
            .unwrap();
            let request = || {
                RequestEnvelope::new(ClientRequest::Control(ControlRequest::DiscoverCapabilities))
            };

            let denied = WebSocketTransport::new(server.websocket_url(), "wrong")
                .connect()
                .await
                .err()
                .expect("a wrong token must be refused");
            assert_eq!(denied.code, ErrorCode::AuthenticationFailed);

            let mut connection = WebSocketTransport::new(server.websocket_url(), &token.token)
                .connect()
                .await
                .unwrap();
            assert!(connection.request(request()).await.unwrap().result.is_ok());
            connection.close().await.unwrap();

            let mut small = WebSocketTransport::new(server.websocket_url(), &token.token)
                .with_max_frame_bytes(256)
                .unwrap()
                .connect()
                .await
                .unwrap();
            let error = small.request(request()).await.unwrap_err();
            assert_eq!(error.code, ErrorCode::PayloadTooLarge);
            server.stop().await.unwrap();
        });
    }
}
