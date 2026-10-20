//! The browser sign-in hand-off.
//!
//! A BAS landscape signs in through the browser: the harness opens
//! `<landscape>/ext-login.html`, the user authenticates, and the page POSTs the
//! token back to a loopback listener. That listener is the stateful part — it
//! owns a port and a pending token — so it is handle-based like a tunnel.
//!
//! The token crosses this boundary exactly once: `login_poll` hands it to the
//! host, which stores it in its own credential store. Nothing here caches it.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use bas_core::Result;
use bas_core::error::{Code, Error};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;

/// How long one sign-in may stay pending before the listener gives up.
pub const DEFAULT_LOGIN_TIMEOUT_SECS: u64 = 180;

/// Where a sign-in attempt stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginState {
    Waiting,
    /// The token arrived and has not been collected yet.
    Done {
        jwt: String,
        iasjwt: String,
    },
    /// The token was collected; a second poll reports this.
    Consumed,
    Failed(String),
}

/// One pending sign-in.
struct PendingLogin {
    landscape: String,
    url: String,
    bound_port: u16,
    state: LoginState,
    stop: watch::Sender<bool>,
}

fn logins() -> &'static Mutex<HashMap<String, PendingLogin>> {
    static LOGINS: OnceLock<Mutex<HashMap<String, PendingLogin>>> = OnceLock::new();
    LOGINS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn counter() -> &'static Mutex<u64> {
    static COUNTER: OnceLock<Mutex<u64>> = OnceLock::new();
    COUNTER.get_or_init(|| Mutex::new(0))
}

/// Start listening for one landscape's sign-in hand-off.
///
/// Returns the handle, the URL to open, and the port the browser must reach.
pub async fn begin(landscape: &str, host: &str, port: u16) -> Result<(String, String, u16)> {
    let listener = TcpListener::bind((host, port)).await.map_err(|error| {
        Error::transport(format!(
            "cannot listen on {host}:{port} for the sign-in hand-off: {error}"
        ))
    })?;
    let bound_port = listener
        .local_addr()
        .map_err(|error| Error::transport(format!("cannot read the listener port: {error}")))?
        .port();
    let url = bas_core::landscape::ext_login_url(landscape, (std::process::id() % 100_000) as u32);
    let id = {
        let mut counter = counter().lock().expect("the login counter is not poisoned");
        *counter += 1;
        format!("login-{}", *counter)
    };
    let (stop, mut stop_rx) = watch::channel(false);
    let landscape_owned = landscape.to_string();
    let handle = id.clone();
    tokio::spawn(async move {
        let accept = async {
            loop {
                let accepted = tokio::select! {
                    _ = stop_rx.changed() => break,
                    accepted = listener.accept() => accepted,
                };
                let Ok((mut socket, _peer)) = accepted else {
                    break;
                };
                // The hand-off is one small POST; read it, acknowledge, move on.
                let mut buffer = vec![0u8; 64 * 1024];
                let read = tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    socket.read(&mut buffer),
                )
                .await;
                let Ok(Ok(read)) = read else {
                    continue;
                };
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                let body = request
                    .split("\r\n\r\n")
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                let payload = parse_handoff(&body);
                let (jwt, iasjwt) = match payload {
                    Some(pair) => pair,
                    None => {
                        let _ = socket
                            .write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n")
                            .await;
                        continue;
                    }
                };
                let response = b"HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: 46\r\n\r\n<html><body>You can close this window.</body></html>";
                let _ = socket.write_all(response).await;
                let mut guard = logins().lock().expect("the login registry is not poisoned");
                if let Some(entry) = guard.get_mut(&handle) {
                    entry.state = LoginState::Done { jwt, iasjwt };
                }
                break;
            }
        };
        // The listener gives up after the default window; a host that polls
        // later sees `failed` with a reason instead of waiting forever.
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(DEFAULT_LOGIN_TIMEOUT_SECS),
            accept,
        )
        .await;
        let mut guard = logins().lock().expect("the login registry is not poisoned");
        if let Some(entry) = guard.get_mut(&handle)
            && entry.state == LoginState::Waiting
        {
            entry.state = LoginState::Failed(format!(
                "no sign-in arrived within {DEFAULT_LOGIN_TIMEOUT_SECS}s"
            ));
        }
        let _ = &landscape_owned;
    });

    logins()
        .lock()
        .expect("the login registry is not poisoned")
        .insert(
            id.clone(),
            PendingLogin {
                landscape: landscape.to_string(),
                url: url.clone(),
                bound_port,
                state: LoginState::Waiting,
                stop,
            },
        );
    Ok((id, url, bound_port))
}

/// Parse the hand-off body: `{"jwt": "...", "iasjwt": "..."}`.
fn parse_handoff(body: &str) -> Option<(String, String)> {
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        let jwt = value.get("jwt")?.as_str()?.to_string();
        if jwt.is_empty() {
            return None;
        }
        let iasjwt = value
            .get("iasjwt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        return Some((jwt, iasjwt));
    }
    // A form-encoded post (`jwt=...&iasjwt=...`) is what the landscape sends
    // when the page is served as a plain form.
    let mut jwt = String::new();
    let mut iasjwt = String::new();
    for pair in body.split('&') {
        let mut parts = pair.splitn(2, '=');
        let key = parts.next().unwrap_or_default();
        let value = parts.next().unwrap_or_default();
        let decoded = percent_decode(value);
        match key {
            "jwt" => jwt = decoded,
            "iasjwt" => iasjwt = decoded,
            _ => {}
        }
    }
    if jwt.is_empty() {
        None
    } else {
        Some((jwt, iasjwt))
    }
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or_default();
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Poll one sign-in.
///
/// `waiting`, `done` (with the token), `consumed`, or `failed`. The token is
/// handed out once: a second poll reports `consumed`, so a host that retries
/// cannot silently overwrite a newer credential with an older token.
pub fn poll(id: &str) -> Result<Value> {
    let mut guard = logins().lock().expect("the login registry is not poisoned");
    let Some(entry) = guard.get_mut(id) else {
        return Err(Error::new(
            Code::NotFound,
            format!("no sign-in with id \"{id}\""),
        ));
    };
    let (state, jwt, iasjwt, error) = match &entry.state {
        LoginState::Waiting => ("waiting", None, None, None),
        LoginState::Done { jwt, iasjwt } => ("done", Some(jwt.clone()), Some(iasjwt.clone()), None),
        LoginState::Consumed => ("consumed", None, None, None),
        LoginState::Failed(reason) => ("failed", None, None, Some(reason.clone())),
    };
    if matches!(entry.state, LoginState::Done { .. }) {
        entry.state = LoginState::Consumed;
    }
    Ok(json!({
        "state": state,
        "landscape": entry.landscape,
        "url": entry.url,
        "boundPort": entry.bound_port,
        "jwt": jwt,
        "iasjwt": iasjwt,
        "error": error,
    }))
}

/// Stop listening for one sign-in and forget it.
pub fn cancel(id: &str) -> Result<bool> {
    let Some(entry) = logins()
        .lock()
        .expect("the login registry is not poisoned")
        .remove(id)
    else {
        return Ok(false);
    };
    let _ = entry.stop.send(true);
    Ok(true)
}

/// Forget every pending sign-in (called on teardown).
pub fn cancel_all() {
    let entries: Vec<(String, PendingLogin)> = {
        let mut guard = logins().lock().expect("the login registry is not poisoned");
        guard.drain().collect()
    };
    for (_, entry) in entries {
        let _ = entry.stop.send(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_json_and_form_handoffs() {
        assert_eq!(
            parse_handoff(r#"{"jwt":"a.b.c","iasjwt":"x"}"#),
            Some(("a.b.c".to_string(), "x".to_string()))
        );
        assert_eq!(
            parse_handoff("jwt=a.b.c&iasjwt=x"),
            Some(("a.b.c".to_string(), "x".to_string()))
        );
        assert_eq!(
            parse_handoff("jwt=a%2Eb&iasjwt="),
            Some(("a.b".to_string(), String::new()))
        );
        assert_eq!(parse_handoff(""), None);
        assert_eq!(parse_handoff("iasjwt=only"), None);
    }
}
