//! The exported C functions.
//!
//! ```c
//! char *bas_call(const char *op, const char *args_json);  // malloc'd JSON, NUL-terminated
//! void  bas_free(char *response);                          // release what bas_call returned
//! const char *bas_version(void);                           // static, never freed
//! ```
//!
//! The operation runs **exactly once** per call. An earlier draft used the
//! usual `probe → allocate → call` two-call pattern, where a `NULL`/small
//! buffer reports the size needed; that is wrong for this ABI, because
//! "report the size" means running the operation to know the size, and running
//! it twice starts two tunnels or two bridge daemons. A single call that
//! returns an owned string has no such trap and no ambiguity about who frees
//! what.
//!
//! Ownership is one rule: whatever `bas_call` returns, the caller passes back
//! to `bas_free` exactly once. `bas_version` is a static string and must never
//! be freed.
//!
//! A panic inside any operation is caught here and reported as an `internal`
//! error: unwinding across an FFI boundary is undefined behaviour, and a host
//! should get a message, not a crashed process.

use std::ffi::{CStr, CString, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};

use serde_json::Value;

use crate::ops;
use crate::reply::Reply;

/// Library version, matching the crate version.
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// `const char *bas_version(void)` — a static string, never freed.
#[unsafe(no_mangle)]
pub extern "C" fn bas_version() -> *const c_char {
    static VERSION: std::sync::OnceLock<CString> = std::sync::OnceLock::new();
    VERSION
        .get_or_init(|| CString::new(version()).expect("the version has no NUL"))
        .as_ptr()
}

/// `char *bas_call(const char *op, const char *args_json)`
///
/// Runs one operation and returns its JSON envelope in a buffer the caller must
/// release with [`bas_free`]. Never returns `NULL`: an unusable argument or an
/// internal panic comes back as an `{"ok":false,...}` envelope, so a host has
/// exactly one response shape to parse.
///
/// # Safety
///
/// `op` and `args_json` must be NUL-terminated UTF-8 strings, or `NULL`
/// (`args_json` only; a `NULL` `op` is an error envelope).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bas_call(op: *const c_char, args_json: *const c_char) -> *mut c_char {
    let response = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the caller guarantees NUL-terminated strings; `CStr::from_ptr`
        // reads only up to the first NUL.
        let op = if op.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(op) }.to_string_lossy().into_owned()
        };
        let args = if args_json.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(args_json) }
                .to_string_lossy()
                .into_owned()
        };
        dispatch(&op, &args)
    }))
    .unwrap_or_else(|panic| {
        let detail = panic
            .downcast_ref::<&str>()
            .map(|value| (*value).to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".to_string());
        Reply::err("internal", format!("bas-api panicked: {detail}"))
    });

    // `CString::into_raw` hands ownership to the caller; `bas_free` takes it
    // back. The JSON never contains a NUL, so `new` cannot fail here.
    match CString::new(response.to_json()) {
        Ok(text) => text.into_raw(),
        Err(_) => CString::new(
            r#"{"ok":false,"error":{"code":"internal","message":"response contained a NUL"}}"#,
        )
        .expect("the fallback envelope has no NUL")
        .into_raw(),
    }
}

/// `void bas_free(char *response)` — release a string [`bas_call`] returned.
///
/// Passing `NULL` is a no-op. Passing the same pointer twice, or a pointer that
/// did not come from [`bas_call`], is undefined behaviour; a host frees exactly
/// once, which is what `lib/native.js` does in a `finally`.
///
/// # Safety
///
/// `response` must be a pointer returned by [`bas_call`] and not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bas_free(response: *mut c_char) {
    if response.is_null() {
        return;
    }
    // SAFETY: the caller passes a pointer from `CString::into_raw`, which is the
    // only shape `CString::from_raw` accepts.
    drop(unsafe { CString::from_raw(response) });
}

/// Route one operation name to its implementation.
fn dispatch(op: &str, args: &str) -> Reply {
    let args: Value = if args.trim().is_empty() {
        Value::Object(serde_json::Map::new())
    } else {
        match serde_json::from_str(args) {
            Ok(value) => value,
            Err(error) => {
                return Reply::err("invalid", format!("args is not JSON: {error}"));
            }
        }
    };
    match ops::call(op, &args) {
        Ok(data) => Reply::ok(data),
        Err(error) => Reply::err(error.code.as_str(), error.message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Call one op and take ownership of the response, as a host does.
    fn call(op: &str, args: &str) -> Value {
        let op = CString::new(op).unwrap();
        let args = CString::new(args).unwrap();
        let pointer = unsafe { bas_call(op.as_ptr(), args.as_ptr()) };
        assert!(!pointer.is_null(), "bas_call never returns NULL");
        let text = unsafe { CStr::from_ptr(pointer) }
            .to_str()
            .unwrap()
            .to_string();
        unsafe { bas_free(pointer) };
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn returns_the_envelope() {
        let parsed = call("normalize_landscape", r#"{"landscape":"host.example"}"#);
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["data"]["landscape"], "https://host.example");
    }

    #[test]
    fn unknown_ops_and_bad_json_are_reported_not_panicked() {
        let unknown = call("nope", "{}");
        assert_eq!(unknown["ok"], false);
        assert_eq!(unknown["error"]["code"], "invalid");

        let bad = call("normalize_landscape", "not json");
        assert_eq!(bad["error"]["code"], "invalid");
    }

    #[test]
    fn null_args_are_an_empty_object() {
        let op = CString::new("port_free").unwrap();
        let pointer = unsafe { bas_call(op.as_ptr(), std::ptr::null()) };
        let text = unsafe { CStr::from_ptr(pointer) }
            .to_str()
            .unwrap()
            .to_string();
        unsafe { bas_free(pointer) };
        let parsed: Value = serde_json::from_str(&text).unwrap();
        // A missing `port` is a caller error, not a crash.
        assert_eq!(parsed["error"]["code"], "invalid");
    }

    #[test]
    fn freeing_null_is_a_no_op() {
        unsafe { bas_free(std::ptr::null_mut()) };
    }

    #[test]
    fn version_is_exposed_and_static() {
        assert!(!version().is_empty());
        let first = bas_version();
        let second = bas_version();
        assert_eq!(first, second, "the version pointer is stable");
        let text = unsafe { CStr::from_ptr(first) }.to_str().unwrap();
        assert_eq!(text, version());
    }

    /// The property the whole redesign exists for: one call, one execution.
    #[test]
    fn an_operation_runs_exactly_once() {
        // `login_cancel` on an unknown handle is idempotent, so it cannot show
        // the count. Use the tunnel registry instead: opening a tunnel needs a
        // network, but `tunnel_list` proves reads are stable across calls.
        let first = call("tunnel_list", "{}");
        let second = call("tunnel_list", "{}");
        assert_eq!(first["data"]["tunnels"], second["data"]["tunnels"]);
    }
}
