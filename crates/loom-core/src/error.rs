use std::{fmt, time::Duration};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    InvalidState,
    MalformedPayload,
    AuthenticationRequired,
    AuthenticationFailed,
    AuthorizationDenied,
    NotFound,
    Conflict,
    CapabilityDenied,
    ApprovalRequired,
    WorkspaceAccessDenied,
    ProcessCancelled,
    UnsupportedProtocol,
    ProviderUnavailable,
    ProviderAuthentication,
    ProviderRateLimited,
    ProviderInvalidResponse,
    ToolExecution,
    Persistence,
    ContextLimitExceeded,
    SessionLimitExceeded,
    RecoveryRequired,
    UnsupportedCapability,
    FileTooLarge,
    InvalidEncoding,
    ExternalChange,
    Vcs,
    RequestCancelled,
    DeadlineExceeded,
    Backpressure,
    Internal,
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::InvalidRequest => "invalid_request",
            Self::InvalidState => "invalid_state",
            Self::MalformedPayload => "malformed_payload",
            Self::AuthenticationRequired => "authentication_required",
            Self::AuthenticationFailed => "authentication_failed",
            Self::AuthorizationDenied => "authorization_denied",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::CapabilityDenied => "capability_denied",
            Self::ApprovalRequired => "approval_required",
            Self::WorkspaceAccessDenied => "workspace_access_denied",
            Self::ProcessCancelled => "process_cancelled",
            Self::UnsupportedProtocol => "unsupported_protocol",
            Self::ProviderUnavailable => "provider_unavailable",
            Self::ProviderAuthentication => "provider_authentication",
            Self::ProviderRateLimited => "provider_rate_limited",
            Self::ProviderInvalidResponse => "provider_invalid_response",
            Self::ToolExecution => "tool_execution",
            Self::Persistence => "persistence",
            Self::ContextLimitExceeded => "context_limit_exceeded",
            Self::SessionLimitExceeded => "session_limit_exceeded",
            Self::RecoveryRequired => "recovery_required",
            Self::UnsupportedCapability => "unsupported_capability",
            Self::FileTooLarge => "file_too_large",
            Self::InvalidEncoding => "invalid_encoding",
            Self::ExternalChange => "external_change",
            Self::Vcs => "vcs",
            Self::RequestCancelled => "request_cancelled",
            Self::DeadlineExceeded => "deadline_exceeded",
            Self::Backpressure => "backpressure",
            Self::Internal => "internal",
        };
        formatter.write_str(value)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Error, PartialEq, Serialize)]
#[error("{code}: {message}")]
pub struct LoomError {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
    /// An explicit delay hint for a retry, such as a `Retry-After` header.
    ///
    /// Transient transport metadata; it is never serialized so persisted and
    /// protocol errors keep their existing shape.
    #[serde(default, skip)]
    pub retry_after: Option<Duration>,
}

impl LoomError {
    pub fn new(code: ErrorCode, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
            retry_after: None,
        }
    }

    /// Attaches a server-provided delay hint used by the provider retry loop.
    pub fn with_retry_after(mut self, retry_after: Option<Duration>) -> Self {
        self.retry_after = retry_after;
        self
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidRequest, message, false)
    }

    pub fn invalid_state(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidState, message, false)
    }

    pub fn malformed_payload(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::MalformedPayload, message, false)
    }

    pub fn not_found(resource: &str, id: impl fmt::Display) -> Self {
        Self::new(
            ErrorCode::NotFound,
            format!("{resource} '{id}' was not found"),
            false,
        )
    }

    pub fn unsupported_protocol(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::UnsupportedProtocol, message, false)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Conflict, message, false)
    }

    pub fn approval_required(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::ApprovalRequired, message, false)
    }
}

pub type Result<T> = std::result::Result<T, LoomError>;

#[cfg(test)]
mod tests {
    use super::{ErrorCode, LoomError};

    #[test]
    fn error_codes_are_stable_and_errors_display_their_message() {
        let cases = [
            (ErrorCode::InvalidRequest, "invalid_request"),
            (ErrorCode::InvalidState, "invalid_state"),
            (ErrorCode::MalformedPayload, "malformed_payload"),
            (ErrorCode::AuthenticationRequired, "authentication_required"),
            (ErrorCode::AuthenticationFailed, "authentication_failed"),
            (ErrorCode::AuthorizationDenied, "authorization_denied"),
            (ErrorCode::NotFound, "not_found"),
            (ErrorCode::Conflict, "conflict"),
            (ErrorCode::CapabilityDenied, "capability_denied"),
            (ErrorCode::ApprovalRequired, "approval_required"),
            (ErrorCode::WorkspaceAccessDenied, "workspace_access_denied"),
            (ErrorCode::ProcessCancelled, "process_cancelled"),
            (ErrorCode::UnsupportedProtocol, "unsupported_protocol"),
            (ErrorCode::ProviderUnavailable, "provider_unavailable"),
            (ErrorCode::ProviderAuthentication, "provider_authentication"),
            (ErrorCode::ProviderRateLimited, "provider_rate_limited"),
            (
                ErrorCode::ProviderInvalidResponse,
                "provider_invalid_response",
            ),
            (ErrorCode::ToolExecution, "tool_execution"),
            (ErrorCode::Persistence, "persistence"),
            (ErrorCode::ContextLimitExceeded, "context_limit_exceeded"),
            (ErrorCode::SessionLimitExceeded, "session_limit_exceeded"),
            (ErrorCode::RecoveryRequired, "recovery_required"),
            (ErrorCode::UnsupportedCapability, "unsupported_capability"),
            (ErrorCode::FileTooLarge, "file_too_large"),
            (ErrorCode::InvalidEncoding, "invalid_encoding"),
            (ErrorCode::ExternalChange, "external_change"),
            (ErrorCode::Vcs, "vcs"),
            (ErrorCode::RequestCancelled, "request_cancelled"),
            (ErrorCode::DeadlineExceeded, "deadline_exceeded"),
            (ErrorCode::Backpressure, "backpressure"),
            (ErrorCode::Internal, "internal"),
        ];
        for (code, name) in cases {
            assert_eq!(code.to_string(), name);
            assert_eq!(serde_json::to_string(&code).unwrap(), format!("\"{name}\""));
        }

        let error = LoomError::not_found("run", "abc");
        assert_eq!(error.to_string(), "not_found: run 'abc' was not found");
        assert!(!error.retryable);
        assert!(LoomError::new(ErrorCode::Internal, "oops", true).retryable);
        assert_eq!(
            LoomError::invalid_request("bad").code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            LoomError::invalid_state("bad").code,
            ErrorCode::InvalidState
        );
        assert_eq!(
            LoomError::malformed_payload("bad").code,
            ErrorCode::MalformedPayload
        );
        assert_eq!(
            LoomError::unsupported_protocol("bad").code,
            ErrorCode::UnsupportedProtocol
        );
        assert_eq!(LoomError::conflict("bad").code, ErrorCode::Conflict);
        assert_eq!(
            LoomError::approval_required("bad").code,
            ErrorCode::ApprovalRequired
        );
    }
}
