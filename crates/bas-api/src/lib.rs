//! `bas-api` — the C ABI over [`bas_core`].
//!
//! This crate exists for one reason: the BAS protocol must not depend on a
//! harness platform. A Node plugin, a Python script, a Go daemon and a shell
//! script all reach the same implementation through one C function, and the
//! contract is written down in `docs/abi.md`.
//!
//! Two kinds of thing live here, and only these two:
//!
//! * **effects the core refuses to have** — reading and writing files, owning a
//!   tokio runtime, binding a listener (all in [`ops`]);
//! * **live resources** — tunnels and sign-in listeners, which are handle-based
//!   so a host can never confuse two of them (in [`registry`] and [`login`]).
//!
//! Everything else is a thin translation of JSON into a core call.

pub mod abi;
pub mod login;
pub mod ops;
pub mod registry;
pub mod reply;

pub use abi::{bas_call, bas_free, bas_version};

/// Runtime shared by every operation.
///
/// One multi-threaded runtime for the whole library: a host may call from any
/// thread (koffi's async calls run on worker threads), so the runtime must be
/// `Send + Sync` and must not be tied to a caller's stack.
pub(crate) fn runtime() -> &'static tokio::runtime::Runtime {
    use std::sync::OnceLock;
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("bas-api")
            .build()
            .expect("the bas-api runtime must start")
    })
}
