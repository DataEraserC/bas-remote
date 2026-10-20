//! End-to-end proof of the dev channel without a BAS landscape.
//!
//! A BAS dev channel is *SSH over a WebSocket, with no SSH authentication*
//! (the WebSocket bearer token is the authentication). That shape is the one
//! thing this crate cannot get wrong, and it cannot be verified against a
//! mock — so this test builds the same shape locally:
//!
//!   TCP listener  ->  WebSocket server (sub-protocol "ssh")
//!                 ->  russh server, `none` auth accepted
//!                 ->  sshd-like handlers: exec, and direct-tcpip to an echo port
//!
//! then drives it through `DevChannel` exactly as a dev space would be driven.
//! If `russh`'s WebSocket adapter or the `none` auth path ever regress, this
//! test fails.

// The handshake callback signature is fixed by tungstenite and its error type
// carries a whole HTTP response; that is the library's shape, not a choice here.
#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::time::Duration;

use bas_core::devchannel::DevChannel;
use russh::ChannelMsg;
use russh::server::{Auth, Msg, Session};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Clone)]
struct EchoServer;

impl russh::server::Server for EchoServer {
    type Handler = Self;

    fn new_client(&mut self, _peer: Option<std::net::SocketAddr>) -> Self {
        Self
    }
}

impl russh::server::Handler for EchoServer {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
        // The dev channel authenticates through the WebSocket bearer, so the
        // SSH layer accepts the `none` method.
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: russh::Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        let mut channel = channel;
        tokio::spawn(async move {
            while let Some(message) = channel.wait().await {
                match message {
                    ChannelMsg::Exec { command, .. } => {
                        let command = String::from_utf8_lossy(&command).to_string();
                        let _ = channel.data(format!("ran: {command}").as_bytes()).await;
                        let _ = channel.exit_status(0).await;
                        let _ = channel.eof().await;
                        // A real sshd closes the channel after the exit status;
                        // without this the client waits forever for the close.
                        let _ = channel.close().await;
                        break;
                    }
                    ChannelMsg::Close => break,
                    _ => {}
                }
            }
        });
        Ok(true)
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: russh::Channel<Msg>,
        host_to_connect: &str,
        port_to_connect: u32,
        _origin: &str,
        _origin_port: u32,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        assert_eq!(host_to_connect, "127.0.0.1");
        let mut channel = channel;
        let port = port_to_connect as u16;
        tokio::spawn(async move {
            // Stand in for the dev space's sshd: answer whatever arrives with
            // the port that was reached, proving the channel targets it.
            while let Some(message) = channel.wait().await {
                if let ChannelMsg::Data { data } = message {
                    let reply = format!("port {port}: {}", String::from_utf8_lossy(&data));
                    let _ = channel.data(reply.as_bytes()).await;
                }
            }
        });
        Ok(true)
    }
}

/// Start the WebSocket + SSH server on an ephemeral port.
async fn start_server() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = EchoServer;
    let config = Arc::new(russh::server::Config {
        inactivity_timeout: None,
        auth_rejection_time: Duration::from_millis(0),
        keys: vec![{
            let mut rng = russh::keys::ssh_key::rand_core::OsRng;
            russh::keys::PrivateKey::random(&mut rng, russh::keys::Algorithm::Ed25519).unwrap()
        }],
        ..russh::server::Config::default()
    });
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            let config = Arc::clone(&config);
            let server = server.clone();
            tokio::spawn(async move {
                let Ok(ws) = tokio_tungstenite::accept_hdr_async(
                    socket,
                    |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                     mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                        // The dev channel requires the "ssh" sub-protocol and a
                        // bearer token; assert both, so a regression in either
                        // side of the handshake is caught here.
                        let protocol = request
                            .headers()
                            .get("sec-websocket-protocol")
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        assert_eq!(protocol, "ssh", "the client must ask for the ssh sub-protocol");
                        let auth = request
                            .headers()
                            .get("authorization")
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        assert_eq!(auth, "bearer test-jwt", "the bearer token must be sent");
                        response
                            .headers_mut()
                            .insert("sec-websocket-protocol", "ssh".parse().unwrap());
                        Ok(response)
                    },
                )
                .await
                else {
                    return;
                };
                let adapter = bas_core::wss::WsByteStream::new(ws);
                let _ = russh::server::run_stream(config, adapter, server.clone()).await;
            });
        }
    });
    port
}

/// Point a `DevChannel` at the local server.
///
/// The crate derives `wss://port33765-<host>:443` from the runtime URL, which
/// a local server cannot serve, so the test uses the explicit-URI entry point;
/// everything after the URI is the production code path.
async fn connect(port: u16) -> DevChannel {
    DevChannel::connect_with_uri(
        "test-jwt",
        &format!("ws://127.0.0.1:{port}"),
        "user",
        1,
        Duration::from_secs(10),
    )
    .await
    .expect("the dev channel must connect")
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_over_websocket_ssh() {
    let port = start_server().await;
    let channel = connect(port).await;
    let output = channel
        .exec("whoami", Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(output.code, 0);
    assert_eq!(output.stdout, "ran: whoami");
    channel.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn forward_reaches_the_requested_remote_port() {
    let port = start_server().await;
    let channel = Arc::new(connect(port).await);

    // The forward is the piece that turns the channel into a normal localhost
    // endpoint: bind, accept, pipe, and the far side sees the remote port.
    let forward = bas_core::forward::Forward::open(Arc::clone(&channel), 2222, 0)
        .await
        .unwrap();
    assert_ne!(forward.local_port, 0);

    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", forward.local_port))
        .await
        .unwrap();
    socket.write_all(b"ping").await.unwrap();
    let mut buffer = vec![0u8; 64];
    let read = tokio::time::timeout(Duration::from_secs(10), socket.read(&mut buffer))
        .await
        .expect("the forwarded port must answer")
        .unwrap();
    assert_eq!(&buffer[..read], b"port 2222: ping");
    // Dropping the forward must release its reference; closing through the
    // shared handle then works without unwrapping the Arc.
    drop(forward);
    channel.close().await;
}
