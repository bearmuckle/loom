//! Thin Agent Client Protocol (ACP v1) host for external agent harnesses.
//!
//! Loom launches the operator's official, unmodified harness binary as a
//! supervised piped-stdio subprocess (newline-delimited JSON-RPC 2.0, never a
//! PTY) and drives the standard ACP lifecycle:
//!
//! `initialize` → `authenticate` → `session/new` (or `session/load` /
//! `session/resume` only when advertised) → `session/prompt` / `session/cancel`.
//!
//! The harness owns its own authentication, conversation state, context window,
//! and tool execution. Loom never reads, copies, logs, or stores harness
//! credentials or token files, and never injects Loom's system prompt, tool
//! list, or context assembler into the harness.

pub mod antigravity;
pub mod client;
#[cfg(any(test, feature = "test-support"))]
pub mod stub;

#[cfg(test)]
mod tests;

use std::path::PathBuf;

use loom_core::{AcpCapabilities, AcpModelCatalogEntry};
use serde_json::Value;

/// ACP protocol version supported by this host slice.
pub const ACP_PROTOCOL_VERSION: u32 = 1;

/// Client identity sent to ACP agents during `initialize`.
///
/// Always Loom's own identity — never another product's client id or name.
pub const LOOM_ACP_CLIENT_NAME: &str = "loom";
pub const LOOM_ACP_CLIENT_TITLE: &str = "Loom";
pub const LOOM_ACP_CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Resolved command line for spawning an ACP harness subprocess.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcpCommandSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Human-readable command string recorded on the session (never carries secrets).
    pub display: String,
    /// Whether this command invokes the official `agy acp` CLI subcommand and
    /// accepts `--app-data-dir` / `--gemini-dir` profile flags.
    pub supports_agy_profile_flags: bool,
}

/// Options for launching one supervised ACP harness process.
#[derive(Clone, Debug)]
pub struct AcpOptions {
    pub command: AcpCommandSpec,
    /// Session working directory passed as the child's cwd and `session/new` cwd.
    pub cwd: PathBuf,
    /// Isolated per-session profile directory chosen by Loom. Loom only creates
    /// this directory path and passes it to the harness; Loom never reads any
    /// file inside it.
    pub profile_dir: Option<PathBuf>,
    /// Non-secret environment overrides (for example `CARGO_TARGET_DIR`).
    pub env: Vec<(String, String)>,
}

/// Auth method advertised by the agent in `initialize`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcpAuthMethod {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

/// Result of the ACP `initialize` handshake.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcpInitializeResult {
    pub protocol_version: u32,
    pub agent_name: Option<String>,
    pub agent_version: Option<String>,
    pub capabilities: AcpCapabilities,
    pub auth_methods: Vec<AcpAuthMethod>,
}

/// Result of `session/new`, `session/load`, or `session/resume`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcpSessionInfo {
    pub session_id: String,
    pub current_model_id: Option<String>,
    pub models: Vec<AcpModelCatalogEntry>,
    /// If the agent exposed models via `configOptions`, the config option id
    /// to pass to `session/set_config_option`.
    pub model_config_option_id: Option<String>,
}

/// One ACP tool call reported by `session/update` (`tool_call`).
#[derive(Clone, Debug, PartialEq)]
pub struct AcpToolCall {
    pub tool_call_id: String,
    pub title: String,
    pub kind: Option<String>,
    pub status: Option<String>,
    pub raw_input: Value,
    pub locations: Vec<String>,
}

/// Update to an existing ACP tool call (`tool_call_update`).
#[derive(Clone, Debug, PartialEq)]
pub struct AcpToolCallUpdate {
    pub tool_call_id: String,
    pub title: Option<String>,
    pub kind: Option<String>,
    pub status: Option<String>,
    pub raw_input: Option<Value>,
    pub raw_output: Option<String>,
}

/// One step in an ACP `plan` update.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcpPlanEntry {
    pub content: String,
    pub priority: Option<String>,
    pub status: Option<String>,
}

/// Usage update emitted by the harness (`usage_update` or `session/prompt` result).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AcpUsageUpdate {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub context_used: Option<u64>,
    pub context_size: Option<u64>,
    pub cost_usd: Option<f64>,
}

/// One option offered by the agent in `session/request_permission`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcpPermissionOption {
    pub option_id: String,
    pub name: String,
    /// ACP option kind: `"allow_once"`, `"allow_always"`, `"reject_once"`, `"reject_always"`.
    pub kind: String,
}

/// Live permission request sent by the ACP agent during a prompt turn.
#[derive(Clone, Debug, PartialEq)]
pub struct AcpPermissionRequest {
    pub request_id: Value,
    pub session_id: Option<String>,
    pub tool_call_id: Option<String>,
    pub title: String,
    pub kind: Option<String>,
    pub raw_input: Value,
    pub options: Vec<AcpPermissionOption>,
}

impl AcpPermissionRequest {
    /// Selects a single-action allow option id offered by the agent.
    ///
    /// Never selects `"allow_always"` (which would silently disable future
    /// interactive approvals) and never falls back to an arbitrary first option.
    pub fn allow_option_id(&self) -> Option<&str> {
        self.options
            .iter()
            .find(|option| option.kind == "allow_once")
            .or_else(|| self.options.iter().find(|option| option.kind == "allow"))
            .map(|option| option.option_id.as_str())
    }

    /// Selects the preferred reject option id offered by the agent.
    pub fn reject_option_id(&self) -> Option<&str> {
        self.options
            .iter()
            .find(|option| option.kind == "reject_once")
            .or_else(|| {
                self.options
                    .iter()
                    .find(|option| option.kind == "reject_always")
            })
            .or_else(|| {
                self.options
                    .iter()
                    .find(|option| option.kind.starts_with("reject"))
            })
            .map(|option| option.option_id.as_str())
    }
}

/// Host's answer to a live `session/request_permission` request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AcpPermissionOutcome {
    Selected { option_id: String },
    Cancelled,
}

/// Terminal result of a `session/prompt` request.
#[derive(Clone, Debug, PartialEq)]
pub struct AcpPromptResult {
    pub request_id: u64,
    pub stop_reason: String,
    pub usage: Option<AcpUsageUpdate>,
}

/// Events streamed from an active [`client::AcpClient`].
#[derive(Clone, Debug, PartialEq)]
pub enum AcpEvent {
    MessageChunk(String),
    ThoughtChunk(String),
    ToolCall(AcpToolCall),
    ToolCallUpdate(AcpToolCallUpdate),
    Plan(Vec<AcpPlanEntry>),
    UsageUpdate(AcpUsageUpdate),
    ModelsUpdated {
        models: Vec<AcpModelCatalogEntry>,
        current_model_id: Option<String>,
        model_config_option_id: Option<String>,
    },
    PermissionRequest(AcpPermissionRequest),
    PromptFinished(AcpPromptResult),
    PromptFailed {
        request_id: u64,
        code: i64,
        message: String,
    },
    StderrLine(String),
    Other {
        kind: String,
    },
}

/// What [`client::AcpClient::next_event`] observed.
#[derive(Clone, Debug, PartialEq)]
pub enum AcpNext {
    Event(Box<AcpEvent>),
    Idle,
    Exited {
        exit_code: Option<i32>,
        stderr_tail: String,
    },
}

pub use antigravity::{
    ANTIGRAVITY_COMMAND_ENV, AntigravityRunContext, antigravity_notices, check_interactive_only,
    check_interactive_only_with_ci_policy, check_interactive_only_with_env,
    discover_antigravity_command, redact_acp_text, validate_auth_method,
};
pub use client::{AcpClient, AcpKillHandle};
