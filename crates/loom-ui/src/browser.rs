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
use loom_protocol::{
    RequestEnvelope, ResponseEnvelope, decode_event, decode_response, encode_request,
};
use wasm_bindgen::{JsCast, prelude::*};
use web_sys::{CloseEvent, MessageEvent, Storage, UrlSearchParams, WebSocket};

const SAVED_CONNECTION_KEY: &str = "loom.bootstrap_connection";
const SAVED_DEFAULT_MODEL_KEY: &str = "loom.default_model";

/// Configuration read from the page's URL query string.
///
/// Example: `index.html?remote=ws://127.0.0.1:8791/ws&token=demo-token`.
pub(crate) struct BrowserOptions {
    remote: String,
    token: String,
    workspace: Option<String>,
    model: Option<ModelId>,
    demo: bool,
}

impl BrowserOptions {
    pub(crate) fn empty() -> Self {
        Self {
            remote: String::new(),
            token: String::new(),
            workspace: None,
            model: None,
            demo: false,
        }
    }

    pub(crate) fn from_connection(
        remote: String,
        token: String,
        workspace: Option<String>,
        model: Option<ModelId>,
    ) -> Self {
        Self {
            remote,
            token,
            workspace,
            model,
            demo: false,
        }
    }

    pub(crate) fn from_location() -> Result<Self, LoomError> {
        let window = web_sys::window()
            .ok_or_else(|| LoomError::invalid_request("no browser window is available"))?;
        let search = window.location().search().unwrap_or_default();
        let params = UrlSearchParams::new_with_str(&search)
            .map_err(|_| LoomError::invalid_request("could not parse the page's query string"))?;
        let demo = params.get("demo").is_some_and(|value| {
            value.is_empty() || matches!(value.as_str(), "1" | "true" | "yes")
        });
        if demo {
            return Ok(Self {
                remote: String::new(),
                token: String::new(),
                workspace: None,
                model: Some(ModelId::new("deterministic/demo")),
                demo: true,
            });
        }
        let query_remote = params.get("remote").unwrap_or_default();
        let query_token = params.get("token").unwrap_or_default();
        let storage = browser_storage()?;
        let (saved_remote, saved_token) = if query_remote.is_empty() || query_token.is_empty() {
            saved_connection(&storage)?
        } else {
            (None, None)
        };
        let saved_model = saved_default_model(&storage)?;
        let reuse_saved_token =
            query_remote.is_empty() || saved_remote.as_deref() == Some(query_remote.as_str());
        Ok(Self {
            remote: if query_remote.is_empty() {
                saved_remote.unwrap_or_default()
            } else {
                query_remote
            },
            token: if query_token.is_empty() && reuse_saved_token {
                saved_token.unwrap_or_default()
            } else if query_token.is_empty() {
                String::new()
            } else {
                query_token
            },
            workspace: None,
            model: saved_model,
            demo: false,
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

    pub(crate) fn demo(&self) -> bool {
        self.demo
    }

    pub(crate) fn is_configured(&self) -> bool {
        !self.remote.trim().is_empty() && !self.token.trim().is_empty()
    }

    pub(crate) fn persist_connection(&self) -> Result<(), LoomError> {
        let storage = browser_storage()?;
        let value = serde_json::json!({
            "remote": self.remote,
            "token": self.token,
        });
        let serialized = serde_json::to_string(&value).map_err(|error| {
            LoomError::new(
                ErrorCode::Persistence,
                format!("could not encode the bootstrap worker config: {error}"),
                false,
            )
        })?;
        storage
            .set_item(SAVED_CONNECTION_KEY, &serialized)
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::Persistence,
                    format!(
                        "could not save the bootstrap worker config in browser storage: {}",
                        describe_js(&error)
                    ),
                    false,
                )
            })
    }

    pub(crate) fn persist_default_model(model: &ModelId) -> Result<(), LoomError> {
        browser_storage()?
            .set_item(SAVED_DEFAULT_MODEL_KEY, model.as_str())
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::Persistence,
                    format!(
                        "could not save the default model in browser storage: {}",
                        describe_js(&error)
                    ),
                    false,
                )
            })
    }
}

fn saved_default_model(storage: &Storage) -> Result<Option<ModelId>, LoomError> {
    storage
        .get_item(SAVED_DEFAULT_MODEL_KEY)
        .map(|model| model.map(ModelId::new))
        .map_err(|error| {
            LoomError::new(
                ErrorCode::Persistence,
                format!(
                    "could not read the saved default model from browser storage: {}",
                    describe_js(&error)
                ),
                false,
            )
        })
}

fn saved_connection(storage: &Storage) -> Result<(Option<String>, Option<String>), LoomError> {
    let serialized = storage.get_item(SAVED_CONNECTION_KEY).map_err(|error| {
        LoomError::new(
            ErrorCode::Persistence,
            format!(
                "could not read the saved bootstrap worker config: {}",
                describe_js(&error)
            ),
            false,
        )
    })?;
    let Some(serialized) = serialized else {
        return Ok((None, None));
    };
    let value: serde_json::Value = serde_json::from_str(&serialized).map_err(|error| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            format!("saved bootstrap worker config is malformed: {error}"),
            false,
        )
    })?;
    let object = value.as_object().ok_or_else(|| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            "saved bootstrap worker config must be a JSON object",
            false,
        )
    })?;
    let remote = object
        .get("remote")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let token = object
        .get("token")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    Ok((remote, token))
}

fn browser_storage() -> Result<Storage, LoomError> {
    web_sys::window()
        .ok_or_else(|| LoomError::invalid_request("no browser window is available"))?
        .local_storage()
        .map_err(|error| {
            LoomError::new(
                ErrorCode::Persistence,
                format!("could not access browser storage: {}", describe_js(&error)),
                false,
            )
        })?
        .ok_or_else(|| {
            LoomError::new(
                ErrorCode::Persistence,
                "browser local storage is unavailable",
                false,
            )
        })
}

/// Prefix for the bearer token subprotocol. A browser `WebSocket` cannot set an
/// `Authorization` header during the handshake, so the token travels in
/// `Sec-WebSocket-Protocol` instead of the URL.
const BEARER_SUBPROTOCOL_PREFIX: &str = "loom.bearer.";

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
    state.outbox.clear();
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
    secure_for_secrets: bool,
    // The socket's callbacks borrow these closures for their lifetime; they
    // must stay alive as long as the connection does.
    _on_open: Rc<Closure<dyn FnMut(web_sys::Event)>>,
    _on_message: Rc<Closure<dyn FnMut(MessageEvent)>>,
    _on_error: Rc<Closure<dyn FnMut(web_sys::Event)>>,
    _on_close: Rc<Closure<dyn FnMut(CloseEvent)>>,
}

impl BrowserConnection {
    pub(crate) fn connect(remote: &str, token: &str) -> Result<Self, LoomError> {
        let protocols = js_sys::Array::new();
        protocols.push(&JsValue::from_str(&format!(
            "{BEARER_SUBPROTOCOL_PREFIX}{token}"
        )));
        let socket = WebSocket::new_with_str_sequence(remote, &protocols).map_err(|_| {
            LoomError::new(
                ErrorCode::InvalidRequest,
                "could not open a worker WebSocket; check the ws:// or wss:// URL and path",
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
                if let Some(sender) = message_state
                    .borrow_mut()
                    .pending
                    .remove(&response.request_id)
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
            secure_for_secrets: crate::connection::remote_url_is_secure_for_secrets(remote),
            _on_open: Rc::new(on_open),
            _on_message: Rc::new(on_message),
            _on_error: Rc::new(on_error),
            _on_close: Rc::new(on_close),
        })
    }

    pub(crate) fn secure_for_secrets(&self) -> bool {
        self.secure_for_secrets
    }

    /// The reason this connection is closed, if it has been closed.
    ///
    /// The browser transport is single-use: once `onclose`/`onerror` fires the
    /// socket cannot be reopened, so the view replaces it with a reconnect
    /// screen instead of silently failing every later request.
    pub(crate) fn closed_reason(&self) -> Option<String> {
        self.state.borrow().closed.clone()
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

    pub(crate) fn close(&self) -> Result<(), LoomError> {
        let state = self.state.borrow_mut();
        state.socket.set_onopen(None);
        state.socket.set_onmessage(None);
        state.socket.set_onerror(None);
        state.socket.set_onclose(None);
        let result = state.socket.close();
        drop(state);
        fail_all(
            &self.state,
            "the browser connection was closed by the client",
        );
        result.map_err(|error| {
            LoomError::new(
                ErrorCode::RequestCancelled,
                format!(
                    "could not close the browser connection: {}",
                    describe_js(&error)
                ),
                true,
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
