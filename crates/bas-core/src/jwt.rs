//! Landscape JWT handling.
//!
//! BAS hands the harness a JWT through the browser hand-off. Nothing here
//! verifies the signature — the landscape does that — but the expiry is read
//! so a host can tell "signed in" from "signed in, but the token died".

use base64::Engine;
use serde::Deserialize;

use crate::model::JwtInfo;

/// The subset of the payload this crate cares about.
#[derive(Debug, Deserialize)]
pub struct Claims {
    #[serde(default)]
    exp: Option<i64>,
    #[serde(default)]
    sub: Option<String>,
    #[serde(default)]
    email: Option<String>,
}

/// Decode the payload of a compact JWS without verifying it.
///
/// Returns `None` when the token is not a readable JWT at all (some landscapes
/// hand out an opaque IAS token instead).
#[must_use]
pub fn claims(token: &str) -> Option<Claims> {
    let payload = token.split('.').nth(1)?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&raw).ok()
}

/// Describe a token's usability at `now_ms` (Unix milliseconds).
///
/// `skew_ms` treats a token that expires sooner as already expired, so a caller
/// does not start a long operation with a token that dies mid-flight.
#[must_use]
pub fn describe(token: &str, now_ms: i64, skew_ms: i64) -> JwtInfo {
    let Some(claims) = claims(token) else {
        // An opaque token cannot be dated; assume the landscape will say no.
        return JwtInfo {
            valid: !token.is_empty(),
            remaining_ms: None,
            expires_at: None,
            subject: None,
        };
    };
    let subject = claims.sub.or(claims.email);
    match claims.exp {
        Some(exp) => {
            let expires_at_ms = exp.saturating_mul(1000);
            let remaining = expires_at_ms - now_ms;
            JwtInfo {
                valid: remaining > skew_ms,
                remaining_ms: Some(remaining),
                expires_at: Some(exp),
                subject,
            }
        }
        None => JwtInfo {
            valid: true,
            remaining_ms: None,
            expires_at: None,
            subject,
        },
    }
}

/// Whether a token is usable at `now_ms`, allowing `skew_ms` of headroom.
#[must_use]
pub fn usable(token: &str, now_ms: i64, skew_ms: i64) -> bool {
    !token.is_empty() && describe(token, now_ms, skew_ms).valid
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a token with a given `exp` so the tests do not need a real one.
    fn token_with(exp: Option<i64>) -> String {
        let payload = match exp {
            Some(exp) => format!(r#"{{"sub":"u","exp":{exp}}}"#),
            None => r#"{"sub":"u"}"#.to_string(),
        };
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
        format!("header.{encoded}.signature")
    }

    #[test]
    fn reads_expiry() {
        let token = token_with(Some(2_000));
        let info = describe(&token, 1_000_000, 60_000);
        assert_eq!(info.expires_at, Some(2_000));
        assert_eq!(info.remaining_ms, Some(1_000_000));
        assert!(info.valid);
        assert_eq!(info.subject.as_deref(), Some("u"));
    }

    #[test]
    fn honours_skew() {
        let token = token_with(Some(2_000));
        // 30 s left, 60 s of skew: treated as expired.
        assert!(!describe(&token, 1_970_000, 60_000).valid);
    }

    #[test]
    fn token_without_exp_is_usable() {
        let token = token_with(None);
        assert!(usable(&token, i64::MAX / 2, 60_000));
    }

    #[test]
    fn opaque_token_is_usable_but_undated() {
        let info = describe("opaque", 0, 0);
        assert!(info.valid);
        assert!(info.remaining_ms.is_none());
        assert!(!usable("", 0, 0));
    }
}
