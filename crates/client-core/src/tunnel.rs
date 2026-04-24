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
