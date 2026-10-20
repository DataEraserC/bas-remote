//! Live tunnels, keyed by an opaque handle.
//!
//! A tunnel is the one thing here that is genuinely stateful: it owns a
//! WebSocket, an SSH session and a listener. Handles make that state explicit
//! and unforgeable — a host holds an id, not a pointer, and an id from another
//! process means nothing.
//!
//! The registry is process-global because a C ABI has no `self` to hang state
//! on. That is safe: every access goes through one lock, and a tunnel lives
//! independently of whichever thread created it.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use bas_core::Error;
use bas_core::devchannel::DevChannel;
use bas_core::forward::Forward;
use bas_core::model::{ForwardInfo, TunnelStatus};
use serde_json::json;

/// One live tunnel and everything it owns.
pub struct Tunnel {
    pub id: String,
    pub server_uri: String,
    pub local_port: u16,
    pub username: String,
    pub started_at: i64,
    pub channel: std::sync::Arc<DevChannel>,
    pub forwards: Vec<Forward>,
    /// Set when the tunnel dropped on its own, so a status call can say why.
    pub closed_reason: String,
}

impl Tunnel {
    /// The JSON description a host reads.
    #[must_use]
    pub fn status(&self) -> TunnelStatus {
        TunnelStatus {
            id: self.id.clone(),
            server_uri: self.server_uri.clone(),
            local_port: self.local_port,
            username: self.username.clone(),
            ssh_host: "127.0.0.1".to_string(),
            ssh_port: bas_core::DEV_SPACE_SSHD_PORT,
            active: self.channel.active(),
            started_at: self.started_at,
            closed_reason: self.closed_reason.clone(),
            forwards: self
                .forwards
                .iter()
                .map(|forward| ForwardInfo {
                    remote_port: forward.remote_port,
                    local_port: forward.local_port,
                })
                .collect(),
        }
    }
}

fn tunnels() -> &'static Mutex<HashMap<String, Tunnel>> {
    static TUNNELS: OnceLock<Mutex<HashMap<String, Tunnel>>> = OnceLock::new();
    TUNNELS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn counter() -> &'static Mutex<u64> {
    static COUNTER: OnceLock<Mutex<u64>> = OnceLock::new();
    COUNTER.get_or_init(|| Mutex::new(0))
}

/// Unix milliseconds, the timestamp unit every payload uses.
#[must_use]
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Allocate a fresh handle (`tunnel-1`, `tunnel-2`, …).
///
/// A monotonic counter rather than a random id: the sequence makes logs
/// readable, and a handle is not a security token — it is scoped to this
/// process, and a host that guesses one could only reach its own tunnels.
fn next_id(prefix: &str) -> String {
    let mut counter = counter()
        .lock()
        .expect("the handle counter is not poisoned");
    *counter += 1;
    format!("{prefix}-{counter}")
}

/// Register a live tunnel and return its handle.
pub fn insert(tunnel: Tunnel) -> String {
    let id = tunnel.id.clone();
    tunnels()
        .lock()
        .expect("the tunnel registry is not poisoned")
        .insert(id.clone(), tunnel);
    id
}

/// Run `f` with the tunnel behind `id`.
pub fn with<R>(id: &str, f: impl FnOnce(&mut Tunnel) -> R) -> Result<R, Error> {
    let mut guard = tunnels()
        .lock()
        .expect("the tunnel registry is not poisoned");
    match guard.get_mut(id) {
        Some(tunnel) => Ok(f(tunnel)),
        None => Err(Error::new(
            bas_core::error::Code::NotFound,
            format!("no tunnel with id \"{id}\""),
        )),
    }
}

/// Remove a tunnel, returning it so the caller can tear it down outside the lock.
pub fn take(id: &str) -> Option<Tunnel> {
    tunnels()
        .lock()
        .expect("the tunnel registry is not poisoned")
        .remove(id)
}

/// Every live tunnel, in handle order.
#[must_use]
pub fn list() -> Vec<TunnelStatus> {
    let guard = tunnels()
        .lock()
        .expect("the tunnel registry is not poisoned");
    let mut entries: Vec<TunnelStatus> = guard.values().map(Tunnel::status).collect();
    entries.sort_by(|left, right| left.id.cmp(&right.id));
    entries
}

/// Build the handle of a new tunnel.
#[must_use]
pub fn new_handle() -> String {
    next_id("tunnel")
}

/// Describe one tunnel as JSON (the shape `tunnel_status` returns).
#[must_use]
pub fn status_json(status: &TunnelStatus) -> serde_json::Value {
    json!(status)
}
