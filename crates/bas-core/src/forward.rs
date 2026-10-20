//! Loopback TCP forwarders over the dev channel.
//!
//! The dev channel carries `direct-tcpip` channels; a forwarder is the small
//! piece that accepts local TCP connections and pipes each one through a fresh
//! channel to a port inside the dev space. That is what turns the WebSocket
//! into a normal `127.0.0.1:<port>` SSH endpoint any client can use.

use std::sync::Arc;
use std::time::Duration;

use russh::ChannelMsg;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::devchannel::DevChannel;
use crate::error::{Code, Error, Result};

/// One loopback listener bound to a dev-space port.
pub struct Forward {
    pub remote_port: u16,
    pub local_port: u16,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl Forward {
    /// Bind `local_port` (0 = kernel choice) and forward to `remote_port`.
    pub async fn open(channel: Arc<DevChannel>, remote_port: u16, local_port: u16) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", local_port))
            .await
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::AddrInUse {
                    Error::new(
                        Code::PortInUse,
                        format!("127.0.0.1:{local_port} is already in use"),
                    )
                } else {
                    Error::transport(format!("cannot listen on 127.0.0.1:{local_port}: {error}"))
                }
            })?;
        let bound = listener
            .local_addr()
            .map_err(|error| Error::transport(format!("cannot read the bound port: {error}")))?
            .port();
        let (stop, mut stop_rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    _ = stop_rx.changed() => break,
                    accepted = listener.accept() => accepted,
                };
                let Ok((mut socket, _peer)) = accepted else {
                    break;
                };
                let channel = Arc::clone(&channel);
                tokio::spawn(async move {
                    if let Err(error) = pump(channel, remote_port, &mut socket).await {
                        // A single failed connection must not take the forward
                        // down: the client reconnects, the listener stays.
                        let _ = error;
                    }
                });
            }
        });
        Ok(Self {
            remote_port,
            local_port: bound,
            stop,
            task,
        })
    }

    /// Stop accepting new connections; existing ones end on their own.
    pub fn close(&self) {
        let _ = self.stop.send(true);
        self.task.abort();
    }
}

impl Drop for Forward {
    fn drop(&mut self) {
        self.close();
    }
}

/// Pipe one accepted socket through a `direct-tcpip` channel, both ways.
async fn pump(
    channel: Arc<DevChannel>,
    remote_port: u16,
    socket: &mut tokio::net::TcpStream,
) -> Result<()> {
    let mut ssh = channel.open_forward(remote_port).await?;
    let mut buffer = vec![0u8; 32 * 1024];
    let idle = Duration::from_secs(3600);
    loop {
        tokio::select! {
            read = socket.read(&mut buffer) => {
                let read = read.map_err(|error| Error::transport(format!("local read failed: {error}")))?;
                if read == 0 {
                    let _ = ssh.eof().await;
                    break;
                }
                ssh.data(&buffer[..read])
                    .await
                    .map_err(|error| Error::transport(format!("dev-channel write failed: {error}")))?;
            }
            message = tokio::time::timeout(idle, ssh.wait()) => {
                let Ok(message) = message else { break };
                match message {
                    Some(ChannelMsg::Data { data }) => {
                        socket
                            .write_all(&data)
                            .await
                            .map_err(|error| Error::transport(format!("local write failed: {error}")))?;
                    }
                    Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                    Some(_) => {}
                }
            }
        }
    }
    let _ = socket.shutdown().await;
    Ok(())
}

/// Find out whether a port is free on loopback.
///
/// Binding is the only reliable test: a `connect` check reports a port free
/// when the owner listens on another interface.
pub async fn port_free(port: u16) -> bool {
    match TcpListener::bind(("127.0.0.1", port)).await {
        Ok(listener) => {
            drop(listener);
            true
        }
        Err(error) => error.kind() != std::io::ErrorKind::AddrInUse,
    }
}
