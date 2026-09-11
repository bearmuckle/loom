use std::fmt;

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
}

impl LoomError {
    pub fn new(code: ErrorCode, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
        }
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
