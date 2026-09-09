use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    MalformedPayload,
    NotFound,
    Conflict,
    CapabilityDenied,
    UnsupportedProtocol,
    Internal,
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::InvalidRequest => "invalid_request",
            Self::MalformedPayload => "malformed_payload",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::CapabilityDenied => "capability_denied",
            Self::UnsupportedProtocol => "unsupported_protocol",
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
}

pub type Result<T> = std::result::Result<T, LoomError>;
