//! BAS (SAP Business Application Studio) protocol layer.
//!
//! Everything that talks to a BAS landscape or to a dev space lives here, and
//! nothing that talks to a human does: this crate has no terminal, no
//! environment, no filesystem and no process spawning. Callers own the token,
//! the paths, the ports and the policy; this crate owns the wire formats.
//!
//! The layering is deliberate and enforced by `tests/purity.rs`:
//!
//! | crate | role | may touch |
//! |---|---|---|
//! | `bas-core` | protocol + transport | network only |
//! | `bas-api` | process-local effects (files, tokio registry) behind a C ABI | fs, threads |
//! | `bas-cli` | argv, prompts, exit codes | everything |
//!
//! A host that is not this repository's DSH plugin (a Python script, a Go
//! daemon, an editor extension) only needs `bas-api`'s C ABI and nothing from
//! this module tree — which is the point of the split.

pub mod bridge;
pub mod devchannel;
pub mod error;
pub mod exec;
pub mod forward;
pub mod jwt;
pub mod landscape;
pub mod model;
pub mod sshclient;
pub mod sshconfig;
pub mod wss;

pub use error::{Error, Result};
pub use model::{
    BridgeInfo, BridgeState, DevSpace, ExecOutput, ForwardInfo, KeyMaterial, StatInfo, TunnelStatus,
};

/// Status a dev space reports while its runtime is usable.
pub const DEV_SPACE_RUNNING: &str = "RUNNING";

/// Status a dev space reports while it is down.
pub const DEV_SPACE_STOPPED: &str = "STOPPED";

/// The landscape `ws-manager` API prefix, relative to the landscape origin.
pub const WS_MANAGER_PATH: &str = "/ws-manager/api/v1";

/// Port the landscape login page posts its token back to.
pub const DEFAULT_LOGIN_PORT: u16 = 55532;

/// The dev space's sshd, as seen from inside the dev space.
pub const DEV_SPACE_SSHD_HOST: &str = "127.0.0.1";

/// The dev space's sshd port.
pub const DEV_SPACE_SSHD_PORT: u16 = 2222;

/// Dev-channel SSH-over-WebSocket socket port (`port33765-<host>`).
pub const SSHD_SOCKET_PORT: u16 = 33765;

/// Dev-channel SSH-over-WebSocket TCP port.
pub const SSH_SOCKET_PORT: u16 = 443;
