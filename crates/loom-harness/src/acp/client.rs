//! Supervised ACP v1 JSON-RPC 2.0 client over piped stdio.
//!
//! Spawns one unmodified harness binary per session with `Stdio::piped()` (never
//! a PTY), enforces bounded stdout/stderr buffering, streams ACP `session/update`
//! notifications and live `session/request_permission` requests, and cleans up
//! the child process on drop.

use std::{
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};

use loom_core::{
    ANTIGRAVITY_AUTH_GEMINI_API_KEY, AcpCapabilities, AcpModelCatalogEntry, ErrorCode, LoomError,
    Result,
};
use serde_json::{Value, json};

use super::{
    ACP_PROTOCOL_VERSION, AcpAuthMethod, AcpEvent, AcpInitializeResult, AcpNext, AcpOptions,
    AcpPermissionOption, AcpPermissionOutcome, AcpPermissionRequest, AcpPlanEntry, AcpPromptResult,
    AcpSessionInfo, AcpToolCall, AcpToolCallUpdate, AcpUsageUpdate, LOOM_ACP_CLIENT_NAME,
    LOOM_ACP_CLIENT_TITLE, LOOM_ACP_CLIENT_VERSION,
    antigravity::{redact_acp_text, validate_auth_method},
};

/// Bound kept from stderr for error reports (matches `loom-harness` / `loom-process` bounds).
const STDERR_TAIL_BYTES: usize = 16 * 1024;
/// Bound on a single JSON-RPC line from stdout so a misbehaving child cannot OOM the worker.
const MAX_JSON_LINE_BYTES: usize = 16 * 1024 * 1024;
/// Bound on a single forwarded stderr line.
const MAX_STDERR_LINE_BYTES: usize = 4 * 1024;
/// Bounded wire-message channel capacity so an unpolled child cannot grow memory without limit.
const WIRE_CHANNEL_CAPACITY: usize = 4096;
/// Poll quantum while awaiting handshake/auth responses.
const WAIT_POLL: Duration = Duration::from_millis(50);

/// Internal messages from the stdout/stderr reader threads.
#[derive(Debug)]
enum WireMessage {
    Response {
        id: u64,
        result: Option<Value>,
        error: Option<JsonRpcError>,
    },
    Event(AcpEvent),
    UnsupportedClientRequest {
        id: Value,
        method: String,
    },
    StdoutClosed,
}

#[derive(Clone, Debug)]
struct JsonRpcError {
    code: i64,
    message: String,
}

/// Out-of-band kill handle for terminating an [`AcpClient`] child process
/// without blocking on a turn loop that owns `&mut AcpClient`.
#[derive(Clone)]
pub struct AcpKillHandle {
    child: Arc<Mutex<Child>>,
}

impl AcpKillHandle {
    /// Kills and reaps the underlying child process if it is still alive.
    pub fn kill(&self) {
        let mut child = self.child.lock().unwrap_or_else(|error| error.into_inner());
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// One live supervised ACP harness subprocess.
pub struct AcpClient {
    child: Arc<Mutex<Child>>,
    stdin: Option<ChildStdin>,
    rx: Receiver<WireMessage>,
    stderr_tail: Arc<Mutex<String>>,
    next_id: u64,
    command_display: String,
    /// Pending prompt request id, if a `session/prompt` turn is in flight.
    active_prompt_id: Option<u64>,
}

impl AcpClient {
    /// Spawns the ACP harness subprocess with piped stdio (never a PTY).
    pub fn spawn(options: &AcpOptions) -> Result<Self> {
        let mut command = Command::new(&options.command.program);
        command
            .args(&options.command.args)
            .current_dir(&options.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(path) = loom_core::child_path() {
            command.env("PATH", path);
        }

        if let Some(profile_dir) = &options.profile_dir {
            loom_core::create_private_dir(profile_dir).map_err(|error| {
                LoomError::new(
                    ErrorCode::WorkspaceAccessDenied,
                    format!(
                        "could not create isolated ACP profile directory '{}': {error}",
                        profile_dir.display()
                    ),
                    false,
                )
            })?;
            let profile_str = profile_dir.to_string_lossy().into_owned();
            if options.command.supports_agy_profile_flags {
                let gemini_dir = profile_dir.join(".gemini");
                loom_core::create_private_dir(&gemini_dir).map_err(|error| {
                    LoomError::new(
                        ErrorCode::WorkspaceAccessDenied,
                        format!(
                            "could not create isolated ACP gemini directory '{}': {error}",
                            gemini_dir.display()
                        ),
                        false,
                    )
                })?;
                command
                    .arg("--app-data-dir")
                    .arg(&profile_str)
                    .arg("--gemini-dir")
                    .arg(gemini_dir.to_string_lossy().into_owned());
            }
            command.env("LOOM_ACP_PROFILE_DIR", profile_str);
        }

        for (key, value) in &options.env {
            command.env(key, value);
        }

        let mut child = command.spawn().map_err(|error| {
            LoomError::new(
                ErrorCode::ProviderUnavailable,
                format!(
                    "could not start ACP harness '{}': {error}. Ensure the official `agy` / \
                     `antigravity-acp` binary is installed or configure a manual override \
                     (for the key-based path, use auth method '{ANTIGRAVITY_AUTH_GEMINI_API_KEY}').",
                    options.command.display
                ),
                false,
            )
        })?;

        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or_else(|| {
            LoomError::new(ErrorCode::Internal, "ACP harness has no stdout pipe", false)
        })?;
        let stderr = child.stderr.take();

        let (tx, rx) = mpsc::sync_channel(WIRE_CHANNEL_CAPACITY);
        let stdout_tx = tx.clone();
        thread::Builder::new()
            .name("loom-acp-stdout".to_owned())
            .spawn(move || read_stdout_loop(stdout, stdout_tx))
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::Internal,
                    format!("could not spawn ACP stdout reader: {error}"),
                    false,
                )
            })?;

        let stderr_tail = Arc::new(Mutex::new(String::new()));
        if let Some(stderr) = stderr {
            let tail = Arc::clone(&stderr_tail);
            let stderr_tx = tx;
            let _ = thread::Builder::new()
                .name("loom-acp-stderr".to_owned())
                .spawn(move || read_stderr_loop(stderr, &tail, stderr_tx));
        }

        Ok(Self {
            child: Arc::new(Mutex::new(child)),
            stdin,
            rx,
            stderr_tail,
            next_id: 0,
            command_display: options.command.display.clone(),
            active_prompt_id: None,
        })
    }

    /// Returns a cloneable handle that can terminate this subprocess without
    /// waiting for `&mut AcpClient`.
    pub fn kill_handle(&self) -> AcpKillHandle {
        AcpKillHandle {
            child: Arc::clone(&self.child),
        }
    }

    /// Performs ACP `initialize` using Loom's own `clientInfo` (`loom` + crate version).
    pub fn initialize(&mut self, timeout: Duration) -> Result<AcpInitializeResult> {
        let id = self.send_request(
            "initialize",
            json!({
                "protocolVersion": ACP_PROTOCOL_VERSION,
                "clientCapabilities": {
                    "fs": {
                        "readTextFile": false,
                        "writeTextFile": false
                    },
                    "terminal": false
                },
                "clientInfo": {
                    "name": LOOM_ACP_CLIENT_NAME,
                    "title": LOOM_ACP_CLIENT_TITLE,
                    "version": LOOM_ACP_CLIENT_VERSION
                }
            }),
        )?;
        let value = self.wait_for_response(id, timeout, None, &mut |_| {})?;
        parse_initialize_result(&value)
    }

    /// Performs ACP `authenticate` using a method advertised by `initialize`.
    ///
    /// Streams any stderr lines or notifications to `on_event` while waiting
    /// (for example interactive browser/terminal sign-in prompts) and checks
    /// `is_cancelled` so sign-in can never hang.
    pub fn authenticate(
        &mut self,
        requested_method: Option<&str>,
        advertised: &[AcpAuthMethod],
        timeout: Duration,
        is_cancelled: Option<&dyn Fn() -> bool>,
        on_event: &mut dyn FnMut(AcpEvent),
    ) -> Result<String> {
        let method_id = validate_auth_method(requested_method, advertised)?;
        let id = self.send_request(
            "authenticate",
            json!({
                "methodId": method_id,
            }),
        )?;
        match self.wait_for_response(id, timeout, is_cancelled, on_event) {
            Ok(_) => Ok(method_id),
            Err(error) if error.code == ErrorCode::RequestCancelled => Err(error),
            Err(error) => {
                let hint = if method_id == ANTIGRAVITY_AUTH_GEMINI_API_KEY {
                    " Ensure GEMINI_API_KEY is exported in the worker environment."
                } else {
                    " You can retry sign-in or switch to auth method 'gemini-api-key' with \
                     GEMINI_API_KEY set in the worker environment."
                };
                Err(LoomError::new(
                    ErrorCode::ProviderAuthentication,
                    format!(
                        "ACP authenticate('{method_id}') failed: {}.{}",
                        error.message, hint
                    ),
                    false,
                ))
            }
        }
    }

    /// Starts a new ACP session rooted at `cwd` with empty `mcpServers`.
    pub fn session_new(&mut self, cwd: &Path, timeout: Duration) -> Result<AcpSessionInfo> {
        let id = self.send_request(
            "session/new",
            json!({
                "cwd": cwd.to_string_lossy(),
                "mcpServers": [],
            }),
        )?;
        let value = self.wait_for_response(id, timeout, None, &mut |_| {})?;
        parse_session_info(&value, None)
    }

    /// Resumes an existing ACP session via `session/load` (only call when
    /// `capabilities.load_session` was advertised).
    pub fn session_load(
        &mut self,
        session_id: &str,
        cwd: &Path,
        timeout: Duration,
    ) -> Result<AcpSessionInfo> {
        let id = self.send_request(
            "session/load",
            json!({
                "sessionId": session_id,
                "cwd": cwd.to_string_lossy(),
                "mcpServers": [],
            }),
        )?;
        let value = self.wait_for_response(id, timeout, None, &mut |_| {})?;
        parse_session_info(&value, Some(session_id))
    }

    /// Resumes an existing ACP session via `session/resume` (only call when
    /// `capabilities.resume_session` was advertised).
    pub fn session_resume(
        &mut self,
        session_id: &str,
        cwd: &Path,
        timeout: Duration,
    ) -> Result<AcpSessionInfo> {
        let id = self.send_request(
            "session/resume",
            json!({
                "sessionId": session_id,
                "cwd": cwd.to_string_lossy(),
                "mcpServers": [],
            }),
        )?;
        let value = self.wait_for_response(id, timeout, None, &mut |_| {})?;
        parse_session_info(&value, Some(session_id))
    }

    /// Selects a model from the harness's advertised catalog.
    pub fn set_model(
        &mut self,
        session_id: &str,
        model_id: &str,
        config_option_id: Option<&str>,
        timeout: Duration,
    ) -> Result<()> {
        let id = if let Some(config_id) = config_option_id {
            self.send_request(
                "session/set_config_option",
                json!({
                    "sessionId": session_id,
                    "configId": config_id,
                    "value": model_id,
                }),
            )?
        } else {
            self.send_request(
                "session/set_model",
                json!({
                    "sessionId": session_id,
                    "modelId": model_id,
                }),
            )?
        };
        let _ = self.wait_for_response(id, timeout, None, &mut |_| {})?;
        Ok(())
    }

    /// Starts one `session/prompt` turn with the user's raw prompt text (never
    /// injecting Loom's system prompt, tool list, or context assembly).
    pub fn start_prompt(&mut self, session_id: &str, text: &str) -> Result<u64> {
        let id = self.send_request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [
                    {
                        "type": "text",
                        "text": text,
                    }
                ]
            }),
        )?;
        self.active_prompt_id = Some(id);
        Ok(id)
    }

    /// Sends an ACP `session/cancel` notification for the active turn.
    pub fn cancel_session(&mut self, session_id: &str) -> Result<()> {
        self.send_notification(
            "session/cancel",
            json!({
                "sessionId": session_id,
            }),
        )
    }

    /// Responds to a live `session/request_permission` request from the agent.
    pub fn respond_permission(
        &mut self,
        request_id: &Value,
        outcome: &AcpPermissionOutcome,
    ) -> Result<()> {
        let outcome_json = match outcome {
            AcpPermissionOutcome::Selected { option_id } => json!({
                "outcome": "selected",
                "optionId": option_id,
            }),
            AcpPermissionOutcome::Cancelled => json!({
                "outcome": "cancelled",
            }),
        };
        self.write_json(&json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {
                "outcome": outcome_json,
            }
        }))
    }

    /// Polls for the next event from the running ACP harness.
    pub fn next_event(&mut self, timeout: Duration) -> AcpNext {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(remaining) {
                Ok(WireMessage::Event(event)) => return AcpNext::Event(Box::new(event)),
                Ok(WireMessage::UnsupportedClientRequest { id, method }) => {
                    let _ = self.respond_method_not_found(&id, &method);
                    if Instant::now() >= deadline {
                        return AcpNext::Idle;
                    }
                }
                Ok(WireMessage::Response { id, result, error }) => {
                    if self.active_prompt_id == Some(id) {
                        self.active_prompt_id = None;
                        if let Some(error) = error {
                            return AcpNext::Event(Box::new(AcpEvent::PromptFailed {
                                request_id: id,
                                code: error.code,
                                message: error.message,
                            }));
                        }
                        let payload = result.unwrap_or(Value::Null);
                        let stop_reason = payload
                            .get("stopReason")
                            .and_then(Value::as_str)
                            .unwrap_or("end_turn")
                            .to_owned();
                        let usage = payload.get("usage").and_then(parse_usage_update);
                        return AcpNext::Event(Box::new(AcpEvent::PromptFinished(
                            AcpPromptResult {
                                request_id: id,
                                stop_reason,
                                usage,
                            },
                        )));
                    }
                    log::debug!(
                        "[loom-harness::acp] ignored out-of-band response for id {id} from '{}'",
                        self.command_display
                    );
                }
                Ok(WireMessage::StdoutClosed) | Err(RecvTimeoutError::Disconnected) => {
                    let exit_code = self.try_exit_code();
                    return AcpNext::Exited {
                        exit_code,
                        stderr_tail: self.stderr_tail(),
                    };
                }
                Err(RecvTimeoutError::Timeout) => return AcpNext::Idle,
            }
        }
    }

    /// Non-blocking poll for one queued wire event (drains unsupported client
    /// requests automatically so the harness does not stall while waiting on
    /// an operator permission decision).
    pub fn try_next_event(&mut self) -> AcpNext {
        loop {
            match self.rx.try_recv() {
                Ok(WireMessage::Event(event)) => return AcpNext::Event(Box::new(event)),
                Ok(WireMessage::UnsupportedClientRequest { id, method }) => {
                    let _ = self.respond_method_not_found(&id, &method);
                }
                Ok(WireMessage::Response { id, result, error }) => {
                    if let Some(event) = self.handle_prompt_response(id, result, error) {
                        return AcpNext::Event(Box::new(event));
                    }
                }
                Ok(WireMessage::StdoutClosed) | Err(TryRecvError::Disconnected) => {
                    let exit_code = self.try_exit_code();
                    return AcpNext::Exited {
                        exit_code,
                        stderr_tail: self.stderr_tail(),
                    };
                }
                Err(TryRecvError::Empty) => return AcpNext::Idle,
            }
        }
    }

    fn handle_prompt_response(
        &mut self,
        id: u64,
        result: Option<Value>,
        error: Option<JsonRpcError>,
    ) -> Option<AcpEvent> {
        if self.active_prompt_id == Some(id) {
            self.active_prompt_id = None;
            if let Some(error) = error {
                return Some(AcpEvent::PromptFailed {
                    request_id: id,
                    code: error.code,
                    message: error.message,
                });
            }
            let payload = result.unwrap_or(Value::Null);
            let stop_reason = payload
                .get("stopReason")
                .and_then(Value::as_str)
                .unwrap_or("end_turn")
                .to_owned();
            let usage = payload.get("usage").and_then(parse_usage_update);
            return Some(AcpEvent::PromptFinished(AcpPromptResult {
                request_id: id,
                stop_reason,
                usage,
            }));
        }
        log::debug!(
            "[loom-harness::acp] ignored out-of-band response for id {id} from '{}'",
            self.command_display
        );
        None
    }

    /// Returns whether the child process has already exited.
    pub fn has_exited(&mut self) -> bool {
        let mut child = self.child.lock().unwrap_or_else(|error| error.into_inner());
        matches!(child.try_wait(), Ok(Some(_)))
    }

    /// Returns the bounded tail of the child's stderr.
    pub fn stderr_tail(&self) -> String {
        self.stderr_tail
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Kills and reaps the child subprocess cleanly.
    pub fn kill(&mut self) {
        self.stdin = None;
        let mut child = self.child.lock().unwrap_or_else(|error| error.into_inner());
        let _ = child.kill();
        let _ = child.wait();
    }

    fn try_exit_code(&mut self) -> Option<i32> {
        {
            let mut child = self.child.lock().unwrap_or_else(|error| error.into_inner());
            if let Ok(Some(status)) = child.try_wait() {
                return status.code();
            }
        }
        // Give the child a brief window to finish exiting after closing stdout.
        for _ in 0..10 {
            thread::sleep(Duration::from_millis(5));
            let mut child = self.child.lock().unwrap_or_else(|error| error.into_inner());
            if let Ok(Some(status)) = child.try_wait() {
                return status.code();
            }
        }
        None
    }

    fn next_request_id(&mut self) -> u64 {
        self.next_id = self.next_id.saturating_add(1);
        self.next_id
    }

    fn send_request(&mut self, method: &str, params: Value) -> Result<u64> {
        let id = self.next_request_id();
        self.write_json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;
        Ok(id)
    }

    fn send_notification(&mut self, method: &str, params: Value) -> Result<()> {
        self.write_json(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
    }

    fn respond_method_not_found(&mut self, id: &Value, method: &str) -> Result<()> {
        self.write_json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32601,
                "message": format!("client method '{method}' is not enabled by Loom ACP host"),
            }
        }))
    }

    fn write_json(&mut self, value: &Value) -> Result<()> {
        let stdin = self.stdin.as_mut().ok_or_else(|| {
            LoomError::new(
                ErrorCode::ProviderUnavailable,
                format!("ACP harness '{}' has exited", self.command_display),
                false,
            )
        })?;
        let mut line = value.to_string();
        line.push('\n');
        stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.flush())
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::ProviderUnavailable,
                    format!(
                        "could not write JSON-RPC message to ACP harness '{}': {error}",
                        self.command_display
                    ),
                    true,
                )
            })
    }

    fn wait_for_response(
        &mut self,
        expected_id: u64,
        timeout: Duration,
        is_cancelled: Option<&dyn Fn() -> bool>,
        on_event: &mut dyn FnMut(AcpEvent),
    ) -> Result<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            if is_cancelled.is_some_and(|check| check()) {
                return Err(LoomError::new(
                    ErrorCode::RequestCancelled,
                    "ACP request was cancelled",
                    false,
                ));
            }
            let now = Instant::now();
            if now >= deadline {
                let tail = self.stderr_tail();
                let detail = if tail.trim().is_empty() {
                    String::new()
                } else {
                    format!(" (stderr: {})", tail.trim())
                };
                return Err(LoomError::new(
                    ErrorCode::DeadlineExceeded,
                    format!(
                        "ACP harness '{}' timed out after {}s waiting for response {expected_id}{detail}",
                        self.command_display,
                        timeout.as_secs()
                    ),
                    true,
                ));
            }
            let wait = (deadline - now).min(WAIT_POLL);
            match self.rx.recv_timeout(wait) {
                Ok(WireMessage::Response { id, result, error }) if id == expected_id => {
                    if let Some(error) = error {
                        return Err(LoomError::new(
                            ErrorCode::ProviderUnavailable,
                            format!("JSON-RPC error {}: {}", error.code, error.message),
                            false,
                        ));
                    }
                    return Ok(result.unwrap_or(Value::Null));
                }
                Ok(WireMessage::Response { .. }) => {}
                Ok(WireMessage::UnsupportedClientRequest { id, method }) => {
                    let _ = self.respond_method_not_found(&id, &method);
                }
                Ok(WireMessage::Event(event)) => on_event(event),
                Ok(WireMessage::StdoutClosed) | Err(RecvTimeoutError::Disconnected) => {
                    let exit_code = self.try_exit_code();
                    let tail = self.stderr_tail();
                    let detail = if tail.trim().is_empty() {
                        String::new()
                    } else {
                        format!(": {}", tail.trim())
                    };
                    return Err(LoomError::new(
                        ErrorCode::ProviderUnavailable,
                        format!(
                            "ACP harness '{}' exited unexpectedly (code {:?}){detail}",
                            self.command_display, exit_code
                        ),
                        true,
                    ));
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        self.kill();
    }
}

fn read_stdout_loop(stdout: impl Read, tx: SyncSender<WireMessage>) {
    let mut reader = BufReader::new(stdout);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let Ok(read) = read_bounded_line(&mut reader, &mut buf, MAX_JSON_LINE_BYTES) else {
            break;
        };
        if read == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&buf);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match parse_wire_line(trimmed) {
            Ok(Some(msg)) => {
                if tx.send(msg).is_err() {
                    return;
                }
            }
            Ok(None) => {}
            Err(error) => {
                log::warn!("[loom-harness::acp] {}", error.message);
            }
        }
    }
    let _ = tx.send(WireMessage::StdoutClosed);
}

fn read_bounded_line(
    reader: &mut BufReader<impl Read>,
    buf: &mut Vec<u8>,
    max_bytes: usize,
) -> std::io::Result<usize> {
    let mut total = 0usize;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(total);
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            let take = pos + 1;
            let allowed = max_bytes.saturating_sub(buf.len()).min(take);
            buf.extend_from_slice(&available[..allowed]);
            reader.consume(take);
            total += take;
            return Ok(total);
        }
        let len = available.len();
        let allowed = max_bytes.saturating_sub(buf.len()).min(len);
        buf.extend_from_slice(&available[..allowed]);
        reader.consume(len);
        total += len;
    }
}

fn read_stderr_loop(stderr: impl Read, tail: &Mutex<String>, tx: SyncSender<WireMessage>) {
    let mut reader = BufReader::new(stderr);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let Ok(read) = read_bounded_line(&mut reader, &mut buf, MAX_STDERR_LINE_BYTES) else {
            break;
        };
        if read == 0 {
            break;
        }
        let raw_line = String::from_utf8_lossy(&buf);
        let redacted = redact_acp_text(&raw_line);
        {
            let mut guard = tail.lock().unwrap_or_else(|error| error.into_inner());
            guard.push_str(&redacted);
            if guard.len() > STDERR_TAIL_BYTES {
                let mut cut = guard.len() - STDERR_TAIL_BYTES;
                while !guard.is_char_boundary(cut) {
                    cut += 1;
                }
                guard.drain(..cut);
            }
        }
        let trimmed = redacted.trim();
        if !trimmed.is_empty() {
            // Non-blocking send so noisy stderr never backpressures or deadlocks
            // the stderr pipe when the receiver is busy; stderr_tail retains the
            // bounded tail regardless.
            let _ = tx.try_send(WireMessage::Event(AcpEvent::StderrLine(trimmed.to_owned())));
        }
    }
}

fn parse_wire_line(line: &str) -> Result<Option<WireMessage>> {
    let value: Value = serde_json::from_str(line).map_err(|error| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            format!("ACP stdout line is not valid JSON: {error}"),
            false,
        )
    })?;
    let Some(object) = value.as_object() else {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "ACP stdout line is not a JSON object",
            false,
        ));
    };

    // JSON-RPC request or notification (has `method`).
    if let Some(method) = object.get("method").and_then(Value::as_str) {
        let params = object.get("params").unwrap_or(&Value::Null);
        if let Some(id) = object.get("id") {
            if method == "session/request_permission" {
                let request = parse_permission_request(id.clone(), params)?;
                return Ok(Some(WireMessage::Event(AcpEvent::PermissionRequest(
                    request,
                ))));
            }
            return Ok(Some(WireMessage::UnsupportedClientRequest {
                id: id.clone(),
                method: method.to_owned(),
            }));
        }
        if method == "session/update" {
            return Ok(parse_session_update(params).map(WireMessage::Event));
        }
        return Ok(Some(WireMessage::Event(AcpEvent::Other {
            kind: method.to_owned(),
        })));
    }

    // JSON-RPC response (has `id` + `result`/`error`).
    if let Some(id) = object.get("id").and_then(Value::as_u64) {
        let error = object.get("error").map(|err| JsonRpcError {
            code: err.get("code").and_then(Value::as_i64).unwrap_or(-32603),
            message: err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown JSON-RPC error")
                .to_owned(),
        });
        let result = object.get("result").cloned();
        return Ok(Some(WireMessage::Response { id, result, error }));
    }

    Ok(None)
}

pub(crate) fn parse_initialize_result(value: &Value) -> Result<AcpInitializeResult> {
    let protocol_version = value
        .get("protocolVersion")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(ACP_PROTOCOL_VERSION);
    let agent_info = value.get("agentInfo").unwrap_or(&Value::Null);
    let agent_name = string_field(agent_info, "name");
    let agent_version = string_field(agent_info, "version");

    let caps = value.get("agentCapabilities").unwrap_or(&Value::Null);
    let load_session = caps
        .get("loadSession")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let session_caps = caps.get("sessionCapabilities").unwrap_or(&Value::Null);
    let resume_session = capability_present(session_caps.get("resume"));
    let list_sessions = capability_present(session_caps.get("list"));
    let prompt_caps = caps.get("promptCapabilities").unwrap_or(&Value::Null);
    let prompt_image = prompt_caps
        .get("image")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let prompt_audio = prompt_caps
        .get("audio")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let prompt_embedded_context = prompt_caps
        .get("embeddedContext")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let auth_methods = value
        .get("authMethods")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let id = string_field(item, "id").or_else(|| string_field(item, "methodId"))?;
                    let name = string_field(item, "name").unwrap_or_else(|| id.clone());
                    let description = string_field(item, "description");
                    Some(AcpAuthMethod {
                        id,
                        name,
                        description,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let capabilities = AcpCapabilities {
        load_session,
        resume_session,
        list_sessions,
        prompt_image,
        prompt_audio,
        prompt_embedded_context,
        auth_methods: auth_methods.iter().map(|m| m.id.clone()).collect(),
    };

    Ok(AcpInitializeResult {
        protocol_version,
        agent_name,
        agent_version,
        capabilities,
        auth_methods,
    })
}

fn capability_present(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Object(_)) => true,
        _ => false,
    }
}

pub(crate) fn parse_session_info(
    value: &Value,
    fallback_session_id: Option<&str>,
) -> Result<AcpSessionInfo> {
    let session_id = string_field(value, "sessionId")
        .or_else(|| fallback_session_id.map(str::to_owned))
        .ok_or_else(|| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "ACP session response did not include sessionId",
                false,
            )
        })?;
    let (models, current_model_id, model_config_option_id) = extract_model_catalog(value);
    Ok(AcpSessionInfo {
        session_id,
        current_model_id,
        models,
        model_config_option_id,
    })
}

/// Extracts available models from ACP `models.availableModels` and/or
/// `configOptions` (category `"model"` or id `"model"`).
pub(crate) fn extract_model_catalog(
    value: &Value,
) -> (Vec<AcpModelCatalogEntry>, Option<String>, Option<String>) {
    if let Some(models_obj) = value.get("models").and_then(Value::as_object) {
        let current = models_obj
            .get("currentModelId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let entries = models_obj
            .get("availableModels")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let id =
                            string_field(item, "modelId").or_else(|| string_field(item, "id"))?;
                        let display_name = string_field(item, "name").unwrap_or_else(|| id.clone());
                        let description = string_field(item, "description");
                        Some(AcpModelCatalogEntry {
                            id,
                            display_name,
                            description,
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if !entries.is_empty() || current.is_some() {
            return (entries, current, None);
        }
    }

    if let Some(config_options) = value.get("configOptions").and_then(Value::as_array) {
        for option in config_options {
            let category = string_field(option, "category");
            let id = string_field(option, "id");
            let is_model = category.as_deref() == Some("model") || id.as_deref() == Some("model");
            if !is_model {
                continue;
            }
            let current = string_field(option, "currentValue");
            let mut entries = Vec::new();
            if let Some(options) = option.get("options").and_then(Value::as_array) {
                collect_config_option_values(options, &mut entries);
            }
            return (entries, current, id);
        }
    }

    (Vec::new(), None, None)
}

fn collect_config_option_values(items: &[Value], out: &mut Vec<AcpModelCatalogEntry>) {
    for item in items {
        if let Some(nested) = item.get("options").and_then(Value::as_array) {
            collect_config_option_values(nested, out);
            continue;
        }
        let Some(id) = string_field(item, "value").or_else(|| string_field(item, "id")) else {
            continue;
        };
        let display_name = string_field(item, "name").unwrap_or_else(|| id.clone());
        let description = string_field(item, "description");
        out.push(AcpModelCatalogEntry {
            id,
            display_name,
            description,
        });
    }
}

pub(crate) fn parse_session_update(params: &Value) -> Option<AcpEvent> {
    let update = params.get("update").unwrap_or(params);
    let kind = update.get("sessionUpdate").and_then(Value::as_str)?;
    match kind {
        "agent_message_chunk" => {
            let text = extract_content_text(update.get("content").unwrap_or(&Value::Null));
            Some(AcpEvent::MessageChunk(text))
        }
        "agent_thought_chunk" => {
            let text = extract_content_text(update.get("content").unwrap_or(&Value::Null));
            Some(AcpEvent::ThoughtChunk(text))
        }
        "tool_call" => Some(AcpEvent::ToolCall(AcpToolCall {
            tool_call_id: string_field(update, "toolCallId").unwrap_or_default(),
            title: string_field(update, "title").unwrap_or_else(|| "tool".to_owned()),
            kind: string_field(update, "kind"),
            status: string_field(update, "status"),
            raw_input: update.get("rawInput").cloned().unwrap_or(Value::Null),
            locations: extract_locations(update.get("locations")),
        })),
        "tool_call_update" => Some(AcpEvent::ToolCallUpdate(AcpToolCallUpdate {
            tool_call_id: string_field(update, "toolCallId").unwrap_or_default(),
            title: string_field(update, "title"),
            kind: string_field(update, "kind"),
            status: string_field(update, "status"),
            raw_input: update.get("rawInput").cloned(),
            raw_output: extract_tool_output(update),
        })),
        "plan" => {
            let entries = update
                .get("entries")
                .and_then(Value::as_array)
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(|entry| {
                            let content = string_field(entry, "content")?;
                            Some(AcpPlanEntry {
                                content,
                                priority: string_field(entry, "priority"),
                                status: string_field(entry, "status"),
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            Some(AcpEvent::Plan(entries))
        }
        "usage_update" => parse_usage_update(update).map(AcpEvent::UsageUpdate),
        "config_option_update" => {
            let (models, current_model_id, model_config_option_id) = extract_model_catalog(update);
            if !models.is_empty() || current_model_id.is_some() {
                Some(AcpEvent::ModelsUpdated {
                    models,
                    current_model_id,
                    model_config_option_id,
                })
            } else {
                Some(AcpEvent::Other {
                    kind: kind.to_owned(),
                })
            }
        }
        other => Some(AcpEvent::Other {
            kind: other.to_owned(),
        }),
    }
}

pub(crate) fn parse_permission_request(
    request_id: Value,
    params: &Value,
) -> Result<AcpPermissionRequest> {
    let tool_call = params.get("toolCall").unwrap_or(&Value::Null);
    let title = string_field(tool_call, "title")
        .or_else(|| string_field(params, "title"))
        .unwrap_or_else(|| "ACP tool action".to_owned());
    let options = params
        .get("options")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let option_id = string_field(item, "optionId")?;
                    let name = string_field(item, "name").unwrap_or_else(|| option_id.clone());
                    let kind = string_field(item, "kind").unwrap_or_default();
                    Some(AcpPermissionOption {
                        option_id,
                        name,
                        kind,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(AcpPermissionRequest {
        request_id,
        session_id: string_field(params, "sessionId"),
        tool_call_id: string_field(tool_call, "toolCallId"),
        title,
        kind: string_field(tool_call, "kind"),
        raw_input: tool_call.get("rawInput").cloned().unwrap_or(Value::Null),
        options,
    })
}

fn parse_usage_update(value: &Value) -> Option<AcpUsageUpdate> {
    let input_tokens = value
        .get("inputTokens")
        .or_else(|| value.get("input_tokens"))
        .and_then(Value::as_u64);
    let output_tokens = value
        .get("outputTokens")
        .or_else(|| value.get("output_tokens"))
        .and_then(Value::as_u64);
    let cached_input_tokens = value
        .get("cachedReadTokens")
        .or_else(|| value.get("cached_input_tokens"))
        .and_then(Value::as_u64);
    let context_used = value.get("used").and_then(Value::as_u64);
    let context_size = value.get("size").and_then(Value::as_u64);
    let cost_usd = value
        .get("cost")
        .and_then(|cost| cost.get("amount"))
        .and_then(Value::as_f64);
    if input_tokens.is_none()
        && output_tokens.is_none()
        && cached_input_tokens.is_none()
        && context_used.is_none()
        && cost_usd.is_none()
    {
        return None;
    }
    Some(AcpUsageUpdate {
        input_tokens,
        output_tokens,
        cached_input_tokens,
        context_used,
        context_size,
        cost_usd,
    })
}

fn extract_content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Object(map) => map
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                map.get("content")
                    .map(extract_content_text)
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_default(),
        Value::Array(items) => items
            .iter()
            .map(extract_content_text)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn extract_tool_output(update: &Value) -> Option<String> {
    if let Some(raw_output) = update.get("rawOutput") {
        let text = match raw_output {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        };
        if !text.is_empty() {
            return Some(text);
        }
    }
    let content = update.get("content")?;
    let text = extract_content_text(content);
    (!text.is_empty()).then_some(text)
}

fn extract_locations(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| string_field(item, "path"))
                .collect()
        })
        .unwrap_or_default()
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}
