//! Client-side onion tunnel: establishes a multi-hop circuit and provides
//! a `Tunnel`-compatible wrapper for send/recv.
//!
//! # Connection lifecycle
//!
//! 1. `OnionTunnel::connect` opens TCP to `hops[0]`, builds the full onion
//!    envelope (ephemeral ECDH per hop), and writes it as the first message.
//!    This establishes the circuit — every intermediate hop peels its layer
//!    and opens a connection to the next hop.
//!
//! 2. After the initial onion, the **forward direction is raw**: subsequent
//!    `send()` calls write bytes directly to the socket and they are relayed
//!    without additional wrapping.
//!
//! 3. **Reply direction is framed**: each `recv()` reads `[4B BE len][ciphertext]`
//!    chunks and peels N AEAD layers (one per hop, outermost first).
//!
//! # Cancellation safety
//!
//! `recv()` accumulates bytes into `recv_buf` using `read()` (not `read_exact`).
//! If the future is cancelled mid-read, partially received bytes remain in
//! `recv_buf` and are used on the next call.  No framing desync can occur.

use std::net::SocketAddr;

use async_trait::async_trait;
use mirage_core::{
    config::{ChainConfig, HandshakeStrategy, HopConfig},
    error::MirageError,
};
use mirage_protocol::onion::{
    builder::{build_onion_ephemeral, OnionSession},
    layer::NextHop,
};
use mirage_protocol::traits::{TransportInfo, Tunnel};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{
    tcp::{OwnedReadHalf, OwnedWriteHalf},
    TcpStream,
};
use tracing::{debug, warn};

pub struct OnionTunnel {
    reader: OwnedReadHalf,
    writer: OwnedWriteHalf,
    /// Session keys for reply unwrapping.
    session: OnionSession,
    info: TransportInfo,
    /// Persistent buffer for cancellation-safe framed reads.
    recv_buf: Vec<u8>,
    /// Overflow buffer: plaintext bytes that did not fit in the caller's `buf`
    /// on the previous `recv()` call and must be returned first on the next call.
    plaintext_buf: Vec<u8>,
}

impl OnionTunnel {
    /// Establish an onion circuit based on `chain.hops` + `destination`.
    ///
    /// Builds the full ephemeral onion in one pass, connects to `hops[0]` over
    /// TCP and writes the initial onion envelope. After this call the circuit is
    /// live: subsequent `send()` calls write raw bytes and `recv()` reads framed
    /// reply chunks.
    ///
    /// `initial_user_data` is embedded in the innermost onion layer and forwarded
    /// to the exit destination. Pass `&[]` for circuit-establishment-only (no
    /// initial application payload).
    pub async fn connect(
        chain: &ChainConfig,
        destination: NextHop,
        initial_user_data: &[u8],
    ) -> Result<Self, MirageError> {
        if chain.hops.is_empty() {
            return Err(MirageError::Config(
                "onion connect: chain.hops is empty".into(),
            ));
        }

        match chain.handshake {
            HandshakeStrategy::Ephemeral => {
                // fall through to ephemeral build below
            }
            HandshakeStrategy::Telescopic => {
                return OnionTunnel::connect_telescopic(chain, destination).await;
            }
        }

        let hop0_addr = resolve_hop_addr(&chain.hops[0]).await?;
        debug!(hop0 = %hop0_addr, hops = chain.hops.len(), "onion tunnel: connecting to entry hop");

        let (onion_bytes, session) =
            build_onion_ephemeral(&chain.hops, &destination, initial_user_data)?;

        let mut stream = TcpStream::connect(hop0_addr)
            .await
            .map_err(MirageError::Io)?;
        let len = (onion_bytes.len() as u32).to_be_bytes();
        stream.write_all(&len).await.map_err(MirageError::Io)?;
        stream
            .write_all(&onion_bytes)
            .await
            .map_err(MirageError::Io)?;

        debug!(chain_id = %chain.chain_id, "onion tunnel: initial envelope sent");

        let info = TransportInfo::tcp_raw(hop0_addr);
        let (reader, writer) = stream.into_split();

        Ok(OnionTunnel {
            reader,
            writer,
            session,
            info,
            recv_buf: Vec::new(),
            plaintext_buf: Vec::new(),
        })
    }

    /// Establish an onion circuit using the telescopic handshake strategy.
    ///
    /// Connects to `hops[0]`, sends `CREATE`, verifies `CREATED`, then for
    /// each subsequent hop sends `EXTEND` through the established circuit and
    /// verifies `EXTENDED`.  Once all hops are confirmed the circuit is live
    /// and ready for `RELAY` data frames.
    ///
    /// `destination` is the exit target encoded in the innermost data frame.
    pub async fn connect_telescopic(
        chain: &ChainConfig,
        destination: NextHop,
    ) -> Result<Self, MirageError> {
        use mirage_protocol::onion::{
            builder::TelescopicBuilder,
            telescopic::{CircuitFrame, MsgType},
        };

        if chain.hops.is_empty() {
            return Err(MirageError::Config(
                "onion telescopic: chain.hops is empty".into(),
            ));
        }

        let hop0_addr = resolve_hop_addr(&chain.hops[0]).await?;
        debug!(
            hop0 = %hop0_addr,
            hops = chain.hops.len(),
            "onion telescopic: connecting to entry hop"
        );

        let mut stream = TcpStream::connect(hop0_addr)
            .await
            .map_err(MirageError::Io)?;

        let mut builder = TelescopicBuilder::new(&chain.hops);

        // ── CREATE ────────────────────────────────────────────────────────────
        let create_bytes = builder.create_message();
        stream.write_all(&create_bytes).await.map_err(MirageError::Io)?;

        let created = CircuitFrame::read(&mut stream)
            .await?
            .ok_or_else(|| {
                MirageError::Protocol("telescopic: hop0 closed before CREATED".into())
            })?;
        if created.msg_type != MsgType::Created {
            return Err(MirageError::Protocol(format!(
                "telescopic: expected CREATED, got {:?}",
                created.msg_type
            )));
        }
        builder.process_created(&created.payload)?;
        debug!(hop0 = %hop0_addr, "onion telescopic: hop 0 established");

        // ── EXTEND for each remaining hop ─────────────────────────────────────
        for position in 1..chain.hops.len() {
            let extend_bytes = builder.extend_message(position)?;
            stream.write_all(&extend_bytes).await.map_err(MirageError::Io)?;

            let extended = CircuitFrame::read(&mut stream)
                .await?
                .ok_or_else(|| {
                    MirageError::Protocol(format!(
                        "telescopic: hop{position} closed before EXTENDED"
                    ))
                })?;
            if extended.msg_type != MsgType::Extended {
                return Err(MirageError::Protocol(format!(
                    "telescopic: expected EXTENDED at position {position}, got {:?}",
                    extended.msg_type
                )));
            }
            builder.process_extended(position, &extended.payload)?;
            debug!(position, "onion telescopic: hop established");
        }

        let session = builder.into_session()?;
        debug!(chain_id = %chain.chain_id, "onion telescopic: circuit fully established");

        // ── Initial RELAY frame ───────────────────────────────────────────────
        // Send the destination encoded as a RELAY payload so the exit hop knows
        // where to connect.  The exit hop expects raw onion bytes inside RELAY.
        // Build a minimal onion (destination-only, no user data) using ephemeral
        // session keys and send it as the first RELAY frame.
        use mirage_protocol::onion::{builder::build_onion_ephemeral_at_position, telescopic::CircuitFrame as CF};
        // In telescopic mode relay nodes forward the RELAY payload opaquely —
        // they do not peel onion layers.  The exit must receive a 1-layer onion
        // whose layer key is derived with the exit's absolute position in the
        // chain so that peel_onion_layer (which uses NodeConfig.onion_hop_position)
        // produces Terminal rather than Forward.
        let exit_idx = chain.hops.len().saturating_sub(1);
        let exit_slice = &chain.hops[exit_idx..];
        let (onion_bytes, _) = build_onion_ephemeral_at_position(exit_slice, exit_idx as u8, &destination, &[])?;
        CF {
            msg_type: MsgType::Relay,
            flags: 0,
            payload: onion_bytes,
        }
        .write(&mut stream)
        .await?;

        let info = TransportInfo::tcp_raw(hop0_addr);
        let (reader, writer) = stream.into_split();

        Ok(OnionTunnel {
            reader,
            writer,
            session,
            info,
            recv_buf: Vec::new(),
            plaintext_buf: Vec::new(),
        })
    }
}

/// Resolve a `HopConfig.addr` string to a `SocketAddr`.
async fn resolve_hop_addr(hop: &HopConfig) -> Result<SocketAddr, MirageError> {
    if let Ok(sa) = hop.addr.parse::<SocketAddr>() {
        return Ok(sa);
    }
    let mut iter = tokio::net::lookup_host(&hop.addr)
        .await
        .map_err(MirageError::Io)?;
    iter.next().ok_or_else(|| {
        MirageError::Config(format!(
            "onion: no DNS result for hop.addr={}",
            hop.addr
        ))
    })
}

#[async_trait]
impl Tunnel for OnionTunnel {
    /// Write `data` as a raw forward stream byte sequence.
    ///
    /// After the initial onion envelope the forward direction is unencapsulated:
    /// each intermediate hop simply copies bytes toward the next peer.
    async fn send(&mut self, data: &[u8]) -> Result<(), MirageError> {
        self.writer.write_all(data).await.map_err(MirageError::Io)
    }

    /// Read and unwrap one framed reply chunk from the onion circuit.
    ///
    /// Wire format per chunk: `[4B BE length][AEAD ciphertext]`.
    /// The `AEAD ciphertext` has N layers — one per hop, outermost (entry hop)
    /// first. All layers are stripped before the plaintext is copied into `buf`.
    ///
    /// Uses a persistent `recv_buf` so partial reads from cancelled futures are
    /// not lost — bytes already pulled from the socket stay in the buffer and
    /// are consumed on the next call.
    async fn recv(&mut self, buf: &mut [u8]) -> Result<usize, MirageError> {
        // Return any bytes that overflowed the caller's buffer on the previous call.
        if !self.plaintext_buf.is_empty() {
            let copy_len = self.plaintext_buf.len().min(buf.len());
            buf[..copy_len].copy_from_slice(&self.plaintext_buf[..copy_len]);
            self.plaintext_buf.drain(..copy_len);
            debug!(copy_len, remaining = self.plaintext_buf.len(), "onion recv: returning buffered plaintext overflow");
            return Ok(copy_len);
        }

        let mut tmp = [0u8; 8192];

        // Accumulate until we have the 4-byte length header.
        while self.recv_buf.len() < 4 {
            let n = self.reader.read(&mut tmp).await.map_err(MirageError::Io)?;
            if n == 0 {
                return Ok(0); // EOF
            }
            debug!(n, recv_buf_len = self.recv_buf.len() + n, "onion recv: read bytes (header phase)");
            self.recv_buf.extend_from_slice(&tmp[..n]);
        }

        let len = u32::from_be_bytes([
            self.recv_buf[0],
            self.recv_buf[1],
            self.recv_buf[2],
            self.recv_buf[3],
        ]) as usize;
        debug!(len, "onion recv: decoded frame length from header");
        if len > 65536 {
            return Err(MirageError::Protocol(format!(
                "onion reply chunk too large: {} bytes",
                len
            )));
        }

        let total = 4 + len;

        // Accumulate until we have the full frame.
        while self.recv_buf.len() < total {
            let n = self.reader.read(&mut tmp).await.map_err(MirageError::Io)?;
            if n == 0 {
                return Err(MirageError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "onion tunnel closed mid-frame",
                )));
            }
            debug!(n, recv_buf_len = self.recv_buf.len() + n, total, "onion recv: read bytes (body phase)");
            self.recv_buf.extend_from_slice(&tmp[..n]);
        }
        debug!(total, "onion recv: full frame accumulated");

        match self.session.unwrap_reply(&self.recv_buf[4..total]) {
            Ok(plaintext) => {
                debug!(plaintext_len = plaintext.len(), "onion recv: unwrap_reply succeeded");
                let copy_len = plaintext.len().min(buf.len());
                buf[..copy_len].copy_from_slice(&plaintext[..copy_len]);
                if plaintext.len() > buf.len() {
                    self.plaintext_buf.extend_from_slice(&plaintext[copy_len..]);
                    debug!(
                        overflow = plaintext.len() - copy_len,
                        "onion recv: plaintext overflow buffered"
                    );
                }
                // Drain the consumed frame.
                self.recv_buf.drain(..total);
                Ok(copy_len)
            }
            Err(e) => {
                let wire = &self.recv_buf[4..total];
                let preview_len = wire.len().min(16);
                let preview: Vec<String> = wire[..preview_len].iter().map(|b| format!("{b:02x}")).collect();
                warn!(
                    error = %e,
                    frame_len = len,
                    wire_preview = %preview.join(" "),
                    "onion recv: unwrap_reply failed"
                );
                Err(e)
            }
        }
    }

    async fn close(&mut self) -> Result<(), MirageError> {
        let _ = self.writer.shutdown().await;
        Ok(())
    }

    fn transport_info(&self) -> &TransportInfo {
        &self.info
    }
}
