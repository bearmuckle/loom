use super::*;

#[derive(Clone)]
pub struct WebSocketTransport {
    pub(super) url: String,
    pub(super) bearer_token: String,
    pub(super) max_frame_bytes: usize,
    /// Rustls configuration that trusts the OS roots plus an operator-supplied
    /// CA. `None` uses the default OS trust roots.
    pub(super) tls_client_config: Option<Arc<rustls::ClientConfig>>,
    /// Explicit opt-in that permits plaintext `ws://` to a non-loopback host.
    pub(super) allow_insecure_remote: bool,
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

    pub fn url(&self) -> &str {
        &self.url
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
    pub(super) socket: tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    pub(super) max_frame_bytes: usize,
    /// Events read while a request was in flight, oldest first and bounded by
    /// [`CLIENT_EVENT_BUFFER_CAPACITY`].
    pub(super) events: VecDeque<ServerEventEnvelope>,
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
    pub(super) fn oversized_inbound_frame(
        &self,
        frame: &[u8],
        request_id: RequestId,
    ) -> Option<LoomError> {
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
    pub(super) fn buffer_event(&mut self, event: ServerEventEnvelope) {
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

    pub fn take_events(&mut self) -> Vec<ServerEventEnvelope> {
        self.events.drain(..).collect()
    }

    pub async fn cancel_request(&mut self, request_id: RequestId) -> Result<()> {
        let bytes = encode_client_frame(&ClientFrame::Cancel { request_id }).map_err(|error| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                format!("could not encode remote cancellation: {error}"),
                false,
            )
        })?;
        self.socket
            .send(ClientMessage::Text(
                String::from_utf8(bytes)
                    .map_err(|error| {
                        LoomError::new(
                            ErrorCode::MalformedPayload,
                            format!("remote cancellation was not UTF-8: {error}"),
                            false,
                        )
                    })?
                    .into(),
            ))
            .await
            .map_err(socket_error)
    }

    pub async fn next_event(&mut self) -> Result<Option<ServerEventEnvelope>> {
        if let Some(event) = self.events.pop_front() {
            return Ok(Some(event));
        }
        loop {
            let Some(message) = self.socket.next().await else {
                return Ok(None);
            };
            match message.map_err(socket_error)? {
                ClientMessage::Text(text) => {
                    if let Ok(event) = decode_event(text.as_bytes()) {
                        return Ok(Some(event));
                    }
                    return Err(LoomError::malformed_payload(
                        "remote text frame is not a Loom event",
                    ));
                }
                ClientMessage::Binary(bytes) => {
                    if let Ok(event) = decode_event(&bytes) {
                        return Ok(Some(event));
                    }
                    return Err(LoomError::malformed_payload(
                        "remote binary frame is not a Loom event",
                    ));
                }
                ClientMessage::Ping(payload) => {
                    self.socket
                        .send(ClientMessage::Pong(payload))
                        .await
                        .map_err(socket_error)?;
                }
                ClientMessage::Pong(_) => {}
                ClientMessage::Close(_) => return Ok(None),
                ClientMessage::Frame(_) => {}
            }
        }
    }

    pub async fn close(&mut self) -> Result<()> {
        self.socket
            .send(ClientMessage::Close(None))
            .await
            .map_err(socket_error)
    }
}

pub(super) fn socket_error(error: impl std::fmt::Display) -> LoomError {
    LoomError::new(
        ErrorCode::RequestCancelled,
        format!("WebSocket transport failed: {error}"),
        true,
    )
}

/// The host of a plaintext `ws://` endpoint that is not the local machine.
///
/// `wss://` endpoints carry TLS and loopback endpoints never leave the machine,
/// so neither needs the insecure-remote opt-in. An unparseable URL is left to
/// the connection attempt to reject.
pub(super) fn insecure_remote_host(url: &str) -> Option<String> {
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
pub(super) const TLS_FAILURE_MARKERS: [&str; 7] = [
    "invalid peer certificate",
    "certificate",
    "invalid dns name",
    "no certificates presented",
    "peer sent no certificates",
    "received fatal alert",
    "handshake failure",
];

pub(super) fn websocket_connect_error(error: TungsteniteError) -> LoomError {
    if matches!(
        error,
        TungsteniteError::Http(ref response)
            if response.status().as_u16() == StatusCode::UNAUTHORIZED.as_u16()
                || response.status().as_u16() == StatusCode::FORBIDDEN.as_u16()
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
pub(super) fn tls_failure_detail(error: &TungsteniteError) -> Option<String> {
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
