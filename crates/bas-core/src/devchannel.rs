//! The dev channel: SSH over a WebSocket.
//!
//! A BAS dev space is not reachable over plain TCP. Its sshd is published on a
//! WebSocket endpoint that carries a standard SSH transport:
//!
//! ```text
//! wss://port33765-<dev-space-host>:443   (sub-protocol "ssh", bearer <JWT>)
//! ```
//!
//! So the client is an ordinary SSH client whose byte stream happens to be a
//! WebSocket. `russh` takes any `AsyncRead + AsyncWrite`, which is why this
//! module is short: wrap the socket, connect, authenticate with the `none`
//! method (the WebSocket bearer already authorised the caller), then open
//! `direct-tcpip` channels into the dev space's loopback.

use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, Handle};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::error::{Code, Error, Result};
use crate::wss::WsByteStream;
use crate::{SSH_SOCKET_PORT, SSHD_SOCKET_PORT};

/// The dev-channel URL of a dev-space runtime URL.
///
/// `https://port33765-abc.example` → `wss://port33765-abc.example:443`.
pub fn server_uri(ws_url: &str) -> Result<String> {
    let url = url::Url::parse(ws_url).map_err(|error| {
        Error::invalid(format!("cannot parse dev-space URL \"{ws_url}\": {error}"))
    })?;
    let host = url.host_str().ok_or_else(|| {
        Error::invalid(format!(
            "cannot derive a dev-channel host from \"{ws_url}\""
        ))
    })?;
    Ok(format!(
        "wss://port{SSHD_SOCKET_PORT}-{host}:{SSH_SOCKET_PORT}"
    ))
}

/// SSH client handler.
///
/// The dev channel presents a host key the caller cannot verify (there is no
/// known-hosts entry for a per-session dev space), and the transport is already
/// authenticated end-to-end by the WebSocket's TLS plus the bearer token, so
/// the key is accepted. This is the same trust decision the SAP extension
/// makes.
struct DevChannelClient;

impl client::Handler for DevChannelClient {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> std::result::Result<bool, Self::Error> {
        Ok(true)
    }
}

/// One live dev-channel session.
///
/// Owns the SSH handle and the task pumping the WebSocket; dropping it (or
/// calling [`DevChannel::close`]) tears both down.
pub struct DevChannel {
    handle: Handle<DevChannelClient>,
}

impl DevChannel {
    /// Open a dev channel and authenticate it.
    ///
    /// Retries the handshake `attempts` times: the dev channel intermittently
    /// answers "Failed to read the protocol version" on a fresh or busy dev
    /// space, and a second attempt a moment later succeeds.
    pub async fn connect(
        jwt: &str,
        ws_url: &str,
        username: &str,
        attempts: u8,
        timeout: Duration,
    ) -> Result<Self> {
        let uri = server_uri(ws_url)?;
        Self::connect_with_uri(jwt, &uri, username, attempts, timeout).await
    }

    /// Connect to an explicit `ws://` or `wss://` endpoint.
    ///
    /// The production path derives the URI from the dev-space URL; this entry
    /// point exists so the transport can be exercised against a local server
    /// (see `tests/devchannel_loopback.rs`), which is the only way to test an
    /// SSH-over-WebSocket path without a BAS landscape.
    pub async fn connect_with_uri(
        jwt: &str,
        uri: &str,
        username: &str,
        attempts: u8,
        timeout: Duration,
    ) -> Result<Self> {
        let attempts = attempts.max(1);
        let mut last = None;
        for attempt in 1..=attempts {
            match Self::connect_once(jwt, uri, username, timeout).await {
                Ok(channel) => return Ok(channel),
                Err(error) => {
                    last = Some(error);
                    if attempt < attempts {
                        tokio::time::sleep(Duration::from_millis(1500 * u64::from(attempt))).await;
                    }
                }
            }
        }
        Err(last.unwrap_or_else(|| Error::transport("the dev channel could not be opened")))
    }

    async fn connect_once(jwt: &str, uri: &str, username: &str, timeout: Duration) -> Result<Self> {
        let mut request = uri.into_client_request().map_err(|error| {
            Error::invalid(format!("cannot build the dev-channel request: {error}"))
        })?;
        request.headers_mut().insert(
            "authorization",
            format!("bearer {jwt}")
                .parse()
                .map_err(|error| Error::invalid(format!("unusable token header: {error}")))?,
        );
        request.headers_mut().insert(
            "sec-websocket-protocol",
            "ssh"
                .parse()
                .map_err(|error| Error::invalid(format!("unusable sub-protocol: {error}")))?,
        );

        let connect = tokio_tungstenite::connect_async(request);
        let (socket, _response) = tokio::time::timeout(timeout, connect)
            .await
            .map_err(|_| {
                Error::transport(format!(
                    "the dev channel at {uri} did not answer within {timeout:?}"
                ))
            })?
            .map_err(|error| Error::transport(format!("dev-channel handshake failed: {error}")))?;

        let stream = WsByteStream::new(socket);
        let config = Arc::new(client::Config {
            inactivity_timeout: None,
            ..client::Config::default()
        });

        let mut handle = tokio::time::timeout(
            timeout,
            client::connect_stream(config, stream, DevChannelClient),
        )
        .await
        .map_err(|_| Error::transport("the SSH handshake over the dev channel timed out"))?
        .map_err(|error| {
            Error::transport(format!(
                "SSH handshake over the dev channel failed: {error}"
            ))
        })?;

        // The WebSocket bearer token is the authentication: the SSH layer is
        // asked to accept the `none` method, exactly as the SAP extension does.
        let authenticated = tokio::time::timeout(timeout, handle.authenticate_none(username))
            .await
            .map_err(|_| Error::transport("the dev channel did not answer the SSH authentication"))?
            .map_err(|error| {
                Error::transport(format!(
                    "the dev channel rejected the SSH authentication: {error}"
                ))
            })?;
        if !authenticated.success() {
            return Err(Error::new(
                Code::Upstream,
                "the dev channel rejected the SSH client authentication",
            ));
        }

        // `russh` owns the socket from here: the session task it spawns reads
        // and writes it, so there is nothing left for this module to pump.
        Ok(Self { handle })
    }

    /// Whether the session still carries traffic.
    #[must_use]
    pub fn active(&self) -> bool {
        !self.handle.is_closed()
    }

    /// Open a `direct-tcpip` channel into the dev space's loopback.
    pub async fn open_forward(&self, remote_port: u16) -> Result<russh::Channel<client::Msg>> {
        self.handle
            .channel_open_direct_tcpip("127.0.0.1", u32::from(remote_port), "127.0.0.1", 0)
            .await
            .map_err(|error| {
                Error::transport(format!(
                    "cannot open a channel to dev-space port {remote_port}: {error}"
                ))
            })
    }

    /// Run one command over a fresh exec channel.
    pub async fn exec(&self, command: &str, timeout: Duration) -> Result<crate::model::ExecOutput> {
        let mut channel =
            self.handle.channel_open_session().await.map_err(|error| {
                Error::transport(format!("cannot open an exec channel: {error}"))
            })?;
        channel.exec(true, command).await.map_err(|error| {
            Error::transport(format!("cannot start the remote command: {error}"))
        })?;
        crate::exec::drain(&mut channel, timeout).await
    }

    /// Open an SFTP client over a new subsystem channel.
    pub async fn sftp(&self) -> Result<russh_sftp::client::SftpSession> {
        let channel =
            self.handle.channel_open_session().await.map_err(|error| {
                Error::transport(format!("cannot open an SFTP channel: {error}"))
            })?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|error| {
                Error::transport(format!("the SFTP subsystem was refused: {error}"))
            })?;
        russh_sftp::client::SftpSession::new(channel.into_stream())
            .await
            .map_err(|error| Error::transport(format!("cannot start the SFTP client: {error}")))
    }

    /// Close the session.
    ///
    /// Takes `&self` rather than `self`: forwarders hold the channel in an
    /// `Arc`, and a caller that must shut down while one is still alive should
    /// not have to hunt down every reference first.
    pub async fn close(&self) {
        let _ = self
            .handle
            .disconnect(russh::Disconnect::ByApplication, "", "en")
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_the_dev_channel_uri() {
        assert_eq!(
            server_uri("https://port33765-abc.example.sap").unwrap(),
            "wss://port33765-port33765-abc.example.sap:443"
        );
        assert!(server_uri("not a url").is_err());
    }
}
