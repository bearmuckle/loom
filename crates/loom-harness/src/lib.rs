//! Agent harnesses Loom can drive instead of running its own loop.
//!
//! A harness is an external agent that owns its conversation, its tool
//! execution and its context. [`claude_code`] drives the Claude Code CLI in its
//! headless `stream-json` mode over a piped child process, using the `claude`
//! binary the person installed and signed in to themselves. Loom never reads,
//! stores or forwards that login.

pub mod claude_code;

pub use claude_code::{
    ClaudeCodeOptions, ClaudeCodeSession, ClaudeEvent, Next, PermissionDecision, PermissionMode,
    ResultSummary, TokenCounts, parse_line,
};
