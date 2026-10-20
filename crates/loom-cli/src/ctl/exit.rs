//! Exit codes, pinned by the contract and by the mapping test.

use loom_core::{ErrorCode, LoomError};

pub(crate) const OK: i32 = 0;
pub(crate) const INTERNAL: i32 = 1;
pub(crate) const USAGE: i32 = 2;
pub(crate) const BLOCKED: i32 = 3;
pub(crate) const TIMEOUT: i32 = 4;
pub(crate) const GONE: i32 = 5;
pub(crate) const MISMATCH: i32 = 6;
pub(crate) const UNREACHABLE: i32 = 7;

/// The exit code a protocol error maps to.
pub(crate) const fn for_code(code: ErrorCode) -> i32 {
    match code {
        ErrorCode::ApprovalRequired
        | ErrorCode::CapabilityDenied
        | ErrorCode::AuthorizationDenied
        | ErrorCode::WorkspaceAccessDenied => BLOCKED,
        ErrorCode::DeadlineExceeded | ErrorCode::Backpressure => TIMEOUT,
        ErrorCode::NotFound | ErrorCode::InvalidState => GONE,
        ErrorCode::UnsupportedProtocol
        | ErrorCode::AuthenticationRequired
        | ErrorCode::AuthenticationFailed => MISMATCH,
        _ => INTERNAL,
    }
}

/// The exit code a failed connection maps to. A refused credential is a
/// protocol or authentication mismatch rather than an unreachable worker; any
/// other connection failure never reached the worker at all.
pub(crate) fn for_connect_error(error: &LoomError) -> i32 {
    match error.code {
        ErrorCode::AuthenticationRequired | ErrorCode::AuthenticationFailed => MISMATCH,
        _ => UNREACHABLE,
    }
}
