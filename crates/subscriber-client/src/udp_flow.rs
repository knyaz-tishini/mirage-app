//! `EdgeUdpFlow` — client-side counterpart to one accepted
//! `StreamOpen{kind: Udp}` mux stream, carrying `[2B BE len][payload]`-framed
//! UDP datagrams to the edge, both directions
//! (`mirage_control_plane::{encode_udp_datagram_frame,decode_udp_datagram_frame}`,
//! already implemented and tested there — this type is pure plumbing on top,
//! the client-side twin of `EdgeSession::open_tcp_stream`'s TCP flow).

use mirage_control_plane::{decode_udp_datagram_frame, encode_udp_datagram_frame, EdgeSessionError as WireError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::session::EdgeStreamError;
use crate::watchdog::WatchedStream;

/// Largest single `read()` chunk pulled off the underlying mux stream while
/// accumulating a partial datagram frame.
const READ_CHUNK_LEN: usize = 4096;

/// One `StreamKind::Udp` mux stream to the edge — one flow, one fixed
/// destination for its whole life. Constructed only by
/// [`crate::session::EdgeSession::open_udp_flow`].
///
/// The underlying stream is a [`WatchedStream`], so a UDP flow the edge
/// refused (no chain held for this exit) is counted by the session's
/// watchdog just like a refused TCP flow.
pub struct EdgeUdpFlow {
    stream: WatchedStream,
    /// Bytes read off the wire but not yet decoded into a whole datagram
    /// frame — a `recv()` call may need several underlying reads to see one
    /// complete frame.
    read_buf: Vec<u8>,
}

impl EdgeUdpFlow {
    pub(crate) fn new(stream: WatchedStream) -> Self {
        Self { stream, read_buf: Vec::new() }
    }

    /// Encode `payload` as one `[2B len][payload]` frame and write it whole.
    /// Flush so the frame leaves this stream's send queue before the caller
    /// waits for a reply. `write_all` alone only queues it.
    pub async fn send(&mut self, payload: &[u8]) -> Result<(), EdgeStreamError> {
        let frame = encode_udp_datagram_frame(payload).map_err(EdgeStreamError::Codec)?;
        self.stream.write_all(&frame).await?;
        self.stream.flush().await?;
        Ok(())
    }

    /// Read and decode exactly one `[2B len][payload]` frame, buffering a
    /// partial frame across underlying `read` calls — the same accumulation
    /// shape `session::read_welcome` already uses for `EdgeWelcome`.
    pub async fn recv(&mut self) -> Result<Vec<u8>, EdgeStreamError> {
        loop {
            match decode_udp_datagram_frame(&self.read_buf) {
                Ok((payload, consumed)) => {
                    let payload = payload.to_vec();
                    self.read_buf.drain(..consumed);
                    return Ok(payload);
                }
                Err(WireError::Truncated { .. }) => {}
                Err(other) => return Err(EdgeStreamError::Codec(other)),
            }
            let mut chunk = [0u8; READ_CHUNK_LEN];
            let n = self.stream.read(&mut chunk).await?;
            if n == 0 {
                return Err(EdgeStreamError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "edge udp flow: mux stream closed before a full datagram frame arrived",
                )));
            }
            self.read_buf.extend_from_slice(&chunk[..n]);
        }
    }
}
