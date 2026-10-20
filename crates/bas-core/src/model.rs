//! Wire shapes shared by the protocol layer, the C ABI and the CLI.
//!
//! Every type serialises to camelCase so the JSON that crosses the ABI is the
//! same JSON a JavaScript host reads directly — no translation table anywhere.

use serde::{Deserialize, Serialize};

/// One dev space, flattened the way `@sap/bas-sdk`'s `DevspaceInfo` is.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DevSpace {
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub status: String,
    /// Runtime base URL — the dev-channel WebSocket host.
    #[serde(default)]
    pub url: String,
    /// Runtime startup URL, which serves `/key`.
    #[serde(default)]
    pub startup_url: String,
    /// `None` until the key endpoint was probed.
    #[serde(default)]
    pub ssh_enabled: Option<bool>,
    #[serde(default)]
    pub pack: String,
    #[serde(default)]
    pub pack_display_name: String,
    #[serde(default)]
    pub origin: String,
}

/// What the landscape hands out when a dev space is asked for its SSH key.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyMaterial {
    /// PEM private key, inline: the protocol layer never touches the filesystem.
    pub key: String,
    /// Dev-space runtime base URL, from which the dev-channel host is derived.
    pub ws_url: String,
    #[serde(default)]
    pub startup_url: String,
}

/// Result of one exec channel.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecOutput {
    pub code: i32,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
}

/// A live tunnel as the registry reports it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TunnelStatus {
    pub id: String,
    pub server_uri: String,
    pub local_port: u16,
    pub username: String,
    pub ssh_host: String,
    pub ssh_port: u16,
    pub active: bool,
    /// Unix milliseconds at which the tunnel became ready.
    pub started_at: i64,
    #[serde(default)]
    pub closed_reason: String,
    #[serde(default)]
    pub forwards: Vec<ForwardInfo>,
}

/// One loopback port carrying a dev-space port.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForwardInfo {
    pub remote_port: u16,
    pub local_port: u16,
}

/// Outcome of bringing the SFTP bridge up inside a dev space.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeInfo {
    pub dir: String,
    pub remote_port: u16,
    pub pid: u32,
    pub installed: bool,
    pub version: String,
}

/// State of the SFTP bridge of one dev space.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeState {
    pub dir: String,
    pub installed: bool,
    pub running: bool,
    pub pid: u32,
    pub listening: bool,
    #[serde(default)]
    pub log_tail: String,
}

/// Decoded view of a landscape JWT.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JwtInfo {
    pub valid: bool,
    /// Milliseconds until `exp`, or `None` when the token carries no expiry.
    pub remaining_ms: Option<i64>,
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub subject: Option<String>,
}

/// Where a dev space is reachable, as the host should publish it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshEndpoint {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub key_file: String,
}

/// Config of one dev-channel tunnel.
#[derive(Debug, Clone)]
pub struct TunnelConfig {
    pub landscape: String,
    pub jwt: String,
    /// Dev-space runtime base URL (`DevSpace::url`).
    pub ws_url: String,
    /// Loopback port; `0` asks the kernel for a free one.
    pub local_port: u16,
    pub username: String,
    pub connect_attempts: u8,
    pub connect_timeout_ms: u64,
}

/// Result of a `stat` over SFTP.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatInfo {
    pub exists: bool,
    pub is_dir: bool,
    pub size: u64,
    pub mode: u32,
}
