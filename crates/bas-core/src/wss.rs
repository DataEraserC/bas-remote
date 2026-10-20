//! Adapts a WebSocket to the byte stream an SSH transport expects.
//!
//! `tokio-tungstenite` exposes a WebSocket as `Stream + Sink`, while `russh`
//! wants `AsyncRead + AsyncWrite`. The gap is not cosmetic:
//!
//! * a WebSocket *message* boundary means nothing to SSH — the transport is a
//!   byte stream — so a message larger than the caller's buffer must be kept
//!   and served in pieces;
//! * `Sink::start_send` only queues a message; without a flush it never reaches
//!   the wire. An adapter that forgets this deadlocks the handshake: both sides
//!   sit waiting for the other's version string, and nothing is ever sent.
//!
//! This module therefore owns an explicit write buffer and drives the flush
//! from *both* `poll_write` and `poll_read`, so progress does not depend on the
//! peer calling `flush`.

use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::{Sink, SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_tungstenite::tungstenite::Message;

/// A WebSocket seen as a byte stream.
pub struct WsByteStream<S> {
    inner: tokio_tungstenite::WebSocketStream<S>,
    /// Bytes of the last binary message the caller's buffer could not take.
    pending: Vec<u8>,
    /// Bytes accepted from the caller but not yet flushed to the socket.
    outgoing: Vec<u8>,
    /// Set when the socket will produce no more bytes.
    read_closed: bool,
}

impl<S> WsByteStream<S> {
    /// Wrap an established WebSocket.
    #[must_use]
    pub fn new(inner: tokio_tungstenite::WebSocketStream<S>) -> Self {
        Self {
            inner,
            pending: Vec::new(),
            outgoing: Vec::new(),
            read_closed: false,
        }
    }

    /// Unwrap the socket (for shutdown paths).
    #[must_use]
    pub fn into_inner(self) -> tokio_tungstenite::WebSocketStream<S> {
        self.inner
    }
}

impl<S> AsyncRead for WsByteStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        // Reading is a natural place to make progress on queued writes: an SSH
        // peer writes its version string and then immediately reads the peer's,
        // and nothing else would drive the flush.
        let _ = this.drive_write(cx);

        if !this.pending.is_empty() {
            let take = this.pending.len().min(buf.remaining());
            let rest = this.pending.split_off(take);
            buf.put_slice(&this.pending);
            this.pending = rest;
            return Poll::Ready(Ok(()));
        }
        if this.read_closed {
            return Poll::Ready(Ok(()));
        }
        loop {
            match this.inner.poll_next_unpin(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    this.read_closed = true;
                    return Poll::Ready(Ok(()));
                }
                Poll::Ready(Some(Err(error))) => {
                    return Poll::Ready(Err(std::io::Error::other(error.to_string())));
                }
                Poll::Ready(Some(Ok(Message::Binary(bytes)))) => {
                    let take = bytes.len().min(buf.remaining());
                    buf.put_slice(&bytes[..take]);
                    if take < bytes.len() {
                        this.pending = bytes[take..].to_vec();
                    }
                    return Poll::Ready(Ok(()));
                }
                // Ping/pong/close are WebSocket-level and never reach SSH.
                Poll::Ready(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
                Poll::Ready(Some(Ok(Message::Close(_)))) => {
                    this.read_closed = true;
                    return Poll::Ready(Ok(()));
                }
                Poll::Ready(Some(Ok(Message::Text(_) | Message::Frame(_)))) => continue,
            }
        }
    }
}

impl<S> WsByteStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Push `outgoing` at the socket. Returns whether it all went out.
    fn drive_write(&mut self, cx: &mut Context<'_>) -> bool {
        while !self.outgoing.is_empty() {
            match Sink::poll_ready(Pin::new(&mut self.inner), cx) {
                Poll::Pending => return false,
                Poll::Ready(Err(_)) => {
                    // The socket is gone; dropping the buffer is the only sane
                    // outcome, and the next read reports the closure.
                    self.outgoing.clear();
                    return false;
                }
                Poll::Ready(Ok(())) => {
                    let chunk = std::mem::take(&mut self.outgoing);
                    if let Err(error) = self.inner.start_send_unpin(Message::Binary(chunk.into())) {
                        self.outgoing.clear();
                        let _ = error;
                        return false;
                    }
                    match Sink::poll_flush(Pin::new(&mut self.inner), cx) {
                        Poll::Pending => return false,
                        Poll::Ready(Err(_)) => return false,
                        Poll::Ready(Ok(())) => {}
                    }
                }
            }
        }
        true
    }
}

impl<S> AsyncWrite for WsByteStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        // Backpressure: do not accept more than the socket has taken, so a slow
        // peer cannot grow the buffer without bound.
        if !this.drive_write(cx) {
            return Poll::Pending;
        }
        this.outgoing.extend_from_slice(data);
        // Try to get it out now; if the socket is not ready, the bytes stay
        // buffered and the next poll (read or write) drives them.
        let _ = this.drive_write(cx);
        Poll::Ready(Ok(data.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.drive_write(cx) {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let _ = this.drive_write(cx);
        Sink::poll_close(Pin::new(&mut this.inner), cx)
            .map_err(|error| std::io::Error::other(error.to_string()))
    }
}
