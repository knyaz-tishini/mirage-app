//! Monitor channel abstraction — `DirectWs` and `OverChain` transports.
//!
//! [`MonitorChannel`] is a trait with two concrete implementations:
//!
//! - [`DirectWsChannel`] — standard WebSocket subscribe (existing code, refactored
//!   into the channel abstraction).
//! - [`OverChainChannel`] — length-prefixed binary frame protocol over a Mirage
//!   tunnel.  Used when all DirectWs URLs are unreachable.
//!
//! # Frame format (OverChain only)
//!
//! ```text
//! +---------+---------+------------------+
//! | Tag (1) | Len (4) | Payload (Len)    |
//! +---------+---------+------------------+
//! ```
//!
//! All multi-byte integers are big-endian.
//!
//! Tags:
//! - `0x00` `FRAME_AUTH_CHALLENGE` — server → client, first frame; 8-byte nonce
//! - `0x01` `FRAME_AUTH`           — client → server; auth payload
//! - `0x02` `FRAME_MSG`            — server → client; UTF-8 JSON (`TaggedServerFrame`)
//! - `0x03` `FRAME_PING`           — server → client; 8-byte nonce keepalive
//! - `0x04` `FRAME_PONG`           — client → server; echoed 8-byte nonce
//! - `0x05` `FRAME_CLOSE`          — either direction; graceful shutdown

use std::{net::SocketAddr, sync::Arc, time::Duration};

use futures::StreamExt;
use mirage_core::error::MirageError;
use mirage_protocol::traits::Tunnel;
use thiserror::Error;
use tokio::time::timeout;
use tokio_tungstenite::{
    connect_async_tls_with_config,
    tungstenite::{client::IntoClientRequest, Message},
};
use tracing::{debug, info, warn};

use crate::chain_set::{ChainId, ChainSet};

// ── Frame tag constants ───────────────────────────────────────────────────────

pub const FRAME_AUTH_CHALLENGE: u8 = 0x00;
pub const FRAME_AUTH: u8 = 0x01;
pub const FRAME_MSG: u8 = 0x02;
pub const FRAME_PING: u8 = 0x03;
pub const FRAME_PONG: u8 = 0x04;
pub const FRAME_CLOSE: u8 = 0x05;

/// Wire version for the over-chain control protocol.
pub const OVER_CHAIN_VERSION: u8 = 0x01;

// ── Error type ────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum MonitorChannelError {
    #[error("WebSocket error: {0}")]
    Ws(String),

    #[error("frame I/O error: {0}")]
    Io(String),

    #[error("auth handshake failed: {0}")]
    Auth(String),

    #[error("unexpected frame tag {0:#04x}")]
    UnexpectedTag(u8),

    #[error("channel closed")]
    Closed,

    #[error("chain not found: {0}")]
    ChainNotFound(String),

    #[error("pool lease error: {0}")]
    Lease(#[from] MirageError),

    #[error("timeout")]
    Timeout,
}

impl From<tokio_tungstenite::tungstenite::Error> for MonitorChannelError {
    fn from(e: tokio_tungstenite::tungstenite::Error) -> Self {
        Self::Ws(e.to_string())
    }
}

// ── Trait ─────────────────────────────────────────────────────────────────────

/// Bidirectional monitor control channel.
///
/// Hides whether the underlying transport is a WebSocket (`DirectWs`) or a
/// length-prefixed binary stream over a Mirage tunnel (`OverChain`).
pub trait MonitorChannel: Send {
    /// Read the next server-pushed JSON frame payload (raw UTF-8 bytes).
    ///
    /// Returns `None` on clean close.  Implementations handle ping/pong
    /// internally and only surface `FRAME_MSG` payloads (or WS text frames).
    fn recv_frame(
        &mut self,
    ) -> impl std::future::Future<Output = Result<Option<Vec<u8>>, MonitorChannelError>> + Send;

    /// Send a pong reply carrying the nonce from a server ping.
    fn send_pong(
        &mut self,
        nonce: [u8; 8],
    ) -> impl std::future::Future<Output = Result<(), MonitorChannelError>> + Send;

    /// Send a UTF-8 JSON text frame from client to server.
    ///
    /// For `DirectWs` this is a WS Text message; for `OverChain` this is a
    /// `FRAME_MSG` carrying the JSON bytes.
    fn send_text(
        &mut self,
        text: &str,
    ) -> impl std::future::Future<Output = Result<(), MonitorChannelError>> + Send;

    /// Graceful close.
    fn close(
        self,
    ) -> impl std::future::Future<Output = Result<(), MonitorChannelError>> + Send;
}

// ── Shared frame codec ────────────────────────────────────────────────────────

/// Encode a tagged frame: `[tag(1)][len(4BE)][payload]`.
pub fn encode_frame(tag: u8, payload: &[u8]) -> Vec<u8> {
    let len = payload.len() as u32;
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.push(tag);
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Read one frame from a `Tunnel`.  Returns `(tag, payload)`.
///
/// Reads exactly 5 header bytes then `len` payload bytes.
pub async fn read_tunnel_frame(
    tunnel: &mut dyn Tunnel,
) -> Result<(u8, Vec<u8>), MonitorChannelError> {
    let mut header = [0u8; 5];
    read_exact_tunnel(tunnel, &mut header).await?;
    let tag = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;

    if len > 4 * 1024 * 1024 {
        return Err(MonitorChannelError::Io(format!(
            "frame too large: {len} bytes"
        )));
    }

    let mut payload = vec![0u8; len];
    if len > 0 {
        read_exact_tunnel(tunnel, &mut payload).await?;
    }
    Ok((tag, payload))
}

async fn read_exact_tunnel(
    tunnel: &mut dyn Tunnel,
    buf: &mut [u8],
) -> Result<(), MonitorChannelError> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = tunnel
            .recv(&mut buf[filled..])
            .await
            .map_err(MonitorChannelError::Lease)?;
        if n == 0 {
            return Err(MonitorChannelError::Closed);
        }
        filled += n;
    }
    Ok(())
}

async fn write_tunnel_frame(
    tunnel: &mut dyn Tunnel,
    tag: u8,
    payload: &[u8],
) -> Result<(), MonitorChannelError> {
    let frame = encode_frame(tag, payload);
    tunnel
        .send(&frame)
        .await
        .map_err(MonitorChannelError::Lease)
}

// ── DirectWsChannel ───────────────────────────────────────────────────────────

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Monitor channel over a standard WebSocket subscribe connection.
pub struct DirectWsChannel {
    /// The unsplit WebSocket stream.  We don't need concurrent read+write for
    /// the WS path (the client only reads; tungstenite handles auto-pong).
    stream: WsStream,
}

impl DirectWsChannel {
    /// Connect to `url` with optional Bearer auth.
    ///
    /// `url` should be a `wss://` or `ws://` URL for the subscribe endpoint.
    pub async fn connect(
        url: &str,
        bearer: Option<&str>,
    ) -> Result<Self, MonitorChannelError> {
        let mut req = url
            .into_client_request()
            .map_err(|e| MonitorChannelError::Ws(format!("build WS request: {e}")))?;

        if let Some(key) = bearer {
            let header_val = format!("Bearer {key}")
                .parse::<tokio_tungstenite::tungstenite::http::HeaderValue>()
                .map_err(|e| MonitorChannelError::Ws(format!("invalid auth header: {e}")))?;
            req.headers_mut().insert(
                tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
                header_val,
            );
        }

        let (stream, _) = connect_async_tls_with_config(req, None, false, None)
            .await
            .map_err(|e| MonitorChannelError::Ws(format!("WS upgrade: {e}")))?;

        Ok(Self { stream })
    }
}

impl MonitorChannel for DirectWsChannel {
    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, MonitorChannelError> {
        loop {
            match self.stream.next().await {
                Some(Ok(Message::Text(text))) => {
                    return Ok(Some(text.into_bytes()));
                }
                Some(Ok(Message::Ping(_))) => {
                    // tungstenite auto-replies with Pong; nothing to do.
                }
                Some(Ok(Message::Close(_))) | None => {
                    return Ok(None);
                }
                Some(Ok(_)) => {
                    // Ignore binary / pong frames.
                }
                Some(Err(e)) => {
                    return Err(MonitorChannelError::Ws(format!("WS stream error: {e}")));
                }
            }
        }
    }

    async fn send_pong(&mut self, _nonce: [u8; 8]) -> Result<(), MonitorChannelError> {
        // WS ping/pong is handled by tungstenite automatically.
        Ok(())
    }

    async fn send_text(&mut self, text: &str) -> Result<(), MonitorChannelError> {
        use futures::SinkExt;
        self.stream
            .send(Message::Text(text.to_owned().into()))
            .await
            .map_err(|e| MonitorChannelError::Ws(format!("WS send_text: {e}")))
    }

    async fn close(mut self) -> Result<(), MonitorChannelError> {
        use futures::SinkExt;
        self.stream
            .send(Message::Close(None))
            .await
            .map_err(|e| MonitorChannelError::Ws(format!("WS close: {e}")))?;
        Ok(())
    }
}

// ── OverChainChannel ──────────────────────────────────────────────────────────

/// Monitor channel over a Mirage tunnel using the length-prefixed binary frame
/// protocol.
///
/// After construction via [`OverChainChannel::connect`] the channel is
/// authenticated and ready to receive `FRAME_MSG` payloads.
pub struct OverChainChannel {
    tunnel: Box<dyn Tunnel>,
}

impl OverChainChannel {
    /// Open a connection to `monitor_addr` through the named chain, then
    /// complete the `AUTH_CHALLENGE` → `AUTH` → `AUTH_OK` handshake.
    ///
    /// # Arguments
    ///
    /// * `chain_set` — registry of all available chains.
    /// * `chain_id`  — which chain to route through.
    /// * `monitor_addr` — the monitor's over-chain listener address
    ///   (`127.0.0.1:19445` by default).
    /// * `bearer`    — same Bearer token used for WS auth.
    ///
    /// Times out after 5 s for both the pool lease and the auth handshake.
    pub async fn connect(
        chain_set: &Arc<ChainSet>,
        chain_id: &ChainId,
        monitor_addr: SocketAddr,
        bearer: &str,
    ) -> Result<Self, MonitorChannelError> {
        const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
        const AUTH_TIMEOUT: Duration = Duration::from_secs(10);

        // Resolve chain handle.
        let handle = chain_set
            .chains
            .get(chain_id)
            .map(|e| Arc::clone(e.value()))
            .ok_or_else(|| MonitorChannelError::ChainNotFound(chain_id.to_string()))?;

        // Lease a tunnel from the pool.
        info!(chain_id = %chain_id, "over_chain: leasing tunnel from pool");
        let mut lease = timeout(CONNECT_TIMEOUT, handle.pool.lease())
            .await
            .map_err(|_| {
                warn!(chain_id = %chain_id, "over_chain: pool lease TIMED OUT after {CONNECT_TIMEOUT:?}");
                MonitorChannelError::Timeout
            })?
            .map_err(|e| {
                warn!(chain_id = %chain_id, "over_chain: pool lease FAILED: {e}");
                MonitorChannelError::Lease(e)
            })?;
        info!(chain_id = %chain_id, "over_chain: tunnel leased from pool");

        // Send target address to the relay entry node (same as SOCKS5 does).
        let target_str = monitor_addr.to_string();
        lease
            .send(target_str.as_bytes())
            .await
            .map_err(MonitorChannelError::Lease)?;

        info!(
            chain_id = %chain_id,
            addr = %monitor_addr,
            "over_chain: tunnel leased, starting auth handshake"
        );

        // Extract the inner tunnel so we can pass `&mut dyn Tunnel` to the
        // auth handshake. We poison the lease slot so the pool reconnects it
        // for future flows — this tunnel is owned by the control channel now.
        lease.poison();
        let mut inner = lease
            .take_tunnel()
            .ok_or_else(|| MonitorChannelError::Io("lease tunnel already taken".to_string()))?;

        // Auth handshake (with timeout).
        timeout(AUTH_TIMEOUT, Self::do_auth_handshake(inner.as_mut(), bearer))
            .await
            .map_err(|_| MonitorChannelError::Timeout)??;

        info!(chain_id = %chain_id, "over_chain: AUTH_OK — channel ready");

        Ok(Self { tunnel: inner })
    }

    /// Perform the AUTH_CHALLENGE / AUTH / AUTH_OK handshake.
    async fn do_auth_handshake(
        tunnel: &mut dyn Tunnel,
        bearer: &str,
    ) -> Result<(), MonitorChannelError> {
        // Step 1: read FRAME_AUTH_CHALLENGE with 8-byte nonce.
        let (tag, payload) = read_tunnel_frame(tunnel).await?;
        if tag != FRAME_AUTH_CHALLENGE {
            return Err(MonitorChannelError::UnexpectedTag(tag));
        }
        if payload.len() < 8 {
            return Err(MonitorChannelError::Auth(
                "AUTH_CHALLENGE nonce too short".to_string(),
            ));
        }
        let mut nonce_echo = [0u8; 8];
        nonce_echo.copy_from_slice(&payload[..8]);

        debug!("over_chain: received AUTH_CHALLENGE, sending AUTH");

        // Step 2: build and send FRAME_AUTH.
        let bearer_bytes = bearer.as_bytes();
        let bearer_len = bearer_bytes.len() as u16;
        let mut auth_payload = Vec::with_capacity(1 + 2 + bearer_bytes.len() + 8);
        auth_payload.push(OVER_CHAIN_VERSION);
        auth_payload.extend_from_slice(&bearer_len.to_be_bytes());
        auth_payload.extend_from_slice(bearer_bytes);
        auth_payload.extend_from_slice(&nonce_echo);

        write_tunnel_frame(tunnel, FRAME_AUTH, &auth_payload).await?;

        // Step 3: read AUTH_OK (tag 0x02 is FRAME_MSG in design;
        // auth response uses a synthetic tag — per design doc §2 the server
        // sends FRAME_AUTH_OK implicitly as the first FRAME_MSG with empty
        // payload, OR closes on failure).
        //
        // Per design doc the server either sends AUTH_OK (closes is failure).
        // We treat any FRAME_MSG with empty payload or a special tag as OK.
        // The design doc says server replies FRAME_AUTH_OK or closes.
        // Since FRAME_AUTH_OK is not in the 5-tag set above, we interpret:
        //   - FRAME_CLOSE → auth rejected
        //   - any other tag as unexpected
        //   - After re-reading design §2: AUTH_OK is implicit — the server
        //     just starts pushing FRAME_MSG after verifying AUTH. So we
        //     accept any non-CLOSE tag as success and buffer it.
        //
        // We read one more frame: if FRAME_CLOSE → auth failed.
        // If FRAME_MSG → auth OK, the payload is the first push.
        let (ack_tag, _ack_payload) = read_tunnel_frame(tunnel).await?;
        match ack_tag {
            FRAME_CLOSE => {
                return Err(MonitorChannelError::Auth(
                    "server rejected credentials".to_string(),
                ));
            }
            FRAME_MSG => {
                // Auth OK — first config push arrived.
                debug!("over_chain: AUTH accepted (first FRAME_MSG received)");
                Ok(())
            }
            _ => Err(MonitorChannelError::UnexpectedTag(ack_tag)),
        }
    }
}

impl MonitorChannel for OverChainChannel {
    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, MonitorChannelError> {
        loop {
            let (tag, payload) = match read_tunnel_frame(self.tunnel.as_mut()).await {
                Ok(f) => f,
                Err(MonitorChannelError::Closed) => return Ok(None),
                Err(e) => return Err(e),
            };

            match tag {
                FRAME_MSG => {
                    return Ok(Some(payload));
                }
                FRAME_PING => {
                    if payload.len() < 8 {
                        warn!("over_chain: FRAME_PING with short payload — ignoring");
                        continue;
                    }
                    let mut nonce = [0u8; 8];
                    nonce.copy_from_slice(&payload[..8]);
                    self.send_pong(nonce).await?;
                }
                FRAME_CLOSE => {
                    return Ok(None);
                }
                other => {
                    warn!(tag = other, "over_chain: unexpected frame tag — skipping");
                }
            }
        }
    }

    async fn send_pong(&mut self, nonce: [u8; 8]) -> Result<(), MonitorChannelError> {
        write_tunnel_frame(self.tunnel.as_mut(), FRAME_PONG, &nonce).await
    }

    async fn send_text(&mut self, text: &str) -> Result<(), MonitorChannelError> {
        write_tunnel_frame(self.tunnel.as_mut(), FRAME_MSG, text.as_bytes()).await
    }

    async fn close(mut self) -> Result<(), MonitorChannelError> {
        write_tunnel_frame(self.tunnel.as_mut(), FRAME_CLOSE, &[]).await
    }
}


// ── Unit test — in-memory AUTH handshake ──────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use mirage_core::error::MirageError;
    use mirage_protocol::traits::{TransportInfo, Tunnel};
    use std::collections::VecDeque;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// Paired in-memory duplex tunnels for testing.
    struct MemTunnel {
        send_buf: Arc<Mutex<VecDeque<u8>>>,
        recv_buf: Arc<Mutex<VecDeque<u8>>>,
        info: TransportInfo,
    }

    impl MemTunnel {
        fn pair() -> (MemTunnel, MemTunnel) {
            let a_to_b: Arc<Mutex<VecDeque<u8>>> = Arc::new(Mutex::new(VecDeque::new()));
            let b_to_a: Arc<Mutex<VecDeque<u8>>> = Arc::new(Mutex::new(VecDeque::new()));
            let info: TransportInfo = TransportInfo::tcp_raw("127.0.0.1:0".parse().unwrap());
            let client = MemTunnel {
                send_buf: Arc::clone(&a_to_b),
                recv_buf: Arc::clone(&b_to_a),
                info: info.clone(),
            };
            let server = MemTunnel {
                send_buf: Arc::clone(&b_to_a),
                recv_buf: Arc::clone(&a_to_b),
                info,
            };
            (client, server)
        }
    }

    #[async_trait::async_trait]
    impl Tunnel for MemTunnel {
        async fn send(&mut self, data: &[u8]) -> Result<(), MirageError> {
            let mut buf = self.send_buf.lock().await;
            buf.extend(data.iter().copied());
            Ok(())
        }

        async fn recv(&mut self, buf: &mut [u8]) -> Result<usize, MirageError> {
            loop {
                {
                    let mut src = self.recv_buf.lock().await;
                    if !src.is_empty() {
                        let n = buf.len().min(src.len());
                        for (i, b) in src.drain(..n).enumerate() {
                            buf[i] = b;
                        }
                        return Ok(n);
                    }
                }
                tokio::task::yield_now().await;
            }
        }

        async fn close(&mut self) -> Result<(), MirageError> {
            Ok(())
        }

        fn transport_info(&self) -> &TransportInfo {
            &self.info
        }
    }

    /// Fake server: sends AUTH_CHALLENGE, reads AUTH, sends FRAME_MSG (AUTH_OK
    /// signal), then sends one config push.
    async fn fake_server(mut server: MemTunnel, expected_bearer: &str) {
        let srv: &mut dyn mirage_protocol::traits::Tunnel = &mut server;

        // Send AUTH_CHALLENGE with nonce = [1,2,3,4,5,6,7,8].
        let nonce: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
        write_tunnel_frame(srv, FRAME_AUTH_CHALLENGE, &nonce)
            .await
            .unwrap();

        // Read AUTH frame.
        let (tag, payload) = read_tunnel_frame(srv).await.unwrap();
        assert_eq!(tag, FRAME_AUTH, "expected FRAME_AUTH from client");

        // Parse AUTH payload: [version(1)][bearer_len(2)][bearer][nonce_echo(8)]
        assert!(payload.len() >= 3);
        let bearer_len = u16::from_be_bytes([payload[1], payload[2]]) as usize;
        assert!(payload.len() >= 3 + bearer_len + 8, "AUTH payload too short");
        let bearer_str = std::str::from_utf8(&payload[3..3 + bearer_len]).unwrap();
        assert_eq!(bearer_str, expected_bearer, "bearer mismatch");
        let echoed_nonce = &payload[3 + bearer_len..3 + bearer_len + 8];
        assert_eq!(echoed_nonce, &nonce, "nonce echo mismatch");

        // Send AUTH_OK as FRAME_MSG with a fake config payload.
        let config_json = br#"{"kind":"config","version":1,"chains":[]}"#;
        write_tunnel_frame(srv, FRAME_MSG, config_json)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn over_chain_auth_handshake_roundtrip() {
        let bearer = "test-bearer-token";
        let (client_tunnel, server_tunnel) = MemTunnel::pair();

        // Run fake server concurrently.
        let server_task = tokio::spawn(async move {
            fake_server(server_tunnel, bearer).await;
        });

        // Run client handshake.
        let mut client_tunnel = client_tunnel;
        let client_result =
            OverChainChannel::do_auth_handshake(&mut client_tunnel, bearer).await;

        server_task.await.expect("fake server panicked");
        assert!(
            client_result.is_ok(),
            "auth handshake failed: {client_result:?}"
        );
    }
}
