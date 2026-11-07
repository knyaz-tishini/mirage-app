//! UDP native-wire smoke test.
//!
//! Dials a single `udp_*`-mode listener on a stend exit (the kind that
//! `mirage-stend-gen`'s UDP aggregator emits), runs the Mirage handshake
//! over the UDP camouflage layer, sends a SOCKS-style `host:port` target
//! frame, and prints whatever bytes come back. Verifies the
//! `udp_aggregator` NodeConfig path on the server end-to-end without
//! pulling in SOCKS5/TunnelPool/cli wiring.
//!
//! Usage:
//!
//! ```text
//! mirage-udp-smoke \
//!     --proto mirage-quic \
//!     --addr 203.0.113.17:4801 \
//!     --pubkey 0d247d890a86703d... \
//!     --target ifconfig.me:80
//! ```

use std::net::SocketAddr;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use mirage_core::config::{
    AuthMode, MirageEndpointConfig, TransportCamouflage, TransportKind,
};
use mirage_protocol::driver::run_client_udp;
use mirage_protocol::traits::Tunnel;
use tokio::time::timeout;
use tracing::{info, warn};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum UdpProto {
    /// MirageQuic: bare QUIC with ALPN=h3 + Mirage handshake on top.
    MirageQuic,
    /// MirageHysteria2: Hysteria2 Salamander with PSK + Mirage handshake.
    MirageHysteria2,
    /// MirageWg: WireGuard-style camouflage + Mirage handshake.
    MirageWg,
    /// MirageUdp: bare UDP carrying length-framed Mirage messages.
    MirageUdp,
}

impl UdpProto {
    fn camouflage(self, bind_addr: SocketAddr) -> TransportCamouflage {
        // bind_addr here is the SERVER address — the layer's connect_client
        // uses it as the peer to dial. The struct field name stays
        // `bind_addr` because it's reused for accept_server too.
        match self {
            UdpProto::MirageQuic => TransportCamouflage::Quic {
                alpn: vec!["h3".to_owned()],
                bind_addr,
            },
            UdpProto::MirageHysteria2 => TransportCamouflage::Hysteria2 {
                psk_hex: "00".repeat(32),
                bind_addr,
            },
            UdpProto::MirageWg => TransportCamouflage::WireGuard {
                magic_byte: 0x04,
                junk_count: 1,
                junk_min: 16,
                junk_max: 64,
                bind_addr,
            },
            UdpProto::MirageUdp => TransportCamouflage::RawUdp { bind_addr },
        }
    }
}

#[derive(Parser, Debug)]
#[command(name = "mirage-udp-smoke")]
struct Cli {
    /// Which UDP wire protocol to use.
    #[arg(long, value_enum)]
    proto: UdpProto,

    /// Server's UDP listener address (e.g. `203.0.113.17:4801`).
    #[arg(long)]
    addr: SocketAddr,

    /// Server's hex-encoded X25519 public key (64 hex chars).
    #[arg(long)]
    pubkey: String,

    /// Target host:port the server should connect to as the exit (e.g. `ifconfig.me:80`).
    #[arg(long, default_value = "ifconfig.me:80")]
    target: String,

    /// Bytes to send after the target frame (an HTTP request, by default).
    #[arg(long, default_value = "GET /ip HTTP/1.1\r\nHost: ifconfig.me\r\nConnection: close\r\n\r\n")]
    payload: String,

    /// SNI hint for QUIC/Http3 layers (ignored by RawUdp / Hysteria2 / WG).
    #[arg(long, default_value = "www.example.com")]
    sni: String,

    /// Overall timeout in seconds.
    #[arg(long, default_value_t = 15)]
    timeout: u64,
}

fn decode_pubkey(s: &str) -> Result<[u8; 32], String> {
    if s.len() != 64 {
        return Err(format!("pubkey must be 64 hex chars, got {}", s.len()));
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
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
        _ => Err(format!("invalid hex char: {}", b as char)),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    let pubkey = decode_pubkey(&cli.pubkey)?;
    let camouflage = cli.proto.camouflage(cli.addr);
    let endpoint = MirageEndpointConfig {
        transport: TransportKind::Udp,
        auth: AuthMode::Blake3,
        camouflage,
        byte_cipher: None,
        byte_cipher_mode: None,
    };

    info!(?cli.proto, %cli.addr, "dialing server via UDP camouflage");

    let timeout_dur = Duration::from_secs(cli.timeout);
    let mut tunnel: Box<dyn Tunnel> = match timeout(
        timeout_dur,
        run_client_udp(&endpoint, cli.addr, &pubkey, &cli.sni, b"staging-v2"),
    )
    .await
    {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => {
            warn!("handshake error: {e}");
            return Err(e.into());
        }
        Err(_) => {
            warn!("handshake timed out after {}s", cli.timeout);
            return Err("handshake timeout".into());
        }
    };
    info!("handshake complete — sending target");

    // First frame: SOCKS-style "host:port" string (consumed by exit handler).
    tunnel.send(cli.target.as_bytes()).await?;
    info!(target = %cli.target, "target sent — sending payload");

    tunnel.send(cli.payload.as_bytes()).await?;
    info!(payload_len = cli.payload.len(), "payload sent — reading response");

    let mut total = 0usize;
    let mut buf = vec![0u8; 16384];
    let read_until = std::time::Instant::now() + timeout_dur;
    loop {
        let remaining = read_until
            .checked_duration_since(std::time::Instant::now())
            .unwrap_or_default();
        if remaining.is_zero() {
            break;
        }
        match timeout(remaining, tunnel.recv(&mut buf)).await {
            Ok(Ok(0)) => {
                info!("EOF after {total} bytes");
                break;
            }
            Ok(Ok(n)) => {
                total += n;
                let preview = String::from_utf8_lossy(&buf[..n.min(200)]);
                println!("[+{n} bytes] {preview}");
            }
            Ok(Err(e)) => {
                warn!("recv error: {e}");
                break;
            }
            Err(_) => {
                info!("idle timeout after {total} bytes");
                break;
            }
        }
    }
    println!("\n=== TOTAL: {total} bytes received ===");
    Ok(())
}
