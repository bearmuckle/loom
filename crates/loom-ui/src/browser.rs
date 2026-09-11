//! Browser transport and startup configuration for the Loom UI.
//!
//! This module is compiled only for the `wasm32` target. It connects to a
//! running Loom remote backend (`loom-cli --serve`) over a real, in-browser
//! WebSocket transport (no threads, no blocking) and reads page-provided
//! connection settings from the URL query string, since a browser client has
//! no argv or process environment. The actual UI is the same `LoomView` used
//! natively (`crate::view`); this module only supplies the wasm-specific
//! transport it talks through.

use std::{cell::RefCell, collections::HashMap, rc::Rc};

use futures_channel::oneshot;
use loom_core::{ErrorCode, LoomError, RequestId};
use loom_model::ModelId;
use loom_protocol::{RequestEnvelope, ResponseEnvelope, decode_event, decode_response, encode_request};
use wasm_bindgen::{JsCast, prelude::*};
use web_sys::{CloseEvent, MessageEvent, UrlSearchParams, WebSocket};

/// Configuration read from the page's URL query string.
///
/// Example: `index.html?remote=ws://127.0.0.1:8791/ws&token=demo-token`.
pub(crate) struct BrowserOptions {
    remote: String,
    token: String,
    workspace: Option<String>,
    model: Option<ModelId>,
}

impl BrowserOptions {
    pub(crate) fn from_location() -> Result<Self, LoomError> {
        let window = web_sys::window()
            .ok_or_else(|| LoomError::invalid_request("no browser window is available"))?;
        let search = window.location().search().unwrap_or_default();
        let params = UrlSearchParams::new_with_str(&search).map_err(|_| {
            LoomError::invalid_request("could not parse the page's query string")
        })?;
        let remote = params.get("remote").ok_or_else(|| {
            LoomError::invalid_request(
                "the page URL must include ?remote=ws://host:port/ws for the browser client to connect",
            )
        })?;
        let token = params.get("token").ok_or_else(|| {
            LoomError::invalid_request("the page URL must include &token=<bearer token>")
        })?;
        Ok(Self {
            remote,
            token,
            workspace: params.get("workspace"),
            model: params.get("model").map(ModelId::new),
        })
    }

    pub(crate) fn remote(&self) -> &str {
        &self.remote
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    pub(crate) fn workspace(&self) -> Option<&str> {
        self.workspace.as_deref()
    }

    pub(crate) fn model(&self) -> Option<&ModelId> {
        self.model.as_ref()
    }
}

/// Appends the bearer token as a query parameter, since a browser
/// `WebSocket` cannot set an `Authorization` header during the handshake.
fn websocket_url(remote: &str, token: &str) -> String {
    let separator = if remote.contains('?') { '&' } else { '?' };
    format!(
        "{remote}{separator}access_token={}",
        js_sys::encode_uri_component(token)
    )
}

struct SocketState {
    socket: WebSocket,
    ready: bool,
    closed: Option<String>,
    pending: HashMap<RequestId, oneshot::Sender<ResponseEnvelope>>,
    outbox: Vec<String>,
}

fn fail_all(state: &Rc<RefCell<SocketState>>, reason: &str) {
    let mut state = state.borrow_mut();
    state.ready = false;
    state.closed = Some(reason.to_owned());
    for (request_id, sender) in state.pending.drain() {
        let _ = sender.send(ResponseEnvelope::failure(
            request_id,
            LoomError::new(ErrorCode::RequestCancelled, reason.to_owned(), true),
        ));
    }
}

/// A real WebSocket connection to a Loom remote backend, driven entirely by
/// the browser's event loop (no threads, no blocking).
#[derive(Clone)]
pub(crate) struct BrowserConnection {
    state: Rc<RefCell<SocketState>>,
    // The socket's callbacks borrow these closures for their lifetime; they
    // must stay alive as long as the connection does.
    _on_open: Rc<Closure<dyn FnMut(web_sys::Event)>>,
    _on_message: Rc<Closure<dyn FnMut(MessageEvent)>>,
    _on_error: Rc<Closure<dyn FnMut(web_sys::Event)>>,
    _on_close: Rc<Closure<dyn FnMut(CloseEvent)>>,
}

impl BrowserConnection {
    pub(crate) fn connect(remote: &str, token: &str) -> Result<Self, LoomError> {
        let socket = WebSocket::new(&websocket_url(remote, token)).map_err(|error| {
            LoomError::new(
                ErrorCode::Internal,
                format!("could not open a WebSocket to '{remote}': {}", describe_js(&error)),
                false,
            )
        })?;
        let state = Rc::new(RefCell::new(SocketState {
            socket: socket.clone(),
            ready: false,
            closed: None,
            pending: HashMap::new(),
            outbox: Vec::new(),
        }));

        let open_state = state.clone();
        let on_open = Closure::<dyn FnMut(_)>::new(move |_event: web_sys::Event| {
            let mut state = open_state.borrow_mut();
            state.ready = true;
            let outbox = std::mem::take(&mut state.outbox);
            for frame in outbox {
                let _ = state.socket.send_with_str(&frame);
            }
        });
        socket.set_onopen(Some(on_open.as_ref().unchecked_ref()));

        let message_state = state.clone();
        let on_message = Closure::<dyn FnMut(_)>::new(move |event: MessageEvent| {
            let Some(text) = event.data().as_string() else {
                return;
            };
            if let Ok(response) = decode_response(text.as_bytes()) {
                if let Some(sender) = message_state.borrow_mut().pending.remove(&response.request_id)
                {
                    let _ = sender.send(response);
                }
                return;
            }
            // Session events arrive unprompted while a run is active; the
            // browser client discovers them by polling `GetSessionEvents`
            // instead, so it only needs to avoid treating them as malformed.
            let _ = decode_event(text.as_bytes());
        });
        socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

        let error_state = state.clone();
        let on_error = Closure::<dyn FnMut(_)>::new(move |_event: web_sys::Event| {
            fail_all(&error_state, "the WebSocket connection reported an error");
        });
        socket.set_onerror(Some(on_error.as_ref().unchecked_ref()));

        let close_state = state.clone();
        let on_close = Closure::<dyn FnMut(_)>::new(move |event: CloseEvent| {
            let reason = if event.reason().is_empty() {
                format!("the connection closed (code {})", event.code())
            } else {
                format!("the connection closed: {}", event.reason())
            };
            fail_all(&close_state, &reason);
        });
        socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));

        Ok(Self {
            state,
            _on_open: Rc::new(on_open),
            _on_message: Rc::new(on_message),
            _on_error: Rc::new(on_error),
            _on_close: Rc::new(on_close),
        })
    }

    /// Sends a request and awaits its matching response. Requests issued
    /// before the socket finishes opening are queued and flushed on open.
    pub(crate) async fn request(&self, request: RequestEnvelope) -> ResponseEnvelope {
        let request_id = request.request_id;
        let frame = match encode_request(&request).map(String::from_utf8) {
            Ok(Ok(text)) => text,
            Ok(Err(error)) => {
                return ResponseEnvelope::failure(
                    request_id,
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        format!("request was not UTF-8: {error}"),
                        false,
                    ),
                );
            }
            Err(error) => {
                return ResponseEnvelope::failure(
                    request_id,
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        format!("could not encode request: {error}"),
                        false,
                    ),
                );
            }
        };

        let receiver = {
            let mut state = self.state.borrow_mut();
            if let Some(reason) = state.closed.clone() {
                return ResponseEnvelope::failure(
                    request_id,
                    LoomError::new(ErrorCode::RequestCancelled, reason, true),
                );
            }
            let (reply, receiver) = oneshot::channel();
            state.pending.insert(request_id, reply);
            if state.ready {
                let _ = state.socket.send_with_str(&frame);
            } else {
                state.outbox.push(frame);
            }
            receiver
        };

        receiver.await.unwrap_or_else(|_| {
            ResponseEnvelope::failure(
                request_id,
                LoomError::new(
                    ErrorCode::RequestCancelled,
                    "the browser connection closed before answering",
                    true,
                ),
            )
        })
    }
}

fn describe_js(value: &JsValue) -> String {
    value
        .as_string()
        .or_else(|| js_sys::Error::from(value.clone()).message().as_string())
        .unwrap_or_else(|| "unknown error".to_owned())
}
