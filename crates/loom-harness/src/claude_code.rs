//! Drives the Claude Code CLI in headless `stream-json` mode.
//!
//! One [`ClaudeCodeSession`] is one `claude -p` child with piped stdio. Prompts
//! go in as `user` lines and the CLI streams one JSON object per line back,
//! which [`parse_line`] turns into [`ClaudeEvent`]s. With
//! `--permission-prompt-tool stdio` the CLI asks the host before every tool
//! that needs approval, and the host answers with
//! [`ClaudeCodeSession::respond_permission`].
//!
//! The binary and its login belong to the person running the worker: this
//! module launches `claude` and never reads its credentials. The child's
//! `ANTHROPIC_API_KEY` and `ANTHROPIC_AUTH_TOKEN` are removed unless
//! [`ClaudeCodeOptions::inherit_api_key_env`] is set, because Claude Code bills
//! the API instead of the subscription when one is present.

use std::{
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread,
    time::Duration,
};

use loom_core::{ErrorCode, LoomError, Result};
use serde_json::{Value, json};

/// How much of the child's stderr is kept for error messages.
const STDERR_TAIL_BYTES: usize = 8 * 1024;

/// How Claude Code decides which tool calls need the host's approval.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PermissionMode {
    /// Ask for everything that is not read-only.
    #[default]
    Default,
    /// Edits inside the working directory are allowed without asking.
    AcceptEdits,
    /// Plan only: nothing is changed.
    Plan,
    /// Anything that would ask is denied without asking.
    DontAsk,
}

impl PermissionMode {
    fn flag(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::DontAsk => "dontAsk",
        }
    }
}

/// What to launch and how.
#[derive(Clone, Debug)]
pub struct ClaudeCodeOptions {
    /// The `claude` executable.
    pub binary: PathBuf,
    /// The directory the agent works in.
    pub cwd: PathBuf,
    /// A model alias or full name; `None` keeps the CLI's own default.
    pub model: Option<String>,
    /// A session id to continue, so the CLI restores its own conversation.
    pub resume: Option<String>,
    pub permission_mode: PermissionMode,
    /// Built-in tools to offer: `None` is the CLI default set and `Some(vec![])`
    /// offers none.
    pub tools: Option<Vec<String>>,
    pub append_system_prompt: Option<String>,
    /// MCP servers, as a JSON file path or inline JSON.
    pub mcp_config: Option<String>,
    /// Which settings files to load (`user`, `project`, `local`), comma
    /// separated; `None` keeps the CLI default.
    pub setting_sources: Option<String>,
    /// Keep `ANTHROPIC_API_KEY` and `ANTHROPIC_AUTH_TOKEN` in the child's
    /// environment, which makes Claude Code bill that key.
    pub inherit_api_key_env: bool,
    /// Extra environment for the child, such as where its builds write.
    pub env: Vec<(String, String)>,
}

impl ClaudeCodeOptions {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            binary: PathBuf::from("claude"),
            cwd: cwd.into(),
            model: None,
            resume: None,
            permission_mode: PermissionMode::Default,
            tools: None,
            append_system_prompt: None,
            mcp_config: None,
            setting_sources: None,
            inherit_api_key_env: false,
            env: Vec::new(),
        }
    }

    /// The command-line arguments, without the binary.
    pub fn args(&self) -> Vec<String> {
        let mut args = [
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--permission-prompts",
            "host",
            "--permission-prompt-tool",
            "stdio",
            "--permission-mode",
            self.permission_mode.flag(),
        ]
        .map(str::to_owned)
        .to_vec();
        let mut option = |flag: &str, value: &Option<String>| {
            if let Some(value) = value {
                args.push(flag.to_owned());
                args.push(value.clone());
            }
        };
        option("--model", &self.model);
        option("--resume", &self.resume);
        option("--append-system-prompt", &self.append_system_prompt);
        option("--mcp-config", &self.mcp_config);
        option("--setting-sources", &self.setting_sources);
        if let Some(tools) = &self.tools {
            args.push("--tools".to_owned());
            args.push(tools.join(","));
        }
        args
    }
}

/// Token counts reported for one result.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TokenCounts {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
}

/// How a turn ended.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResultSummary {
    pub is_error: bool,
    pub subtype: String,
    pub result: Option<String>,
    pub stop_reason: Option<String>,
    pub session_id: Option<String>,
    pub total_cost_usd: Option<f64>,
    pub usage: TokenCounts,
    pub num_turns: Option<u64>,
    pub duration_ms: Option<u64>,
}

/// One decoded line of Claude Code's output.
///
/// Text and thinking arrive as deltas. The complete assistant message repeats
/// that text, so only its tool calls are reported, as [`Self::ToolUse`].
#[derive(Clone, Debug, PartialEq)]
pub enum ClaudeEvent {
    Init {
        session_id: String,
        model: Option<String>,
        tools: Vec<String>,
        permission_mode: Option<String>,
    },
    TextDelta {
        text: String,
        parent_tool_use_id: Option<String>,
    },
    ThinkingDelta {
        text: String,
        parent_tool_use_id: Option<String>,
    },
    /// A tool call, complete with its input.
    ToolUse {
        id: String,
        name: String,
        input: Value,
        parent_tool_use_id: Option<String>,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
        parent_tool_use_id: Option<String>,
    },
    /// The CLI asks the host to allow a tool call and waits for
    /// [`ClaudeCodeSession::respond_permission`].
    PermissionRequest {
        request_id: String,
        tool_name: String,
        input: Value,
        description: Option<String>,
    },
    /// The CLI no longer needs the answer to an earlier request.
    PermissionCancelled {
        request_id: String,
    },
    /// A tool call was refused without asking the host.
    PermissionDenied {
        tool_use_id: Option<String>,
        tool_name: String,
        message: String,
    },
    /// The account's plan limits, as the CLI last saw them.
    RateLimit {
        status: String,
        window: Option<String>,
        is_using_overage: bool,
        /// Every limit window the CLI reported, such as `five_hour` and
        /// `seven_day`.
        windows: Vec<RateLimitWindow>,
    },
    Result(ResultSummary),
    /// A well-formed line this module has no meaning for.
    Other {
        kind: String,
    },
}

/// One plan-limit window from a [`ClaudeEvent::RateLimit`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RateLimitWindow {
    /// The window's name, such as `five_hour`.
    pub name: String,
    /// The share of the window used, from 0 to 1, when reported.
    pub utilization: Option<f64>,
    /// When the window resets, in Unix seconds, when reported.
    pub resets_at: Option<u64>,
    /// The window's status, such as `allowed` or `rejected`, when reported.
    pub status: Option<String>,
}

/// Reads the limit windows of a `rate_limit_info` object.
///
/// Newer CLIs list every window under `unifiedWindows`; older ones describe
/// only the window that applies, with `rateLimitType` and `resetsAt`.
fn rate_limit_windows(info: &Value) -> Vec<RateLimitWindow> {
    let window = |name: &str, value: &Value| RateLimitWindow {
        name: name.to_owned(),
        utilization: value.get("utilization").and_then(Value::as_f64),
        resets_at: value.get("resetsAt").and_then(Value::as_u64),
        status: string_field(value, "status"),
    };
    match info.get("unifiedWindows").and_then(Value::as_object) {
        Some(windows) if !windows.is_empty() => windows
            .iter()
            .map(|(name, value)| window(name, value))
            .collect(),
        _ => string_field(info, "rateLimitType")
            .filter(|_| info.get("utilization").is_some() || info.get("resetsAt").is_some())
            .map(|name| vec![window(&name, info)])
            .unwrap_or_default(),
    }
}

/// The host's answer to a [`ClaudeEvent::PermissionRequest`].
#[derive(Clone, Debug, PartialEq)]
pub enum PermissionDecision {
    /// Run the call with this input, usually the one that was asked about.
    Allow { input: Value },
    /// Refuse the call; the message goes back to the model.
    Deny { message: String },
}

fn malformed(message: impl Into<String>) -> LoomError {
    LoomError::new(ErrorCode::MalformedPayload, message, false)
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn counts(usage: &Value) -> TokenCounts {
    let number = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    TokenCounts {
        input: number("input_tokens"),
        output: number("output_tokens"),
        cache_read: number("cache_read_input_tokens"),
        cache_creation: number("cache_creation_input_tokens"),
    }
}

/// Decodes one output line into the events it carries.
///
/// A line that is not a JSON object is an error; a well-formed line of a kind
/// this module does not use is [`ClaudeEvent::Other`].
pub fn parse_line(line: &str) -> Result<Vec<ClaudeEvent>> {
    let value: Value = serde_json::from_str(line)
        .map_err(|error| malformed(format!("claude output is not JSON: {error}")))?;
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| malformed("claude output line has no type"))?;
    let parent = string_field(&value, "parent_tool_use_id");
    let other = |kind: &str| {
        vec![ClaudeEvent::Other {
            kind: kind.to_owned(),
        }]
    };
    Ok(match kind {
        "system" => match value.get("subtype").and_then(Value::as_str) {
            Some("init") => vec![ClaudeEvent::Init {
                session_id: string_field(&value, "session_id").unwrap_or_default(),
                model: string_field(&value, "model"),
                tools: value
                    .get("tools")
                    .and_then(Value::as_array)
                    .map(|tools| {
                        tools
                            .iter()
                            .filter_map(|tool| tool.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default(),
                permission_mode: string_field(&value, "permissionMode"),
            }],
            Some("permission_denied") => vec![ClaudeEvent::PermissionDenied {
                tool_use_id: string_field(&value, "tool_use_id"),
                tool_name: string_field(&value, "tool_name").unwrap_or_default(),
                message: string_field(&value, "message").unwrap_or_default(),
            }],
            Some(subtype) => other(&format!("system/{subtype}")),
            None => other("system"),
        },
        "stream_event" => {
            let event = value.get("event").unwrap_or(&Value::Null);
            let delta = event.get("delta").unwrap_or(&Value::Null);
            match (
                event.get("type").and_then(Value::as_str),
                delta.get("type").and_then(Value::as_str),
            ) {
                (Some("content_block_delta"), Some("text_delta")) => {
                    vec![ClaudeEvent::TextDelta {
                        text: string_field(delta, "text").unwrap_or_default(),
                        parent_tool_use_id: parent,
                    }]
                }
                (Some("content_block_delta"), Some("thinking_delta")) => {
                    vec![ClaudeEvent::ThinkingDelta {
                        text: string_field(delta, "thinking").unwrap_or_default(),
                        parent_tool_use_id: parent,
                    }]
                }
                _ => other("stream_event"),
            }
        }
        "assistant" => blocks(&value, "tool_use")
            .map(|block| ClaudeEvent::ToolUse {
                id: string_field(block, "id").unwrap_or_default(),
                name: string_field(block, "name").unwrap_or_default(),
                input: block.get("input").cloned().unwrap_or(Value::Null),
                parent_tool_use_id: parent.clone(),
            })
            .collect(),
        "user" => blocks(&value, "tool_result")
            .map(|block| ClaudeEvent::ToolResult {
                tool_use_id: string_field(block, "tool_use_id").unwrap_or_default(),
                content: text_of(block.get("content").unwrap_or(&Value::Null)),
                is_error: block
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                parent_tool_use_id: parent.clone(),
            })
            .collect(),
        "control_request" => {
            let request = value.get("request").unwrap_or(&Value::Null);
            if request.get("subtype").and_then(Value::as_str) == Some("can_use_tool") {
                vec![ClaudeEvent::PermissionRequest {
                    request_id: string_field(&value, "request_id")
                        .ok_or_else(|| malformed("permission request has no request_id"))?,
                    tool_name: string_field(request, "tool_name").unwrap_or_default(),
                    input: request.get("input").cloned().unwrap_or(Value::Null),
                    description: string_field(request, "description"),
                }]
            } else {
                other("control_request")
            }
        }
        "control_cancel_request" => vec![ClaudeEvent::PermissionCancelled {
            request_id: string_field(&value, "request_id").unwrap_or_default(),
        }],
        "rate_limit_event" => {
            let info = value.get("rate_limit_info").unwrap_or(&Value::Null);
            vec![ClaudeEvent::RateLimit {
                status: string_field(info, "status").unwrap_or_default(),
                window: string_field(info, "rateLimitType"),
                is_using_overage: info
                    .get("isUsingOverage")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                windows: rate_limit_windows(info),
            }]
        }
        "result" => vec![ClaudeEvent::Result(ResultSummary {
            is_error: value
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            subtype: string_field(&value, "subtype").unwrap_or_default(),
            result: string_field(&value, "result"),
            stop_reason: string_field(&value, "stop_reason"),
            session_id: string_field(&value, "session_id"),
            total_cost_usd: value.get("total_cost_usd").and_then(Value::as_f64),
            usage: counts(value.get("usage").unwrap_or(&Value::Null)),
            num_turns: value.get("num_turns").and_then(Value::as_u64),
            duration_ms: value.get("duration_ms").and_then(Value::as_u64),
        })],
        other_kind => other(other_kind),
    })
}

/// The content blocks of a message line that have the given `type`.
fn blocks<'a>(value: &'a Value, kind: &'a str) -> impl Iterator<Item = &'a Value> {
    value
        .pointer("/message/content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(move |block| block.get("type").and_then(Value::as_str) == Some(kind))
}

/// What [`ClaudeCodeSession::next_event`] found.
#[derive(Clone, Debug, PartialEq)]
pub enum Next {
    Event(ClaudeEvent),
    /// Nothing arrived within the wait.
    Idle,
    /// The child closed its output; no more events will come.
    Exited,
}

/// One running `claude -p` child.
pub struct ClaudeCodeSession {
    child: Child,
    stdin: Option<ChildStdin>,
    events: Receiver<ClaudeEvent>,
    stderr_tail: Arc<Mutex<String>>,
    next_request: u64,
}

impl ClaudeCodeSession {
    /// Starts the child. The CLI emits its `init` line once the first prompt
    /// arrives, so call [`Self::send_user_message`] next.
    pub fn spawn(options: &ClaudeCodeOptions) -> Result<Self> {
        let mut command = Command::new(&options.binary);
        command
            .args(options.args())
            .current_dir(&options.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if !options.inherit_api_key_env {
            command
                .env_remove("ANTHROPIC_API_KEY")
                .env_remove("ANTHROPIC_AUTH_TOKEN");
        }
        for (key, value) in &options.env {
            command.env(key, value);
        }
        let mut child = command.spawn().map_err(|error| {
            LoomError::new(
                ErrorCode::ProviderUnavailable,
                format!(
                    "could not start '{}': {error}; install Claude Code and sign in with `claude`",
                    options.binary.display()
                ),
                false,
            )
        })?;
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| LoomError::new(ErrorCode::Internal, "claude has no stdout", false))?;
        let stderr = child.stderr.take();

        let (sender, events) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                match parse_line(&line) {
                    Ok(parsed) => {
                        for event in parsed {
                            if sender.send(event).is_err() {
                                return;
                            }
                        }
                    }
                    Err(error) => log::warn!("[loom-harness] {}", error.message),
                }
            }
        });
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        if let Some(stderr) = stderr {
            let tail = Arc::clone(&stderr_tail);
            thread::spawn(move || drain_stderr(stderr, &tail));
        }
        Ok(Self {
            child,
            stdin,
            events,
            stderr_tail,
            next_request: 0,
        })
    }

    fn write_line(&mut self, value: &Value) -> Result<()> {
        let stdin = self.stdin.as_mut().ok_or_else(|| {
            LoomError::new(ErrorCode::ProviderUnavailable, "claude has exited", false)
        })?;
        let mut line = value.to_string();
        line.push('\n');
        stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.flush())
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::ProviderUnavailable,
                    format!("could not write to claude: {error}"),
                    true,
                )
            })
    }

    /// Sends a user prompt, starting or continuing a turn.
    pub fn send_user_message(&mut self, text: &str) -> Result<()> {
        self.write_line(&json!({
            "type": "user",
            "message": { "role": "user", "content": text },
        }))
    }

    /// Answers a [`ClaudeEvent::PermissionRequest`].
    pub fn respond_permission(
        &mut self,
        request_id: &str,
        decision: &PermissionDecision,
    ) -> Result<()> {
        let response = match decision {
            PermissionDecision::Allow { input } => {
                json!({ "behavior": "allow", "updatedInput": input })
            }
            PermissionDecision::Deny { message } => {
                json!({ "behavior": "deny", "message": message })
            }
        };
        self.write_line(&json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": response,
            },
        }))
    }

    /// Asks the CLI to stop the turn in progress and stay alive.
    pub fn interrupt(&mut self) -> Result<()> {
        self.next_request += 1;
        let request_id = format!("loom-{}", self.next_request);
        self.write_line(&json!({
            "type": "control_request",
            "request_id": request_id,
            "request": { "subtype": "interrupt" },
        }))
    }

    /// Waits up to `timeout` for the next event.
    pub fn next_event(&mut self, timeout: Duration) -> Next {
        match self.events.recv_timeout(timeout) {
            Ok(event) => Next::Event(event),
            Err(RecvTimeoutError::Timeout) => Next::Idle,
            Err(RecvTimeoutError::Disconnected) => Next::Exited,
        }
    }

    /// Takes every event already waiting, without blocking.
    ///
    /// A child kept between turns can say more after a turn ends, for example
    /// when a background command finishes. Draining before the next prompt
    /// keeps that output from being read as the next turn's.
    pub fn drain(&mut self) -> Vec<ClaudeEvent> {
        self.events.try_iter().collect()
    }

    /// Whether the child has exited.
    pub fn has_exited(&mut self) -> bool {
        !matches!(self.child.try_wait(), Ok(None))
    }

    /// The end of the child's stderr, for an error message.
    pub fn stderr_tail(&self) -> String {
        self.stderr_tail
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Ends the child and waits for it.
    pub fn kill(&mut self) {
        self.stdin = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for ClaudeCodeSession {
    fn drop(&mut self) {
        self.kill();
    }
}

fn drain_stderr(mut stderr: impl Read, tail: &Mutex<String>) {
    let mut buffer = [0_u8; 1024];
    while let Ok(read) = stderr.read(&mut buffer) {
        if read == 0 {
            break;
        }
        let mut tail = tail.lock().unwrap_or_else(|error| error.into_inner());
        tail.push_str(&String::from_utf8_lossy(&buffer[..read]));
        if tail.len() > STDERR_TAIL_BYTES {
            let mut cut = tail.len() - STDERR_TAIL_BYTES;
            while !tail.is_char_boundary(cut) {
                cut += 1;
            }
            tail.drain(..cut);
        }
    }
}

#[cfg(test)]
mod tests;
