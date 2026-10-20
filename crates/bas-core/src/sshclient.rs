//! SSH client for the dev space's own sshd, reached through a tunnel port.
//!
//! The dev space authenticates the landscape-issued private key, so this is an
//! ordinary key-based SSH client. It exists (rather than reusing the dev
//! channel) because the SFTP bridge needs an exec channel into the dev space
//! *before* the bridge is up, and because a host may point it at any SSH port.

use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, Handle};
use russh::keys::{PrivateKey, PrivateKeyWithHashAlg};
use tokio::net::TcpStream;

use crate::error::{Code, Error, Result};
use crate::model::{ExecOutput, StatInfo};

/// SSH client handler that accepts whatever host key the dev space presents.
///
/// The transport is a loopback port that this process opened itself through the
/// dev channel, so there is no man in the middle to defend against; a
/// known-hosts entry would only break on every dev-space rebuild.
struct SshdClient;

impl client::Handler for SshdClient {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> std::result::Result<bool, Self::Error> {
        Ok(true)
    }
}

/// One authenticated SSH session to a dev space.
pub struct SshClient {
    handle: Handle<SshdClient>,
}

impl SshClient {
    /// Connect and authenticate with a private key.
    pub async fn connect(
        port: u16,
        key_pem: &str,
        username: &str,
        timeout: Duration,
    ) -> Result<Self> {
        let key = PrivateKey::from_openssh(key_pem.trim()).map_err(|error| {
            Error::invalid(format!("cannot read the dev-space private key: {error}"))
        })?;
        let config = Arc::new(client::Config {
            inactivity_timeout: Some(Duration::from_secs(300)),
            ..client::Config::default()
        });
        let socket = tokio::time::timeout(timeout, TcpStream::connect(("127.0.0.1", port)))
            .await
            .map_err(|_| {
                Error::transport(format!(
                    "127.0.0.1:{port} did not accept a connection within {timeout:?}"
                ))
            })?
            .map_err(|error| Error::transport(format!("cannot reach 127.0.0.1:{port}: {error}")))?;

        let mut handle =
            tokio::time::timeout(timeout, client::connect_stream(config, socket, SshdClient))
                .await
                .map_err(|_| {
                    Error::transport(format!("the SSH handshake on port {port} timed out"))
                })?
                .map_err(|error| {
                    Error::transport(format!("the SSH handshake on port {port} failed: {error}"))
                })?;

        let key = PrivateKeyWithHashAlg::new(Arc::new(key), None);
        let result = tokio::time::timeout(timeout, handle.authenticate_publickey(username, key))
            .await
            .map_err(|_| Error::transport("the dev space did not answer the SSH authentication"))?
            .map_err(|error| {
                Error::transport(format!("the dev space rejected the SSH key: {error}"))
            })?;
        if !result.success() {
            return Err(Error::new(
                Code::Upstream,
                format!("the dev space rejected the key of {username}"),
            ));
        }
        Ok(Self { handle })
    }

    /// Run one command over an exec channel.
    pub async fn exec(&self, command: &str, timeout: Duration) -> Result<ExecOutput> {
        let mut channel =
            self.handle.channel_open_session().await.map_err(|error| {
                Error::transport(format!("cannot open an exec channel: {error}"))
            })?;
        channel.exec(true, command).await.map_err(|error| {
            Error::transport(format!("cannot start the remote command: {error}"))
        })?;
        crate::exec::drain(&mut channel, timeout).await
    }

    /// Stat one path over SFTP.
    ///
    /// SFTP is used rather than `stat(1)` because it reports the type and mode
    /// in one round trip and does not depend on the image's coreutils.
    pub async fn stat(&self, path: &str) -> Result<StatInfo> {
        let sftp = self.sftp().await?;
        match sftp.metadata(path.to_string()).await {
            Ok(meta) => {
                let mode = meta.permissions.unwrap_or(0);
                Ok(StatInfo {
                    exists: true,
                    is_dir: meta.is_dir(),
                    size: meta.size.unwrap_or(0),
                    mode,
                })
            }
            Err(russh_sftp::client::error::Error::Status(status))
                if status.status_code == russh_sftp::protocol::StatusCode::NoSuchFile =>
            {
                Ok(StatInfo::default())
            }
            Err(error) => Err(Error::upstream(format!("cannot stat {path}: {error}"))),
        }
    }

    /// Open an SFTP client over a subsystem channel.
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
    pub async fn close(self) {
        let _ = self
            .handle
            .disconnect(russh::Disconnect::ByApplication, "", "en")
            .await;
    }
}
