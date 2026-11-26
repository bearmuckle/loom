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
//! A tool the CLI wants approved ends the turn. The provider refuses the
//! request, interrupts the CLI and returns a [`CLAUDE_CODE_PERMISSION_TOOL`]
//! call, which Loom's own policy and approval prompt judge like any other tool
//! call. Once the person approves it, the next turn tells the CLI to retry and
//! allows exactly that tool input. A decision lives in the transcript, so a
//! restarted worker still honours it. Edits inside the working directory and
//! read-only commands never ask.

use std::{path::Path, process::Command, time::Duration};

use loom_model::{
    CLAUDE_CODE_PERMISSION_APPROVED, CLAUDE_CODE_PERMISSION_TOOL, UsageLimitWindow, UsageLimits,
};

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
    /// Where this session's build output belongs, when the instance keeps one.
    build_cache: Option<PathBuf>,
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
            build_cache: None,
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
        let messages = &request.messages;
        if self.claude_session.is_some() {
            let news = (self.delivered.min(messages.len())..messages.len())
                .filter_map(|index| match messages[index].role {
                    MessageRole::User => Some(messages[index].content.clone()),
                    _ => decided_at(messages, index).map(|decided| decided.note()),
                })
                .collect::<Vec<_>>();
            if !news.is_empty() {
                return (news.join("\n\n"), None);
            }
            // Nothing is new, so the reply the last turn counted is not in the
            // transcript: the turn was cancelled or failed before its answer
            // was kept, and the counter is one ahead of the journal. Sending
            // nothing would fail the run, so continue from the journal.
            self.claude_session = None;
        }
        let answered = decided_at(messages, messages.len().saturating_sub(1));
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
        let recap_of = |turns: &[&ModelMessage]| {
            turns
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
                .join("\n\n")
        };
        let prompt = match turns.split_last() {
            _ if answered.is_some() => {
                let note = answered.map(|decided| decided.note()).unwrap_or_default();
                if turns.is_empty() {
                    note
                } else {
                    format!(
                        "This conversation has earlier messages, kept here as a transcript. \
                         Treat it as history.\n\n{}\n\n{note}",
                        recap_of(&turns)
                    )
                }
            }
            Some((latest, earlier)) if latest.role == MessageRole::User && !earlier.is_empty() => {
                let recap = recap_of(earlier);
                format!(
                    "This conversation has earlier messages, kept here as a transcript. \
                     Treat it as history and answer the latest message.\n\n{recap}\n\n\
                     Latest message:\n{}",
                    latest.content
                )
            }
            Some((latest, _)) if latest.role == MessageRole::User => latest.content.clone(),
            // The last kept turn is already a reply, as after a cancelled turn,
            // so the CLI is asked to carry on from it instead of being handed an
            // empty prompt.
            Some(_) => format!(
                "This conversation has earlier messages, kept here as a transcript. \
                 Treat it as history and continue from where it stops.\n\n{}",
                recap_of(&turns)
            ),
            None => String::new(),
        };
        (prompt, (!system.is_empty()).then_some(system))
    }

    /// Ends the turn with the call that asks the person to approve `tool`.
    fn ask_for_approval(
        &mut self,
        request: &ModelRequest,
        sink: &mut dyn ModelStreamSink,
        (tool, input): (String, serde_json::Value),
        summary: Option<loom_harness::ResultSummary>,
    ) -> Result<()> {
        if let Some(summary) = summary {
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
            if let Some(id) = summary.session_id {
                self.claude_session = Some(id);
            }
        }
        let summary = tool_summary(&tool, &input);
        emit(
            sink,
            ModelStreamEvent::ToolCallDelta {
                call: ToolCall {
                    id: ToolCallId::new(),
                    name: CLAUDE_CODE_PERMISSION_TOOL.to_owned(),
                    arguments: serde_json::json!({
                        "tool": tool,
                        "input": input,
                        "summary": summary.trim_start_matches("→ ").trim_end(),
                    }),
                },
            },
        )?;
        emit(
            sink,
            ModelStreamEvent::Completed {
                reason: FinishReason::ToolCall,
            },
        )?;
        self.delivered = request.messages.len() + 1;
        Ok(())
    }

    fn options(&self, cwd: PathBuf, system: Option<String>) -> ClaudeCodeOptions {
        let mut options = ClaudeCodeOptions::new(cwd);
        options.binary = self.binary.clone();
        options.model = self.alias();
        options.resume = self.claude_session.clone();
        options.permission_mode = PermissionMode::AcceptEdits;
        options.append_system_prompt = system;
        options.env = self.build_env();
        options
    }

    /// The environment the CLI runs builds with.
    ///
    /// The CLI runs its own commands in the session's checkout, so without this
    /// a `cargo build` writes a target directory inside the checkout, where the
    /// instance's build-cache retention cannot reclaim it. Agent builds also
    /// favour small output: full debug info makes a target directory several
    /// times larger, and incremental state is dead weight between turns.
    fn build_env(&self) -> Vec<(String, String)> {
        let mut env = vec![
            ("CARGO_INCREMENTAL".to_owned(), "0".to_owned()),
            ("CARGO_PROFILE_DEV_DEBUG".to_owned(), "0".to_owned()),
        ];
        if let Some(dir) = &self.build_cache {
            env.push((
                "CARGO_TARGET_DIR".to_owned(),
                dir.to_string_lossy().into_owned(),
            ));
        }
        env
    }
}

/// One answered permission call found in a transcript.
struct Decided {
    tool: String,
    input: serde_json::Value,
    approved: bool,
}

impl Decided {
    /// What the CLI is told about the decision.
    fn note(&self) -> String {
        if self.approved {
            format!(
                "The user approved {}. Run it again now with this input: {}",
                self.tool, self.input
            )
        } else {
            format!(
                "The user declined {}. Do not run it; continue another way or say what you need.",
                self.tool
            )
        }
    }
}

/// The permission call a tool message at `index` answers, when it is one.
fn decided_at(messages: &[ModelMessage], index: usize) -> Option<Decided> {
    let answer = messages.get(index)?;
    if answer.role != MessageRole::Tool
        || answer.name.as_deref() != Some(CLAUDE_CODE_PERMISSION_TOOL)
    {
        return None;
    }
    let call = messages[..index]
        .iter()
        .flat_map(|message| message.tool_calls.iter())
        .find(|call| Some(call.id) == answer.tool_call_id)?;
    Some(Decided {
        tool: call
            .arguments
            .get("tool")
            .and_then(serde_json::Value::as_str)?
            .to_owned(),
        input: call
            .arguments
            .get("input")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        approved: answer.content.trim() == CLAUDE_CODE_PERMISSION_APPROVED,
    })
}

/// Every tool input the person approved so far in this transcript.
fn approved_inputs(messages: &[ModelMessage]) -> Vec<(String, serde_json::Value)> {
    (0..messages.len())
        .filter_map(|index| decided_at(messages, index))
        .filter(|decided| decided.approved)
        .map(|decided| (decided.tool, decided.input))
        .collect()
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

/// Joins the text of the assistant messages in one Claude Code turn.
///
/// The CLI streams each message's text on its own, and a tool call sits
/// between two messages. Streamed back to back, the second message would start
/// on the last word of the first, so it starts a new paragraph instead.
#[derive(Default)]
struct ReplyText {
    last: Option<char>,
    new_message: bool,
}

impl ReplyText {
    /// The text to emit for one delta of the reply.
    fn delta(&mut self, text: String) -> String {
        if text.is_empty() {
            return text;
        }
        let previous = self.last;
        self.last = text.chars().last();
        if !std::mem::take(&mut self.new_message) {
            return text;
        }
        let gap = match previous {
            None => "",
            Some(_) if text.starts_with('\n') => "",
            Some('\n') => "\n",
            Some(_) => "\n\n",
        };
        format!("{gap}{text}")
    }

    /// Marks that a tool call ended the current message.
    fn tool_call(&mut self) {
        self.new_message = true;
    }

    /// Whether no text was emitted yet.
    fn is_empty(&self) -> bool {
        self.last.is_none()
    }
}

fn emit(sink: &mut dyn ModelStreamSink, event: ModelStreamEvent) -> Result<StreamFlow> {
    sink.emit(event)
}

/// The plan limits from a [`ClaudeEvent::RateLimit`], with each window's share
/// as a rounded percentage.
fn usage_limits(
    status: String,
    using_overage: bool,
    windows: Vec<loom_harness::RateLimitWindow>,
    reported_at: u64,
) -> UsageLimits {
    UsageLimits {
        status,
        using_overage,
        reported_at: Some(reported_at),
        windows: windows
            .into_iter()
            .map(|window| UsageLimitWindow {
                name: window.name,
                used_percent: window
                    .utilization
                    .filter(|share| share.is_finite())
                    .map(|share| (share.clamp(0.0, 10.0) * 100.0).round() as u32),
                resets_at: window.resets_at,
                status: window.status,
            })
            .collect(),
    }
}

/// Why the account cannot run this turn, when it cannot.
fn limit_reached_message(limits: &UsageLimits) -> Option<String> {
    if limits.status != "rejected" {
        return None;
    }
    let blocked = limits
        .windows
        .iter()
        .find(|window| window.status.as_deref() == Some("rejected"))
        .or_else(|| limits.windows.first());
    let Some(window) = blocked else {
        return Some("Claude Code usage limit reached".to_owned());
    };
    Some(match window.resets_at {
        Some(at) => format!(
            "Claude Code usage limit reached: the {} window resets {}",
            window.label(),
            wait_until(at)
        ),
        None => format!(
            "Claude Code usage limit reached: the {} window is used up",
            window.label()
        ),
    })
}

/// What a turn that ended with no reply is reported as.
fn empty_turn_message(limits: Option<&UsageLimits>) -> String {
    let used = limits
        .map(|limits| {
            limits
                .windows
                .iter()
                .filter_map(|window| {
                    window
                        .used_percent
                        .map(|percent| format!("the {} window {percent}% used", window.label()))
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    if used.is_empty() {
        "Claude Code ended the turn without a reply".to_owned()
    } else {
        format!("Claude Code ended the turn without a reply; it last reported {used}")
    }
}

/// The wait until `at` in the roughest useful words, such as `in about 3h 12m`.
fn wait_until(at: u64) -> String {
    let now = loom_core::Timestamp::now().as_unix_millis() / 1_000;
    let Some(seconds) = at.checked_sub(now) else {
        return "now".to_owned();
    };
    match seconds {
        0..=90 => "in about a minute".to_owned(),
        seconds if seconds < 3_600 => format!("in about {} minutes", seconds.div_ceil(60)),
        seconds => {
            let (hours, minutes) = (seconds / 3_600, (seconds % 3_600) / 60);
            match minutes {
                0 => format!("in about {hours}h"),
                minutes => format!("in about {hours}h {minutes}m"),
            }
        }
    }
}

impl ModelProvider for ClaudeCodeProvider {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn bind_workspace(&mut self, root: &Path) {
        self.cwd = Some(root.to_path_buf());
    }

    fn bind_build_cache(&mut self, dir: Option<&Path>) {
        self.build_cache = dir.map(Path::to_path_buf);
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
        let approved = approved_inputs(&request.messages);
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
        // The tool the CLI asked for that the person has not approved yet.
        let mut asked: Option<(String, serde_json::Value)> = None;
        let mut reply = ReplyText::default();
        // The limits the CLI reported during this turn, if any.
        let mut seen_limits: Option<UsageLimits> = None;
        loop {
            if !stopping && cancel.is_cancelled() {
                stopping = true;
                stop_deadline = Some(Instant::now() + INTERRUPT_GRACE);
                let _ = session.interrupt();
            }
            if stop_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                if let (Some(tool), false) = (asked.take(), cancel.is_cancelled()) {
                    return self.ask_for_approval(request, sink, tool, None);
                }
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
                if let ClaudeEvent::Result(summary) = event {
                    if let (Some(tool), false) = (asked.take(), cancel.is_cancelled()) {
                        return self.ask_for_approval(request, sink, tool, Some(summary));
                    }
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
                } => emit(
                    sink,
                    ModelStreamEvent::TextDelta {
                        text: reply.delta(text),
                    },
                )?,
                ClaudeEvent::TextDelta { .. } => StreamFlow::Continue,
                ClaudeEvent::ThinkingDelta { text, .. } => emit(sink, reasoning(text))?,
                ClaudeEvent::ToolUse {
                    name,
                    input,
                    parent_tool_use_id,
                    ..
                } => {
                    if parent_tool_use_id.is_none() {
                        reply.tool_call();
                    }
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
                    input,
                    ..
                } => {
                    if approved
                        .iter()
                        .any(|(tool, allowed)| *tool == tool_name && *allowed == input)
                    {
                        session.respond_permission(
                            &request_id,
                            &PermissionDecision::Allow { input },
                        )?;
                        emit(sink, reasoning(format!("  ✓ {tool_name} approved\n")))?
                    } else {
                        session.respond_permission(
                            &request_id,
                            &PermissionDecision::Deny {
                                message: format!(
                                    "{tool_name} needs the user's approval. Stop now and wait; \
                                     do not retry it or work around it."
                                ),
                            },
                        )?;
                        if asked.is_none() {
                            asked = Some((tool_name.clone(), input));
                            stopping = true;
                            stop_deadline = Some(Instant::now() + INTERRUPT_GRACE);
                            let _ = session.interrupt();
                        }
                        emit(
                            sink,
                            reasoning(format!("  ? {tool_name} waits for approval\n")),
                        )?
                    }
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
                    if reply.is_empty() {
                        // Nothing reached the stream, so the turn would end with
                        // no message at all. A CLI that kept the reply for its
                        // final message still said something, so stream that;
                        // otherwise say what happened instead of completing
                        // silently, as a blocked account does.
                        let kept = summary
                            .result
                            .as_deref()
                            .map(str::trim)
                            .filter(|text| !text.is_empty());
                        match kept {
                            Some(text) => {
                                emit(
                                    sink,
                                    ModelStreamEvent::TextDelta {
                                        text: text.to_owned(),
                                    },
                                )?;
                            }
                            None => {
                                return Err(LoomError::new(
                                    ErrorCode::ProviderUnavailable,
                                    empty_turn_message(seen_limits.as_ref()),
                                    false,
                                ));
                            }
                        }
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
                ClaudeEvent::RateLimit {
                    status,
                    is_using_overage,
                    windows,
                    ..
                } => {
                    let limits = usage_limits(
                        status,
                        is_using_overage,
                        windows,
                        loom_core::Timestamp::now().as_unix_millis() / 1_000,
                    );
                    let blocked = limit_reached_message(&limits);
                    emit(
                        sink,
                        ModelStreamEvent::Limits {
                            limits: limits.clone(),
                        },
                    )?;
                    seen_limits = Some(limits);
                    if let Some(message) = blocked {
                        // The account cannot run this turn, so fail with the
                        // reason instead of letting the turn end empty.
                        return Err(LoomError::new(
                            ErrorCode::ProviderUnavailable,
                            message,
                            false,
                        ));
                    }
                    StreamFlow::Continue
                }
                ClaudeEvent::PermissionCancelled { .. }
                | ClaudeEvent::ToolResult { .. }
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
