use std::{
    collections::{HashMap, VecDeque},
    fmt, fs, io,
    net::{IpAddr, SocketAddr},
    path::Path,
    sync::Arc,
    time::Duration,
};

use axum::{
    Router,
    extract::{
        State,
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
    },
    http::{
        HeaderMap, StatusCode,
        header::{AUTHORIZATION, SEC_WEBSOCKET_PROTOCOL},
    },
    response::{IntoResponse, Response},
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use loom_core::{ErrorCode, LoomError, RequestId, Result};
use loom_protocol::{
    ClientFrame, ClientRequest, ProjectRequest, RequestEnvelope, ResponseEnvelope, RunRequest,
    ServerEventEnvelope, TaskRequest, TerminalRequest, decode_client_frame, decode_event,
    decode_request, encode_client_frame, encode_response,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    task::{JoinHandle, spawn_blocking},
    time::{Instant, interval, timeout},
};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::{
    Connector, MaybeTlsStream, connect_async_tls_with_config,
    tungstenite::{Error as TungsteniteError, Message as ClientMessage, client::IntoClientRequest},
};

use super::{AuthTokenStore, GitHubSignInConfig, InProcessBackend, InProcessConnection};

const DEFAULT_WS_PATH: &str = "/ws";
const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(15);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// The largest single WebSocket frame either side accepts, in bytes (4 MiB).
///
/// The protocol has no chunking, so a result that grows with workspace or
/// content size can cross this bound. Both sides then report
/// [`ErrorCode::PayloadTooLarge`] naming the frame size, the configured limit,
/// and the request, instead of implying that the frame was malformed. Raise the
/// bound with [`MAX_FRAME_BYTES_FLAG`] or [`MAX_FRAME_BYTES_ENV`] on a server and
/// on the clients that reach it, or per connection with
/// [`WebSocketTransport::with_max_frame_bytes`].
pub const DEFAULT_MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
/// The flag that carries the frame-size limit, named in parse errors.
pub const MAX_FRAME_BYTES_FLAG: &str = "--max-frame-bytes";
/// Environment variable that sets the frame-size limit when the flag is absent.
///
/// A server start and a native client resolve their limit from the same input,
/// so one value bounds both sides of the remote transport.
pub const MAX_FRAME_BYTES_ENV: &str = "LOOM_MAX_FRAME_BYTES";
/// WebSocket close code 1009, "message too big".
///
/// An oversized frame that names no request is closed with this code and a
/// reason carrying the same size-and-limit explanation, because there is no
/// request id to answer under.
const CLOSE_MESSAGE_TOO_BIG: u16 = 1009;
/// Close code sent when a person's session access is revoked.
const CLOSE_POLICY_VIOLATION: u16 = 1008;
const DEFAULT_OUTGOING_CAPACITY: usize = 128;
/// Most ordinary requests one connection may have in flight at once.
///
/// Every received frame runs on a blocking thread, and Tokio's blocking queue
/// is unbounded, so without an explicit bound a client that pipelines frames
/// can queue unlimited blocking work and occupy the pool that other clients'
/// control and interrupt requests need. The frame that finds the limit reached
/// is refused with [`ErrorCode::Backpressure`] naming the limit; see
/// [`InFlightLimits`]. Set the limit with
/// [`RemoteServerConfig::max_in_flight_requests`].
pub const DEFAULT_MAX_IN_FLIGHT_REQUESTS: usize = 8;
/// Most control and interrupt requests one connection may have in flight at
/// once, counted separately from [`DEFAULT_MAX_IN_FLIGHT_REQUESTS`].
///
/// These requests are how a client observes a worker and stops work that is
/// already running, so they draw on their own permits: pipelined ordinary
/// requests cannot refuse or delay them. Set the limit with
/// [`RemoteServerConfig::max_in_flight_control_requests`]. See
/// [`is_control_or_interrupt_request`].
pub const DEFAULT_MAX_IN_FLIGHT_CONTROL_REQUESTS: usize = 4;
/// How long an incoming TLS handshake may take before the listener drops it.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the client waits for the next frame while it is waiting for a
/// response.
///
/// The server heartbeats every [`DEFAULT_HEARTBEAT`] and answers every request
/// within its own configured request timeout, so a longer silence means the
/// socket is half-open rather than merely busy. Without this bound a stalled
/// socket left the calling thread waiting forever.
const CLIENT_FRAME_DEADLINE: Duration = Duration::from_secs(30);
/// Most events buffered while a request is in flight.
///
/// Events that arrive while a request is being answered are buffered for the
/// event stream, so the buffer is capped instead of growing with a session that
/// streams faster than the caller drains it.
const CLIENT_EVENT_BUFFER_CAPACITY: usize = 1024;
/// Most connections one listener accepts without a credential at once.
///
/// Such a connection is waiting for a person to sign in with GitHub. It holds a
/// blocking slot and an open socket, so the listener bounds how many can wait
/// at the same time instead of letting an unauthenticated client exhaust them.
/// Set the bound with [`RemoteServerConfig::max_anonymous_connections`].
pub const DEFAULT_MAX_ANONYMOUS_CONNECTIONS: usize = 32;
/// How long a connection accepted without a credential may wait to sign in.
///
/// The device flow itself expires after 15 minutes, so a connection still
/// unsigned after this window is closed instead of holding a slot. Set the
/// bound with [`RemoteServerConfig::sign_in_timeout`].
pub const DEFAULT_SIGN_IN_TIMEOUT: Duration = Duration::from_secs(600);

/// Whether this build's WebSocket transport can negotiate TLS (`wss://`).
///
/// The native transport enables `tokio-tungstenite`'s rustls features, so a
/// `wss://` endpoint is genuinely encrypted and can be verified against the OS
/// trust roots plus an operator-supplied CA. Clients report this capability
/// instead of trusting the URL scheme, so a build that cannot do TLS never
/// claims a `wss://` endpoint is secure.
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

/// The close reason for an oversized frame that names no request.
///
/// A WebSocket close reason is limited to 123 bytes, so this carries the two
/// numbers and nothing else.
fn frame_too_large_close_reason(received: usize, limit: usize) -> String {
    format!("frame of {received} bytes over the configured {limit}-byte limit")
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

mod client;
mod server;
#[cfg(test)]
mod tests;
mod tls;

pub use client::{WebSocketConnection, WebSocketTransport};
pub use server::{RemoteServer, RemoteServerConfig, RunningRemoteServer};
pub use tls::ServerTlsConfig;

#[cfg(test)]
use client::*;
#[cfg(test)]
use server::*;
use tls::*;
