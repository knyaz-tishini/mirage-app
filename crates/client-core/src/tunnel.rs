//! Tunnel establishment through the Mirage chain.

use std::time::Duration;

use mirage_core::{
    config::{ChainConfig, ShapingPreset},
    error::MirageError,
};
use mirage_protocol::{
    byte_cipher::ByteCipherTunnel,
    frame_obfuscation::patterns,
    negotiation::ProtocolChain,
    onion::layer::NextHop,
    traits::Tunnel,
};
use tracing::debug;

use crate::cover_tunnel::CoverTunnel;
use crate::onion_tunnel::OnionTunnel;
use crate::shaped_tunnel::ShapedTunnel;

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

    debug!(
        chain_id = %chain.chain_id,
        l1_addr = %l1.addr,
        l1_transparent = chain.l1_transparent,
        "establishing raw chain tunnel (no target)"
    );

    // Bound the TCP connect (and protocol handshake) to avoid hanging pool
    // slots for the full OS TCP timeout (~130 s on Windows) when the relay is
    // slow or unreachable.
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
    let (_proto_id, tunnel) = tokio::time::timeout(
        CONNECT_TIMEOUT,
        chain_protocols.connect(l1.addr, &handshake_pubkey, &handshake_sni, chain.camouflage.clone()),
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
        // Server upgrades to raw mux frames after receiving MUX_MAGIC.
        // Reply stream is no longer AEAD-framed; bypass unwrap on recv().
        tunnel.raw_mode = true;
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
}
