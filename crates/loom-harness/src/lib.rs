//! Agent harnesses Loom can drive instead of running its own loop.
//!
//! A harness is an external agent that owns its conversation, its tool
//! execution and its context. [`claude_code`] drives the Claude Code CLI in its
//! headless `stream-json` mode over a piped child process, and [`acp`] drives
//! Agent Client Protocol (ACP v1) harnesses such as Google's official `agy acp`
//! over piped JSON-RPC 2.0 stdio. Loom never reads, stores or forwards harness
//! logins or credentials.

pub mod acp;
pub mod claude_code;

pub use claude_code::{
    ClaudeCodeOptions, ClaudeCodeSession, ClaudeEvent, Next, PermissionDecision, PermissionMode,
    RateLimitWindow, ResultSummary, TokenCounts, parse_line,
};
