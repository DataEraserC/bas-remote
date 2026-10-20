//! Reading one exec channel to completion.
//!
//! Shared by the dev channel and the plain SSH client so both behave the same.
//!
//! Two things make "the command finished" subtler than "the channel closed":
//!
//! * **`eof` is not the end.** OpenSSH may send `eof` before `exit-status`, and
//!   a client that stops at the first `eof` reports exit code `-1` for a command
//!   that succeeded. The exit status is what makes a failure visible, so the
//!   reader keeps waiting for it after `eof` — briefly.
//! * **a background child inherits the channel.** A command that starts a
//!   daemon (`setsid`, `nohup`, the bridge's own start) leaves the channel open
//!   after the exit status, so waiting for `close` hangs forever. Once the exit
//!   status is in, only a short grace window is spent collecting trailing
//!   output, then the channel is abandoned.
//!
//! The channel is closed by this function when it is done, so a caller never
//! leaves an exec channel dangling.

use std::time::Duration;

use russh::ChannelMsg;

use crate::error::{Error, Result};
use crate::model::ExecOutput;

/// How long to wait for an exit status after `eof`, and for trailing output
/// after an exit status.
const GRACE: Duration = Duration::from_millis(750);

/// Drain one exec channel until the command finished.
pub async fn drain<S>(channel: &mut russh::Channel<S>, timeout: Duration) -> Result<ExecOutput>
where
    S: From<(russh::ChannelId, ChannelMsg)> + Send + Sync + 'static,
{
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut code: Option<i32> = None;
    let mut eof = false;

    let collect = async {
        loop {
            let next = if code.is_some() {
                // The command finished. Only trailing output can still arrive,
                // and a background child may hold the channel open forever.
                match tokio::time::timeout(GRACE, channel.wait()).await {
                    Ok(message) => message,
                    Err(_) => break,
                }
            } else if eof {
                // `eof` arrived but the exit status has not: the status is the
                // one message worth waiting for, because without it a failed
                // command is indistinguishable from a successful one.
                match tokio::time::timeout(GRACE, channel.wait()).await {
                    Ok(message) => message,
                    Err(_) => break,
                }
            } else {
                channel.wait().await
            };
            match next {
                None => break,
                Some(ChannelMsg::Data { data }) => stdout.extend_from_slice(&data),
                Some(ChannelMsg::ExtendedData { data, ext }) => {
                    if ext == 1 {
                        stderr.extend_from_slice(&data);
                    }
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => code = Some(exit_status as i32),
                Some(ChannelMsg::Eof) => eof = true,
                Some(ChannelMsg::Close) => break,
                Some(_) => {}
            }
        }
    };

    tokio::time::timeout(timeout, collect).await.map_err(|_| {
        Error::transport(format!(
            "the remote command did not finish within {timeout:?}"
        ))
    })?;

    // Closing is best effort: a channel a background child still holds may
    // refuse, and that must not turn a completed command into a failure.
    let _ = channel.close().await;

    Ok(ExecOutput {
        code: code.unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The grace window is what keeps a daemon-spawning command from hanging;
    /// it must stay short enough that a caller does not notice it, and long
    /// enough that an in-flight exit status still lands.
    #[test]
    fn grace_window_is_short() {
        assert!(GRACE <= Duration::from_secs(2));
        assert!(GRACE >= Duration::from_millis(200));
    }
}
