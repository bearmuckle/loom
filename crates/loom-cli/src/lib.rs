//! Library surface of `loom-cli`, for embedding binaries such as `loom-mcp`.
//!
//! The binary keeps its own thin `main`; everything a caller can embed lives
//! under [`cli`], which re-exports the client seam, the structured execution
//! result, and the connection and token-file resolvers the binary itself uses.

pub mod cli;
