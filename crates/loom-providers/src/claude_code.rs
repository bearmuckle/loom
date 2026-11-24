//! The `claude` CLI as a model provider.
//!
//! Claude Code is an agent, not a model: it keeps its own conversation and runs
//! its own tools. So one [`ModelProvider::stream`] call plays one whole Claude
//! Code turn, and Loom's tool executor, approval policy and context assembly
//! have no part in it. The text streams out as the assistant message; what the
//! agent does on the way (tool calls, refusals) streams as reasoning, which a
//! client may show and the transcript does not keep.
//!
//! The CLI runs in the session's working directory, signed in as the account
//! that runs the worker. Nothing here reads that login.
//!
//! A tool that needs approval is refused, because Loom cannot yet put the
//! question to the person. Edits inside the working directory and read-only
//! commands go ahead.

use std::{path::Path, process::Command, time::Duration};

use loom_harness::{
    ClaudeCodeOptions, ClaudeCodeSession, ClaudeEvent, Next, PermissionDecision, PermissionMode,
};

use super::*;

/// The prefix of a Claude Code model id, before the `--model` alias.
const MODEL_PREFIX: &str = "claude-code:";
/// How long the CLI gets to wind a turn down after an interrupt.
const INTERRUPT_GRACE: Duration = Duration::from_secs(5);
/// How often a waiting stream looks at its cancellation token.
const POLL: Duration = Duration::from_millis(200);
/// The longest tool description streamed as reasoning.
const SUMMARY_BYTES: usize = 160;

pub struct ClaudeCodeProvider {
    descriptor: ModelDescriptor,
    binary: PathBuf,
    cwd: Option<PathBuf>,
    /// The CLI's own id for the conversation, so later turns continue it.
    claude_session: Option<String>,
    /// How many messages of the request the CLI has already seen, counting the
    /// reply it gave.
    delivered: usize,
}

impl ClaudeCodeProvider {
    pub fn new(descriptor: ModelDescriptor, binary: impl Into<PathBuf>) -> Self {
        Self {
            descriptor,
            binary: binary.into(),
            cwd: None,
            claude_session: None,
            delivered: 0,
        }
    }

    pub fn with_workspace(mut self, root: impl Into<PathBuf>) -> Self {
        self.cwd = Some(root.into());
        self
    }

    /// The `--model` alias, or `None` to leave the choice to the CLI.
    fn alias(&self) -> Option<String> {
        self.descriptor
            .id
            .as_str()
            .strip_prefix(MODEL_PREFIX)
            .filter(|alias| *alias != "default")
            .map(str::to_owned)
    }

    /// The prompt for the next turn and the system text to give the CLI.
    ///
    /// A continued conversation needs only the user messages the CLI has not
    /// seen. A fresh one carries what came before as a labelled recap, because
    /// Loom's journal, not the CLI's memory, is the record of the session.
    fn next_prompt(&mut self, request: &ModelRequest) -> (String, Option<String>) {
        if request.messages.len() < self.delivered {
            // Compaction removed messages the CLI had seen.
            self.claude_session = None;
        }
        let user_messages = |from: usize| {
            request.messages[from.min(request.messages.len())..]
                .iter()
                .filter(|message| message.role == MessageRole::User)
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>()
        };
        if self.claude_session.is_some() {
            return (user_messages(self.delivered).join("\n\n"), None);
        }
        let system = request
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::System)
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let turns = request
            .messages
            .iter()
            .filter(|message| matches!(message.role, MessageRole::User | MessageRole::Assistant))
            .collect::<Vec<_>>();
        let prompt = match turns.split_last() {
            Some((latest, earlier)) if latest.role == MessageRole::User && !earlier.is_empty() => {
                let recap = earlier
                    .iter()
                    .map(|message| {
                        let who = if message.role == MessageRole::User {
                            "User"
                        } else {
                            "Assistant"
                        };
                        format!("[{who}] {}", message.content)
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n");
                format!(
                    "This conversation has earlier messages, kept here as a transcript. \
                     Treat it as history and answer the latest message.\n\n{recap}\n\n\
                     Latest message:\n{}",
                    latest.content
                )
            }
            Some((latest, _)) if latest.role == MessageRole::User => latest.content.clone(),
            _ => String::new(),
        };
        (prompt, (!system.is_empty()).then_some(system))
    }

    fn options(&self, cwd: PathBuf, system: Option<String>) -> ClaudeCodeOptions {
        let mut options = ClaudeCodeOptions::new(cwd);
        options.binary = self.binary.clone();
        options.model = self.alias();
        options.resume = self.claude_session.clone();
        options.permission_mode = PermissionMode::AcceptEdits;
        options.append_system_prompt = system;
        options
    }
}

/// The first line of `text`, cut to a length a reasoning line can carry.
fn shorten(text: &str) -> String {
    let mut line = text.lines().next().unwrap_or_default().to_owned();
    if line.len() > SUMMARY_BYTES {
        let mut cut = SUMMARY_BYTES;
        while !line.is_char_boundary(cut) {
            cut -= 1;
        }
        line.truncate(cut);
        line.push('…');
    }
    line
}

/// A short account of a tool call, for the reasoning stream.
fn tool_summary(name: &str, input: &serde_json::Value) -> String {
    let detail = ["command", "file_path", "path", "pattern", "url", "query"]
        .iter()
        .find_map(|key| input.get(key).and_then(serde_json::Value::as_str))
        .map(shorten)
        .unwrap_or_default();
    if detail.is_empty() {
        format!("→ {name}\n")
    } else {
        format!("→ {name}: {detail}\n")
    }
}

fn emit(sink: &mut dyn ModelStreamSink, event: ModelStreamEvent) -> Result<StreamFlow> {
    sink.emit(event)
}

impl ModelProvider for ClaudeCodeProvider {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn bind_workspace(&mut self, root: &Path) {
        self.cwd = Some(root.to_path_buf());
    }

    fn stream(
        &mut self,
        request: &ModelRequest,
        cancel: &CancellationToken,
        sink: &mut dyn ModelStreamSink,
    ) -> Result<()> {
        let cwd = self.cwd.clone().ok_or_else(|| {
            LoomError::invalid_request("Claude Code needs the session's working directory")
        })?;
        let (prompt, system) = self.next_prompt(request);
        if prompt.trim().is_empty() {
            return Err(LoomError::invalid_request(
                "there is no new user message to send to Claude Code",
            ));
        }
        let mut session = ClaudeCodeSession::spawn(&self.options(cwd, system))?;
        session.send_user_message(&prompt)?;
        let mut stopping = false;
        let mut stop_deadline = None;
        loop {
            if !stopping && cancel.is_cancelled() {
                stopping = true;
                stop_deadline = Some(Instant::now() + INTERRUPT_GRACE);
                let _ = session.interrupt();
            }
            if stop_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(loom_model::cancelled_error());
            }
            let event = match session.next_event(POLL) {
                Next::Idle => continue,
                Next::Exited => {
                    return Err(LoomError::new(
                        ErrorCode::ProviderUnavailable,
                        format!(
                            "claude exited before finishing the turn: {}",
                            session.stderr_tail().trim()
                        ),
                        true,
                    ));
                }
                Next::Event(event) => event,
            };
            if stopping {
                if matches!(event, ClaudeEvent::Result(_)) {
                    return Err(loom_model::cancelled_error());
                }
                continue;
            }
            let reasoning = |text: String| ModelStreamEvent::ReasoningDelta { text };
            let flow = match event {
                ClaudeEvent::Init { session_id, .. } => {
                    self.claude_session = Some(session_id);
                    StreamFlow::Continue
                }
                ClaudeEvent::TextDelta {
                    text,
                    parent_tool_use_id: None,
                } => emit(sink, ModelStreamEvent::TextDelta { text })?,
                ClaudeEvent::TextDelta { .. } => StreamFlow::Continue,
                ClaudeEvent::ThinkingDelta { text, .. } => emit(sink, reasoning(text))?,
                ClaudeEvent::ToolUse { name, input, .. } => {
                    emit(sink, reasoning(tool_summary(&name, &input)))?
                }
                ClaudeEvent::ToolResult {
                    content,
                    is_error: true,
                    ..
                } => emit(sink, reasoning(format!("  ✗ {}\n", shorten(&content))))?,
                ClaudeEvent::PermissionRequest {
                    request_id,
                    tool_name,
                    ..
                } => {
                    session.respond_permission(
                        &request_id,
                        &PermissionDecision::Deny {
                            message: format!(
                                "{tool_name} needs approval and this worker cannot ask for it \
                                 yet. Do not retry it; continue without it or say what is needed."
                            ),
                        },
                    )?;
                    emit(
                        sink,
                        reasoning(format!("  ✗ {tool_name} needs approval; refused\n")),
                    )?
                }
                ClaudeEvent::PermissionDenied { tool_name, .. } => emit(
                    sink,
                    reasoning(format!("  ✗ {tool_name} was not allowed\n")),
                )?,
                ClaudeEvent::Result(summary) => {
                    if summary.is_error {
                        return Err(LoomError::new(
                            ErrorCode::ProviderUnavailable,
                            format!(
                                "Claude Code failed: {}",
                                summary.result.unwrap_or(summary.subtype)
                            ),
                            false,
                        ));
                    }
                    emit(
                        sink,
                        ModelStreamEvent::Usage {
                            usage: TokenUsage {
                                input_tokens: summary.usage.input + summary.usage.cache_creation,
                                output_tokens: summary.usage.output,
                                cached_input_tokens: summary.usage.cache_read,
                            },
                        },
                    )?;
                    emit(
                        sink,
                        ModelStreamEvent::Completed {
                            reason: FinishReason::Stop,
                        },
                    )?;
                    if let Some(id) = summary.session_id {
                        self.claude_session = Some(id);
                    }
                    // The reply the runtime stores next is one more message.
                    self.delivered = request.messages.len() + 1;
                    return Ok(());
                }
                ClaudeEvent::PermissionCancelled { .. }
                | ClaudeEvent::ToolResult { .. }
                | ClaudeEvent::RateLimit { .. }
                | ClaudeEvent::Other { .. } => StreamFlow::Continue,
            };
            if flow == StreamFlow::Stop {
                let _ = session.interrupt();
                return Ok(());
            }
        }
    }

    fn health_check(&mut self) -> Result<()> {
        let output = Command::new(&self.binary)
            .arg("--version")
            .output()
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::ProviderUnavailable,
                    format!("could not run '{}': {error}", self.binary.display()),
                    false,
                )
            })?;
        if output.status.success() {
            Ok(())
        } else {
            Err(LoomError::new(
                ErrorCode::ProviderUnavailable,
                format!("'{} --version' failed", self.binary.display()),
                false,
            ))
        }
    }

    fn reset(&mut self) {
        self.claude_session = None;
        self.delivered = 0;
    }
}

#[cfg(test)]
mod tests;
