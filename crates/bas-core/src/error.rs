//! Error type of the protocol layer.
//!
//! Errors carry a stable `code` so a host can branch on the failure without
//! parsing prose: the C ABI surfaces it as the JSON `error.code` field and the
//! CLI maps it to an exit code.

use serde::{Deserialize, Serialize};

/// Machine-readable failure classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Code {
    /// The landscape or dev space refused the request (HTTP or protocol error).
    Upstream,
    /// The transport failed (connect, TLS, WebSocket, SSH).
    Transport,
    /// The token is missing or expired.
    TokenExpired,
    /// The requested dev space is not in the listing the landscape returned.
    NotFound,
    /// A port is already in use and the caller asked for strict ports.
    PortInUse,
    /// The response did not have the expected shape.
    BadBody,
    /// The caller passed something unusable.
    Invalid,
    /// The dev space reached an unexpected status while waiting.
    Status,
    /// Anything else.
    Other,
}

impl Code {
    /// Stable string form, used in the JSON envelope.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Code::Upstream => "upstream",
            Code::Transport => "transport",
            Code::TokenExpired => "token_expired",
            Code::NotFound => "not_found",
            Code::PortInUse => "port_in_use",
            Code::BadBody => "bad_body",
            Code::Invalid => "invalid",
            Code::Status => "status",
            Code::Other => "other",
        }
    }
}

/// One protocol-layer failure.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct Error {
    pub code: Code,
    pub message: String,
    #[source]
    pub source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl Error {
    /// Build an error with no underlying source.
    #[must_use]
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            source: None,
        }
    }

    /// Attach an underlying cause.
    #[must_use]
    pub fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    /// Shorthand for [`Code::Invalid`].
    #[must_use]
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(Code::Invalid, message)
    }

    /// Shorthand for [`Code::Transport`].
    #[must_use]
    pub fn transport(message: impl Into<String>) -> Self {
        Self::new(Code::Transport, message)
    }

    /// Shorthand for [`Code::Upstream`].
    #[must_use]
    pub fn upstream(message: impl Into<String>) -> Self {
        Self::new(Code::Upstream, message)
    }
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;

impl From<reqwest::Error> for Error {
    fn from(value: reqwest::Error) -> Self {
        let message = value.to_string();
        Error::transport(message).with_source(value)
    }
}

impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        let message = value.to_string();
        Error::new(Code::BadBody, message).with_source(value)
    }
}

impl From<url::ParseError> for Error {
    fn from(value: url::ParseError) -> Self {
        let message = value.to_string();
        Error::invalid(message).with_source(value)
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        let message = value.to_string();
        Error::transport(message).with_source(value)
    }
}
