//! The response envelope.
//!
//! Every operation returns one JSON object, and the shape is fixed so a host
//! can parse it once:
//!
//! ```json
//! {"ok": true,  "data": {...}}
//! {"ok": false, "error": {"code": "invalid", "message": "..."}}
//! ```

use serde_json::{Value, json};

use bas_core::error::Code;

/// One response.
#[derive(Debug, Clone)]
pub enum Reply {
    Ok(Value),
    Err { code: String, message: String },
}

impl Reply {
    /// A successful response.
    #[must_use]
    pub fn ok(data: Value) -> Self {
        Self::Ok(data)
    }

    /// A failed response with a stable code.
    #[must_use]
    pub fn err(code: &str, message: impl Into<String>) -> Self {
        Self::Err {
            code: code.to_string(),
            message: message.into(),
        }
    }

    /// A failed response from a core error.
    #[must_use]
    pub fn from_error(error: bas_core::Error) -> Self {
        Self::err(error.code.as_str(), error.message)
    }

    /// A failed response for an unusable argument.
    #[must_use]
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::err(Code::Invalid.as_str(), message)
    }

    /// Serialise to the wire form.
    #[must_use]
    pub fn to_json(&self) -> String {
        match self {
            Reply::Ok(data) => json!({ "ok": true, "data": data }).to_string(),
            Reply::Err { code, message } => json!({
                "ok": false,
                "error": { "code": code, "message": message }
            })
            .to_string(),
        }
    }
}

impl From<bas_core::Error> for Reply {
    fn from(value: bas_core::Error) -> Self {
        Self::from_error(value)
    }
}

impl<T: serde::Serialize> From<Result<T, bas_core::Error>> for Reply {
    fn from(value: Result<T, bas_core::Error>) -> Self {
        match value {
            Ok(data) => match serde_json::to_value(data) {
                Ok(value) => Reply::ok(value),
                Err(error) => {
                    Reply::err("internal", format!("cannot serialise the result: {error}"))
                }
            },
            Err(error) => Reply::from_error(error),
        }
    }
}
