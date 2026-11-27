//! `loom-mcp`: an MCP server that mirrors `loom-cli`.
//!
//! It is a client of the existing versioned WebSocket protocol (through the
//! `loom-cli` library surface), not a second backend. The MCP surface exposes
//! one tool per first-slice verb, session logs as resources, and either
//! streamable HTTP or stdio transport.

pub mod config;
pub mod http;
pub mod jsonrpc;
pub mod resources;
pub mod runner;
pub mod stdio;
pub mod tools;

pub use config::{AppState, Config, Secret, TlsConfig, validate_bind};
