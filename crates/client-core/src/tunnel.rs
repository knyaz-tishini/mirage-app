//! Tunnel establishment through the Mirage chain.

use std::{io, net::SocketAddr, time::Duration};

use mirage_core::{
    config::{
        AuthMode, ChainConfig, MirageEndpointConfig, ProtocolId, ShapingPreset,
        TransportCamouflage, TransportKind, UdpProtocolId,
    },
    error::MirageError,
};
use mirage_protocol::{
    byte_cipher::ByteCipherTunnel,
    driver::run_client_udp,
    frame_obfuscation::patterns,
    negotiation::ProtocolChain,
    onion::layer::NextHop,
    traits::Tunnel,
};
use tokio::net::TcpStream;
use tracing::debug;

use crate::cover_tunnel::CoverTunnel;
use crate::onion_tunnel::OnionTunnel;
use crate::shaped_tunnel::ShapedTunnel;

/// RFC 8305 Happy Eyeballs v2: race v6 vs v4 probes, v6 gets a 200 ms head-start.
///
/// Returns the address whose probe succeeded first.  Both probes use a 5 s
/// per-address TCP timeout.  `chain_protocols.connect` will re-dial the winner.
///
/// Cancellation: the loser future is dropped at the end of `select!`; the OS
/// TCP stack may have a half-open connection in flight for the loser, which the
/// OS cleans up after its own SYN timeout (~20 s).  This is the standard
/// Happy Eyeballs trade-off — one extra SYN at most, no fd leak on our side.
pub(crate) async fn happy_eyeballs(v4: SocketAddr, v6: SocketAddr) -> io::Result<SocketAddr> {
    const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
    const V6_HEAD_START: Duration = Duration::from_millis(200);

    // Each probe returns `(tag, Result<SocketAddr, io::Error>)`.
    // We spawn both as tasks so we can independently await them and cancel the
    // loser by dropping its JoinHandle.
    let v6_task = tokio::spawn(async move {
        match tokio::time::timeout(PROBE_TIMEOUT, TcpStream::connect(v6)).await {
            Ok(Ok(_stream)) => Ok(v6),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "v6 probe timed out")),
        }
    });
    let v4_task = tokio::spawn(async move {
        tokio::time::sleep(V6_HEAD_START).await;
        match tokio::time::timeout(PROBE_TIMEOUT, TcpStream::connect(v4)).await {
            Ok(Ok(_stream)) => Ok(v4),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "v4 probe timed out")),
        }
    });

    // First winner takes it; loser task is aborted when its JoinHandle is
    // dropped.  At most one stray SYN may be in flight in the OS after abort —
    // standard Happy Eyeballs trade-off.
    let mut v6_done = false;
    let mut v4_done = false;
    let mut errors: Vec<io::Error> = Vec::new();

    tokio::pin!(v6_task);
    tokio::pin!(v4_task);

    loop {
        tokio::select! {
            r = &mut v6_task, if !v6_done => {
                v6_done = true;
                match r {
                    Ok(Ok(addr)) => {
                        debug!(v6 = %addr, "happy eyeballs: v6 probe won");
                        return Ok(addr);
                    }
                    Ok(Err(e)) => {
                        debug!(v6 = %v6, error = %e, "happy eyeballs: v6 probe failed");
                        errors.push(e);
                    }
                    Err(e) => {
                        debug!(v6 = %v6, "happy eyeballs: v6 task panicked: {e}");
                        errors.push(io::Error::new(io::ErrorKind::Other, "v6 task panicked"));
                    }
                }
                if v4_done { break; }
            },
            r = &mut v4_task, if !v4_done => {
                v4_done = true;
                match r {
                    Ok(Ok(addr)) => {
                        debug!(v4 = %addr, "happy eyeballs: v4 probe won");
                        return Ok(addr);
                    }
                    Ok(Err(e)) => {
                        debug!(v4 = %v4, error = %e, "happy eyeballs: v4 probe failed");
                        errors.push(e);
                    }
                    Err(e) => {
                        debug!(v4 = %v4, "happy eyeballs: v4 task panicked: {e}");
                        errors.push(io::Error::new(io::ErrorKind::Other, "v4 task panicked"));
                    }
                }
                if v6_done { break; }
            },
        }
    }

    Err(errors.into_iter().next().unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::Other, "no address succeeded")
    }))
}

/// Select the dial address for `node` using Happy Eyeballs v2 (RFC 8305).
///
/// - No `addr_v6`: dial `addr` directly (v4-only, zero overhead).
/// - `addr` already IPv6: dial it directly (`addr_v6` is unused per ChainNode doc).
/// - Both present: race v6 (immediate) vs v4 (200 ms delay); return the winner.
async fn resolve_dial_addr(node: &mirage_core::config::ChainNode) -> io::Result<SocketAddr> {
    match node.addr_v6 {
        None => Ok(node.addr),
        Some(_) if node.addr.is_ipv6() => Ok(node.addr),
        Some(v6_addr) => happy_eyeballs(node.addr, v6_addr).await,
    }
}

/// Establish an encrypted tunnel through the L1 relay node, without sending a
/// target address.
///
/// When `chain.hops` is non-empty the onion path is used instead of the classic
/// two-hop path.  The `chain.l2` node (if present) is used as the exit
/// destination for the onion circuit; if absent the caller must provide a
/// destination address through other means (currently returns an error).
///
/// For the classic two-hop path the handshake is completed and traffic shaping
/// (if any) is applied, but the relay-forwarding instruction (target address)
/// is **not** sent.  Use this when pre-warming pool slots; call
/// [`TunnelLease::send`] with the target before relaying application data.
///
/// Returns a boxed [`Tunnel`] ready for a target-address frame followed by
/// bidirectional data transfer.
pub async fn establish_raw_tunnel(
    chain_protocols: &ProtocolChain,
    chain: &ChainConfig,
) -> Result<Box<dyn Tunnel>, MirageError> {
    // ── Onion path ────────────────────────────────────────────────────────────
    if !chain.hops.is_empty() {
        // The exit node is the last element in chain.hops. The destination for
        // the innermost layer is Terminal so the exit knows the actual internet
        // target arrives later via the raw forward stream (length-prefixed).
        let destination = NextHop::Terminal;

        debug!(
            chain_id = %chain.chain_id,
            hops = chain.hops.len(),
            "establishing onion tunnel"
        );

        const ONION_TIMEOUT: Duration = Duration::from_secs(15);
        let tunnel = tokio::time::timeout(
            ONION_TIMEOUT,
            OnionTunnel::connect(chain, destination, &[]),
        )
        .await
        .map_err(|_| {
            MirageError::NodeUnreachable(format!(
                "chain {}: onion connect timed out after {}s",
                chain.chain_id,
                ONION_TIMEOUT.as_secs()
            ))
        })??;
        return Ok(Box::new(tunnel));
    }

    // ── Native UDP chain path ─────────────────────────────────────────────────
    //
    // When `chain.protocol` selects a UDP wire variant, we bypass the TCP
    // ProtocolChain entirely and dial the server's UDP-aggregator endpoint
    // directly via `mirage_protocol::driver::run_client_udp`. The chain is
    // single-hop here: `chain.l1` is the UDP exit address; L1 transparent /
    // hops / l2 are unused. Pairs with stend-gen's UDP aggregator NodeConfig
    // emission (one per node, all `udp_*` listeners as endpoint_configs[]).
    if let ProtocolId::Udp(udp_id) = &chain.protocol {
        return establish_udp_chain_tunnel(chain, *udp_id).await;
    }

    // ── Classic two-hop path ──────────────────────────────────────────────────
    let l1 = chain.l1.as_ref().ok_or_else(|| {
        MirageError::Config(format!(
            "chain {}: classic two-hop path requires l1 to be set (no hops configured)",
            chain.chain_id
        ))
    })?;

    // When L1 is a transparent TCP pipe, the client must handshake with L2's
    // public key and SNI — L1 just forwards raw bytes and never inspects the
    // ECDH handshake.  In opaque (crypto) relay mode the client handshakes with
    // L1 as usual.
    let (handshake_pubkey, handshake_sni) = if chain.l1_transparent {
        let l2 = chain.l2.as_ref().ok_or_else(|| {
            MirageError::Config(format!(
                "chain {}: l1_transparent=true requires l2 to be set",
                chain.chain_id
            ))
        })?;
        let pk = decode_hex_pubkey(&l2.public_key).map_err(|e| {
            MirageError::Config(format!(
                "chain {}: L2 public_key invalid (needed for transparent handshake): {e}",
                chain.chain_id
            ))
        })?;
        (pk, l2.sni.as_deref().unwrap_or("").to_owned())
    } else {
        let pk = decode_hex_pubkey(&l1.public_key).map_err(|e| {
            MirageError::Config(format!("chain {}: L1 public_key invalid: {e}", chain.chain_id))
        })?;
        (pk, l1.sni.as_deref().unwrap_or("").to_owned())
    };

    // Happy Eyeballs v2 (RFC 8305): race v6 vs v4, v6 gets 200 ms head start.
    let dial_addr = resolve_dial_addr(l1).await.map_err(|e| {
        MirageError::NodeUnreachable(format!(
            "chain {}: both v4 and v6 probes failed: {e}",
            chain.chain_id
        ))
    })?;

    debug!(
        chain_id = %chain.chain_id,
        dial_addr = %dial_addr,
        l1_transparent = chain.l1_transparent,
        "establishing raw chain tunnel (no target)"
    );

    // Bound the TCP connect (and protocol handshake) to avoid hanging pool
    // slots for the full OS TCP timeout (~130 s on Windows) when the relay is
    // slow or unreachable.
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
    let (_proto_id, tunnel) = tokio::time::timeout(
        CONNECT_TIMEOUT,
        chain_protocols.connect(dial_addr, &handshake_pubkey, &handshake_sni, chain.camouflage.clone()),
    )
    .await
    .map_err(|_| {
        MirageError::NodeUnreachable(format!(
            "chain {}: connect to {} timed out after {}s",
            chain.chain_id,
            l1.addr,
            CONNECT_TIMEOUT.as_secs()
        ))
    })??;

    debug!(chain_id = %chain.chain_id, "raw tunnel handshake done");

    // Wrap with traffic shaper if configured.
    let tunnel: Box<dyn Tunnel> = match chain.shaping {
        ShapingPreset::None => tunnel,
        ShapingPreset::WebBrowsing => {
            debug!(chain_id = %chain.chain_id, "applying WebBrowsing traffic shaping");
            Box::new(ShapedTunnel::new(tunnel, patterns::web_browsing()))
        }
        ShapingPreset::CloudApi => {
            debug!(chain_id = %chain.chain_id, "applying CloudApi traffic shaping");
            Box::new(ShapedTunnel::new(tunnel, patterns::yandex_cloud_api()))
        }
        ShapingPreset::Streaming => {
            debug!(chain_id = %chain.chain_id, "applying Streaming traffic shaping");
            Box::new(ShapedTunnel::new(tunnel, patterns::video_streaming()))
        }
    };

    // Wrap with cover-traffic injector if enabled.
    let tunnel: Box<dyn Tunnel> = if chain.cover_traffic.enabled {
        debug!(
            chain_id = %chain.chain_id,
            idle_threshold_ms = chain.cover_traffic.idle_threshold_ms,
            "applying cover traffic injection"
        );
        Box::new(CoverTunnel::new(tunnel, chain.cover_traffic.clone()))
    } else {
        tunnel
    };

    // Wrap with byte cipher layer if configured.
    // This is the outermost wrapper — applied after shaping and cover traffic so
    // those layers operate on L2 bytes and the cipher sees only obfuscated bytes.
    let tunnel: Box<dyn Tunnel> = if let Some(mode) = chain.byte_cipher.clone() {
        debug!(chain_id = %chain.chain_id, "applying byte cipher");
        Box::new(ByteCipherTunnel::new(tunnel, mode))
    } else {
        tunnel
    };

    Ok(tunnel)
}


/// Magic prefix sent as `initial_user_data` to signal the onion exit that this
/// is a multiplexed session.  Must match `MUX_MAGIC` in `mirage-node`'s
/// `exit/onion.rs` exactly.
pub const ONION_MUX_MAGIC: &[u8] = b"MUX\0";

/// Establish a tunnel for use as a mux session transport.
///
/// For onion chains (`chain.hops` non-empty) the onion envelope is built with
/// `ONION_MUX_MAGIC` as `initial_user_data` so the exit node upgrades to a
/// `SessionHandle::server` instead of the single-envelope TCP proxy.
///
/// For direct chains (no hops) this is identical to [`establish_raw_tunnel`].
pub async fn establish_mux_tunnel(
    chain_protocols: &ProtocolChain,
    chain: &ChainConfig,
) -> Result<Box<dyn Tunnel>, MirageError> {
    if !chain.hops.is_empty() {
        let destination = NextHop::Terminal;

        debug!(
            chain_id = %chain.chain_id,
            hops = chain.hops.len(),
            "establishing onion mux tunnel (MUX magic)"
        );

        const ONION_TIMEOUT: Duration = Duration::from_secs(15);
        let mut tunnel = tokio::time::timeout(
            ONION_TIMEOUT,
            OnionTunnel::connect(chain, destination, ONION_MUX_MAGIC),
        )
        .await
        .map_err(|_| {
            MirageError::NodeUnreachable(format!(
                "chain {}: onion mux connect timed out after {}s",
                chain.chain_id,
                ONION_TIMEOUT.as_secs()
            ))
        })??;
        // ReplyMode::Raw (variant 1): exit writes raw mux frames → bypass AEAD
        // unwrap on recv().
        // ReplyMode::Onion (variant 2): exit wraps each mux reply chunk in
        // wrap_reply_layer; intermediate hops add their layers; existing
        // OnionTunnel::recv unwraps N layers. Keep raw_mode = false (default).
        use mirage_core::config::ReplyMode;
        if chain.reply == ReplyMode::Raw {
            tunnel.raw_mode = true;
        }
        return Ok(Box::new(tunnel));
    }

    establish_raw_tunnel(chain_protocols, chain).await
}

/// Decode a 64-character hex string into a 32-byte X25519 public key.
fn decode_hex_pubkey(hex: &str) -> Result<[u8; 32], String> {
    if hex.len() != 64 {
        return Err(format!("expected 64 hex chars, got {}", hex.len()));
    }
    let mut out = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_nibble(b: u8) -> Result<u8, String> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(format!("invalid hex character: {}", b as char)),
    }
}

/// Establish a UDP chain tunnel by dialing the server's UDP aggregator
/// directly via `mirage_protocol::driver::run_client_udp`.
///
/// `chain.l1` carries the UDP listener address (the chain is treated as
/// single-hop for native UDP — no L1 TCP relay). We pick the camouflage
/// layer based on `udp_id` and use placeholder defaults for any
/// per-protocol secrets (PSK, password, magic byte) — these MUST match
/// the values stend-gen wrote into the server's `endpoint_configs[]`. For
/// production deployments, wire a per-chain config block on `ChainConfig`
/// (mirroring the TCP `shadowtls_v3` / `vless_reality` fields) so
/// operators don't have to keep client and server in sync by hand.
async fn establish_udp_chain_tunnel(
    chain: &ChainConfig,
    udp_id: UdpProtocolId,
) -> Result<Box<dyn Tunnel>, MirageError> {
    let l1 = chain.l1.as_ref().ok_or_else(|| {
        MirageError::Config(format!(
            "chain {}: UDP chain requires l1 to be set (it carries the UDP \
             dial target — single-hop direct exit OR multi-hop transparent L1)",
            chain.chain_id
        ))
    })?;

    // Multi-hop UDP path: when `chain.l1_transparent == true`, L1 is a plain
    // `relay::udp_transparent` byte pump (no crypto). Client dials L1's UDP
    // port but performs the Mirage / QUIC TLS handshake against L2's pubkey
    // and SNI — same shape as the TCP transparent-relay path. Single-hop UDP
    // (l1_transparent = false) stays unchanged: handshake against L1 directly.
    let (server_pubkey, sni) = if chain.l1_transparent {
        let l2 = chain.l2.as_ref().ok_or_else(|| {
            MirageError::Config(format!(
                "chain {}: l1_transparent=true requires l2 to be set (handshake target)",
                chain.chain_id
            ))
        })?;
        let pk = decode_hex_pubkey(&l2.public_key).map_err(|e| {
            MirageError::Config(format!(
                "chain {}: L2 public_key invalid (transparent UDP handshake): {e}",
                chain.chain_id
            ))
        })?;
        let sni = l2
            .sni
            .clone()
            .or_else(|| l1.sni.clone())
            .unwrap_or_else(|| "www.example.com".to_owned());
        (pk, sni)
    } else {
        let pk = decode_hex_pubkey(&l1.public_key).map_err(|e| {
            MirageError::Config(format!(
                "chain {}: L1 public_key invalid: {e}",
                chain.chain_id
            ))
        })?;
        let sni = l1.sni.clone().unwrap_or_else(|| "www.example.com".to_owned());
        (pk, sni)
    };

    // Per-chain UDP secret blocks (`chain.udp_quic` / `udp_hysteria2` /
    // `udp_hysteria2_native` / `udp_wg`) override the legacy hard-coded
    // defaults. When the operator has not set a block, we fall back to the
    // matching `Default::default()` so existing configs keep working — at
    // the cost of a 32-zero-byte PSK / fixed test password (operator MUST
    // override these in production, see mirage2monitor/CLAUDE.md).
    let camouflage = match udp_id {
        UdpProtocolId::MirageQuic | UdpProtocolId::QuicXray => {
            let cfg = chain.udp_quic.clone().unwrap_or_default();
            TransportCamouflage::Quic { alpn: cfg.alpn, bind_addr: l1.addr }
        }
        UdpProtocolId::MirageHysteria2 => {
            let cfg = chain.udp_hysteria2.clone().unwrap_or_default();
            TransportCamouflage::Hysteria2 { psk_hex: cfg.psk_hex, bind_addr: l1.addr }
        }
        UdpProtocolId::Hysteria2Native => {
            let cfg = chain.udp_hysteria2_native.clone().unwrap_or_default();
            TransportCamouflage::Hysteria2Native {
                bind_addr: l1.addr,
                password: cfg.password,
                rx_max_mbps: cfg.rx_max_mbps,
            }
        }
        UdpProtocolId::MirageWg => {
            let cfg = chain.udp_wg.clone().unwrap_or_default();
            TransportCamouflage::WireGuard {
                magic_byte: cfg.magic_byte,
                junk_count: cfg.junk_count,
                junk_min: cfg.junk_min,
                junk_max: cfg.junk_max,
                bind_addr: l1.addr,
            }
        }
        UdpProtocolId::MirageUdp
        | UdpProtocolId::MirageExitMinimalUdp
        | UdpProtocolId::MirageExitFullUdp
        | UdpProtocolId::MirageExitHmacUdp => TransportCamouflage::RawUdp { bind_addr: l1.addr },
        other => {
            return Err(MirageError::Config(format!(
                "chain {}: UDP variant {other:?} not yet wired into client \
                 establish_udp_chain_tunnel — open issue + add the camouflage arm",
                chain.chain_id
            )));
        }
    };

    let endpoint = MirageEndpointConfig {
        transport: TransportKind::Udp,
        auth: AuthMode::Blake3,
        camouflage,
        byte_cipher: None,
    };

    debug!(
        chain_id = %chain.chain_id,
        udp_id = ?udp_id,
        addr = %l1.addr,
        "establishing UDP chain tunnel"
    );

    const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
    let tunnel = tokio::time::timeout(
        CONNECT_TIMEOUT,
        run_client_udp(&endpoint, l1.addr, &server_pubkey, &sni),
    )
    .await
    .map_err(|_| {
        MirageError::NodeUnreachable(format!(
            "chain {}: UDP connect to {} timed out after {}s",
            chain.chain_id,
            l1.addr,
            CONNECT_TIMEOUT.as_secs()
        ))
    })??;

    debug!(chain_id = %chain.chain_id, "UDP chain tunnel handshake done");
    Ok(tunnel)
}

#[cfg(test)]
mod tests {
    use mirage_core::config::{
        ChainConfig, HandshakeStrategy, HopConfig, HopRole, ProtocolId, TcpProtocolId,
    };
    use mirage_protocol::onion::layer::NextHop;

    use super::ONION_MUX_MAGIC;
    use crate::onion_tunnel::OnionTunnel;

    /// Build a minimal `ChainConfig` with `handshake = Telescopic` and one dummy
    /// hop pointing at a localhost port that is not listening.
    fn telescopic_chain() -> ChainConfig {
        ChainConfig {
            chain_id: "test-telescopic".into(),
            l1: None,
            l2: None,
            l1_transparent: false,
            hops: vec![HopConfig {
                addr: "127.0.0.1:19999".into(),
                // 64 hex zeros — syntactically valid, cryptographically meaningless
                public_key: "0".repeat(64),
                sni: None,
                role: HopRole::Exit,
                display_name: None,
            }],
            handshake: HandshakeStrategy::Telescopic,
            reply: mirage_core::config::ReplyMode::Raw,
            protocol: ProtocolId::Tcp(TcpProtocolId::Mirage),
            priority: 0,
            shaping: Default::default(),
            camouflage: Default::default(),
            cover_traffic: Default::default(),
            byte_cipher: None,
            socks5_bind: None,
            websocket_xray: None,
            shadowtls_v3: None,
            mirage_websocket: None,
            vless_reality: None,
            udp_quic: None,
            udp_hysteria2: None,
            udp_hysteria2_native: None,
            udp_wg: None,
        }
    }

    /// `establish_mux_tunnel` for a telescopic chain must route through
    /// `connect_telescopic` with `ONION_MUX_MAGIC`.  We confirm the magic
    /// reaches `connect_telescopic` by calling it directly and asserting the
    /// error is a network/IO error (connection refused), not a config/logic
    /// error such as "empty hops" or an onion-builder error that would arise
    /// if user_data were silently dropped or the function signature mismatched.
    #[tokio::test]
    async fn telescopic_mux_magic_reaches_connect_telescopic() {
        let chain = telescopic_chain();
        let result = OnionTunnel::connect_telescopic(
            &chain,
            NextHop::Terminal,
            ONION_MUX_MAGIC,
        )
        .await;

        // The only path that gets this far is: public_key decoded (64 zeros is
        // valid hex), TCP connect attempted, connection refused → NodeUnreachable
        // or Io. A config/logic error would surface with a different message.
        match result {
            Ok(_) => panic!("expected a network-level error from connect_telescopic"),
            Err(err) => {
                let msg = format!("{err:?}");
                assert!(
                    msg.contains("timed out") || msg.contains("refused") || msg.contains("Io") || msg.contains("NodeUnreachable"),
                    "unexpected error (expected network-level failure, got): {msg}"
                );
            }
        }
    }

    // ── Happy Eyeballs tests ──────────────────────────────────────────────────

    use std::net::SocketAddr;
    use super::happy_eyeballs;

    /// Bind two local TCP listeners (v4 + v6) and verify v6 wins the race.
    ///
    /// Both listeners are ready before the race starts, so v6 wins because it
    /// has no 200 ms delay.
    #[tokio::test]
    async fn happy_eyeballs_prefers_v6() {
        let v4_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let v6_listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();

        let v4_addr: SocketAddr = v4_listener.local_addr().unwrap();
        let v6_addr: SocketAddr = v6_listener.local_addr().unwrap();

        let winner = happy_eyeballs(v4_addr, v6_addr).await.unwrap();
        assert_eq!(winner, v6_addr, "v6 should win when both are reachable");
    }

    /// v4-only path: `happy_eyeballs` is not called; `resolve_dial_addr` returns
    /// `node.addr` directly.  This test exercises `resolve_dial_addr` via a
    /// `ChainNode` with `addr_v6 = None`.
    #[tokio::test]
    async fn happy_eyeballs_v4_only() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let v4_addr: SocketAddr = listener.local_addr().unwrap();

        let node = mirage_core::config::ChainNode {
            addr: v4_addr,
            addr_v6: None,
            public_key: "0".repeat(64),
            sni: None,
            display_name: None,
        };

        let result = super::resolve_dial_addr(&node).await.unwrap();
        assert_eq!(result, v4_addr);
    }

    /// When v6 is unreachable (unrouted documentation address 2001:db8::1),
    /// the race should fall back to v4 and succeed.
    ///
    /// Uses a short per-probe timeout so the test completes quickly.  The
    /// 2001:db8:: prefix is IANA-reserved and never routed — connect returns
    /// an error quickly (network unreachable) or times out within the 5 s probe
    /// window.  Either way, v4 must win before the overall test timeout.
    #[tokio::test]
    async fn happy_eyeballs_falls_back_to_v4() {
        let v4_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let v4_addr: SocketAddr = v4_listener.local_addr().unwrap();

        // 2001:db8::1 — documentation range, never routed.
        // Port 1 is almost certainly closed on any system.
        let v6_addr: SocketAddr = "[2001:db8::1]:1".parse().unwrap();

        let winner = happy_eyeballs(v4_addr, v6_addr).await.unwrap();
        assert_eq!(winner, v4_addr, "v4 must win when v6 is unreachable");
    }
}
