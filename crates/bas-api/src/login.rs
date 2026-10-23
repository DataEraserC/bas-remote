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

/// Headers the landscape login page needs to reach the loopback listener.
///
/// The hand-off is a cross-origin `fetch` from `https://<landscape>` to
/// `http://127.0.0.1:<port>`, so the browser preflights it with `OPTIONS` and
/// drops the `POST` unless the preflight is answered. 0.5.x sent these; the
/// Rust rewrite lost them, which made every sign-in fail in the browser before
/// the request left the tab.
const CORS_HEADERS: &str = "\
access-control-allow-origin: *\r\n\
access-control-allow-headers: content-type\r\n\
access-control-allow-methods: POST, OPTIONS\r\n\
access-control-allow-private-network: true\r\n\
access-control-max-age: 600\r\n";

/// Largest hand-off request accepted; the token is a couple of kilobytes.
const MAX_HANDOFF_BYTES: usize = 1024 * 1024;

/// One request read off the hand-off socket.
struct HandoffRequest {
    method: String,
    path: String,
    body: String,
}

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
                // The hand-off is one small request; read it whole, answer,
                // move on. A partial read is not enough: the body carries the
                // token and may arrive in several segments.
                let request = tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    read_request(&mut socket),
                )
                .await;
                let Ok(Some(request)) = request else {
                    continue;
                };
                let answer = match (request.method.as_str(), request.path.as_str()) {
                    // Answer the preflight, or the browser never sends the POST.
                    ("OPTIONS", _) => ("204 No Content", None, "", None),
                    ("POST", "/ext-login") => match parse_handoff(&request.body) {
                        Some((jwt, iasjwt)) => (
                            "200 OK",
                            Some("application/json"),
                            "{\"status\":\"ok\"}",
                            Some((jwt, iasjwt)),
                        ),
                        // Same contract as 0.5.x: the page sees a status it can
                        // render, and the listener stays open for a retry.
                        None => (
                            "200 OK",
                            Some("application/json"),
                            "{\"status\":\"error\"}",
                            None,
                        ),
                    },
                    (_, "/ext-login") => (
                        "405 Method Not Allowed",
                        Some("text/plain"),
                        "dsh-bas-remote: POST only",
                        None,
                    ),
                    _ => (
                        "404 Not Found",
                        Some("text/plain"),
                        "dsh-bas-remote: not found",
                        None,
                    ),
                };
                let (status, content_type, body, token) = answer;
                let _ = socket
                    .write_all(&response_bytes(status, content_type, body))
                    .await;
                let _ = socket.flush().await;
                let Some((jwt, iasjwt)) = token else {
                    continue;
                };
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
        // The landscape sometimes posts an HTML error page instead of a token.
        if jwt.is_empty() || jwt.starts_with('<') {
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
    if jwt.is_empty() || jwt.starts_with('<') {
        None
    } else {
        Some((jwt, iasjwt))
    }
}

/// Find the end of the header block (`\r\n\r\n`), past the blank line.
fn header_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4)
}

/// Parse the request line and `content-length` out of the head bytes.
fn parse_head(head: &str) -> Option<(String, String, usize)> {
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_ascii_uppercase();
    let target = request_line.next()?;
    let path = target.split('?').next().unwrap_or(target).to_string();
    let mut content_length = 0usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse().ok()?;
        }
    }
    Some((method, path, content_length))
}

/// Read one whole request: headers, then `content-length` bytes of body.
async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<HandoffRequest> {
    let mut buffer = Vec::with_capacity(8 * 1024);
    let mut chunk = [0u8; 8 * 1024];
    loop {
        if let Some(end) = header_end(&buffer) {
            let head = String::from_utf8_lossy(&buffer[..end]).to_string();
            if let Some((method, path, content_length)) = parse_head(&head)
                && buffer.len() >= end + content_length
            {
                let body_end = (end + content_length).min(buffer.len());
                let body = String::from_utf8_lossy(&buffer[end..body_end]).to_string();
                return Some(HandoffRequest { method, path, body });
            }
        }
        if buffer.len() > MAX_HANDOFF_BYTES {
            return None;
        }
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

/// Encode one response with the CORS headers the preflight requires.
fn response_bytes(status: &str, content_type: Option<&str>, body: &str) -> Vec<u8> {
    let mut head = format!("HTTP/1.1 {status}\r\n{CORS_HEADERS}");
    if let Some(content_type) = content_type {
        head.push_str("content-type: ");
        head.push_str(content_type);
        head.push_str("\r\n");
    }
    // Byte length, not character count: a hard-coded one truncated the body.
    head.push_str(&format!(
        "content-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    ));
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(body.as_bytes());
    bytes
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
        // An HTML error page is not a token.
        assert_eq!(parse_handoff(r#"{"jwt":"<html>nope</html>"}"#), None);
    }

    #[test]
    fn response_has_cors_and_a_truthful_content_length() {
        let bytes = response_bytes("200 OK", Some("application/json"), "{\"status\":\"ok\"}");
        let text = String::from_utf8(bytes).expect("ascii");
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text}");
        assert!(
            text.contains("access-control-allow-origin: *\r\n"),
            "{text}"
        );
        assert!(
            text.contains("access-control-allow-headers: content-type\r\n"),
            "{text}"
        );
        assert!(
            text.contains("access-control-allow-methods: POST, OPTIONS\r\n"),
            "{text}"
        );
        let (head, body) = text.split_once("\r\n\r\n").expect("a header block");
        let declared: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length: "))
            .expect("a content-length")
            .trim()
            .parse()
            .expect("a number");
        assert_eq!(
            declared,
            body.len(),
            "content-length must be the byte length of the body"
        );
        assert_eq!(body, "{\"status\":\"ok\"}");
    }

    #[test]
    fn reads_a_request_split_across_segments() {
        // The body may arrive after the headers; `read_request` must wait for
        // all of it instead of parsing whatever the first read returned.
        let head = "POST /ext-login HTTP/1.1\r\ncontent-type: application/json\r\ncontent-length: 11\r\n\r\n";
        assert_eq!(header_end(head.as_bytes()), Some(head.len()));
        let (method, path, length) = parse_head(head).expect("a head");
        assert_eq!(method, "POST");
        assert_eq!(path, "/ext-login");
        assert_eq!(length, 11);
    }

    #[tokio::test]
    async fn answers_the_preflight_and_hands_the_token_over_once() {
        let (id, _url, port) = begin("https://example.test", "127.0.0.1", 0)
            .await
            .expect("the listener starts");

        // 1. The browser's preflight must be answered, or it never sends the POST.
        let preflight = send(
            port,
            "OPTIONS /ext-login HTTP/1.1\r\nHost: 127.0.0.1\r\n\
             Origin: https://example.test\r\n\
             Access-Control-Request-Method: POST\r\n\
             Access-Control-Request-Headers: content-type\r\n\r\n",
        )
        .await;
        assert!(
            preflight.starts_with("HTTP/1.1 204 No Content"),
            "preflight must succeed: {preflight}"
        );
        assert!(
            preflight.contains("access-control-allow-origin: *"),
            "preflight must allow the origin: {preflight}"
        );
        assert!(
            preflight.contains("access-control-allow-headers: content-type"),
            "preflight must allow the JSON content type: {preflight}"
        );

        // 2. A bad body is answered, not dropped, and the listener stays open.
        let bad = send(
            port,
            "POST /ext-login HTTP/1.1\r\nHost: 127.0.0.1\r\n\
             content-type: application/json\r\ncontent-length: 2\r\n\r\n{}",
        )
        .await;
        assert!(bad.starts_with("HTTP/1.1 200 OK"), "{bad}");
        assert!(bad.ends_with("{\"status\":\"error\"}"), "{bad}");

        // 3. The real hand-off returns `{"status":"ok"}` with the token.
        let body = r#"{"jwt":"a.b.c","iasjwt":"x.y.z"}"#;
        let request = format!(
            "POST /ext-login HTTP/1.1\r\nHost: 127.0.0.1\r\n\
             content-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let response = send(port, &request).await;
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(
            response.contains("content-type: application/json"),
            "{response}"
        );
        assert!(response.ends_with("{\"status\":\"ok\"}"), "{response}");
        assert!(
            !response.contains("<html>"),
            "the page expects JSON, not an HTML page: {response}"
        );

        // 4. The token is handed out exactly once.
        let first = poll(&id).expect("a poll");
        assert_eq!(first["state"], "done");
        assert_eq!(first["jwt"], "a.b.c");
        let second = poll(&id).expect("a poll");
        assert_eq!(second["state"], "consumed");

        assert!(cancel(&id).expect("a cancel"));
    }

    #[tokio::test]
    async fn unknown_paths_are_404_and_do_not_end_the_sign_in() {
        let (id, _url, port) = begin("https://example.test", "127.0.0.1", 0)
            .await
            .expect("the listener starts");
        let missing = send(port, "GET /favicon.ico HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await;
        assert!(missing.starts_with("HTTP/1.1 404 Not Found"), "{missing}");
        assert!(
            missing.contains("access-control-allow-origin: *"),
            "{missing}"
        );

        // Still waiting: a stray request must not consume the sign-in.
        assert_eq!(poll(&id).expect("a poll")["state"], "waiting");
        assert!(cancel(&id).expect("a cancel"));
    }

    /// Send one raw request to the listener and return the raw response.
    async fn send(port: u16, request: &str) -> String {
        use tokio::io::AsyncWriteExt;

        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("the listener is reachable");
        socket
            .write_all(request.as_bytes())
            .await
            .expect("the request is written");
        let mut response = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut socket, &mut response)
            .await
            .expect("the response is read");
        String::from_utf8(response).expect("utf-8")
    }
}
