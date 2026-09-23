//! [`UdpEntranceHandle`] — dial one UDP-shaped [`ArtefactEntrance`] with the
//! subscriber's persistent static key, the same identity every TCP entrance
//! dials with ([`crate::dial::StaticKeyProtocol`]).
//!
//! ## Why this is not `TunnelPool::datagram_dial()`
//!
//! `mirage-client-core`'s `TunnelPool::is_datagram_chain()` /
//! `datagram_dial()` resolve a datagram-native chain's entry face through
//! the live `peer_table` (`chain_face_udp`, looked up by `node_id`) and,
//! underneath, `mirage_control_plane::tunnel::establish_udp_datagram_tunnel`
//! mints a FRESH EPHEMERAL client static key on every dial — see that
//! function's own doc, "Generate a per-dial ephemeral client static key for
//! V9 (Option A: unlinkable). Never stored". Neither shape fits this crate:
//! `crate::assembly` keeps `peer_table` permanently empty (dependency
//! boundary, see that module's own doc) and a subscriber MUST present the
//! SAME static key on every dial — the box's `SubscriberKeyStore::contains`
//! checks the dialer's key against a fixed, opbox-issued list, exactly the
//! reason [`crate::dial::StaticKeyProtocol`] exists on the TCP side.
//!
//! The fix mirrors `StaticKeyProtocol`'s own precedent exactly: read past
//! the ephemeral-key-minting wrapper to the lower-level driver it calls.
//! `establish_udp_datagram_tunnel`'s single-hop body is a thin shell around
//! `mirage_control_plane::tunnel::establish_native_udp_tunnel_addressed`,
//! which is ALREADY a fully public function taking `client_static_priv: &
//! [u8; 32]` as a plain caller-supplied parameter — no `ChainConfig`, no
//! `PeerTable`, nothing this crate is forbidden to hold. This module calls
//! that function directly, re-exported unchanged via `mirage_client_core::
//! tunnel` (`mirage-client-core`'s `lib.rs` already does `pub use
//! mirage_control_plane::tunnel;`) — no new crate dependency, no
//! `mirage2web` edit, no re-implemented crypto.
//!
//! ## Camouflage
//!
//! [`TransportCamouflage::is_udp`] recognises seven datagram-shaped variants
//! (`Quic`, `Http3`, `Hysteria2`, `WireGuard`, `RawUdp`, `WebTransport`,
//! `DnsTunnel`) — `crate::chain_build::build_chain_config` uses exactly that
//! test to route an entrance to a UDP-protocol `ChainConfig`. Only
//! [`TransportCamouflage::RawUdp`] is actually dialable here:
//! `establish_native_udp_tunnel_addressed` speaks bare V9-over-UDP on the
//! socket and never strips an outer wrapper of any kind — the same
//! restriction `mirage2web`'s own `resolve_entry_face_udp` enforces for the
//! mesh path (see its own doc, "the only shape this can dial"). A
//! `Quic`/`Http3`/`Hysteria2`/`WireGuard`/`WebTransport`/`DnsTunnel`
//! entrance is recognised as UDP-shaped by [`UdpEntranceHandle::try_new`]
//! but refused BY NAME at dial time rather than silently sent unwrapped —
//! see [`UdpEntranceHandle::dial`] and [`UdpEntranceHandle::is_dialable`].

use std::net::SocketAddr;

use async_trait::async_trait;
use mirage_client_core::socks5::DatagramEntranceDialer;
use mirage_client_core::tunnel::{establish_native_udp_tunnel_addressed, DatagramTunnel};
use mirage_core::config::TransportCamouflage;
use mirage_core::crypto::AeadSuite;
use mirage_core::error::MirageError;
use thiserror::Error;

use crate::artefact::{ArtefactEntrance, ArtefactError};

/// AEAD suite preference order for a UDP entrance dial — identical to
/// `establish_udp_datagram_tunnel`'s own choice
/// (`mirage2web/crates/mirage-dial-core/src/tunnel.rs`).
const SUITE_PREFS: &[AeadSuite] = &[AeadSuite::ChaCha20Poly1305, AeadSuite::Aes256Gcm];

/// Bounded wait for the handshake to complete — mirrors
/// `establish_udp_datagram_tunnel`'s own `CONNECT_TIMEOUT`.
const UDP_DIAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Every way [`UdpEntranceHandle::try_new`] / [`UdpEntranceHandle::dial`] refuse.
#[derive(Debug, Error)]
pub enum UdpDialError {
    #[error(transparent)]
    Artefact(#[from] ArtefactError),

    #[error("udp entrance '{chain_id}': addr {addr:?} is not a resolvable host:port: {reason}")]
    BadAddr { chain_id: String, addr: String, reason: String },

    #[error(
        "udp entrance '{chain_id}': camouflage {label} is UDP-shaped but this client's UDP \
         dial only speaks bare V9-over-UDP (RawUdp) — {label} needs an outer wrapper this \
         dial path does not implement, refusing rather than sending unwrapped bytes"
    )]
    UnsupportedCamouflage { chain_id: String, label: &'static str },

    #[error("udp entrance '{chain_id}': dial to {addr} timed out after {timeout_secs}s")]
    Timeout { chain_id: String, addr: SocketAddr, timeout_secs: u64 },

    #[error(transparent)]
    Mirage(#[from] MirageError),
}

/// One UDP-shaped entrance, ready to dial with the subscriber's persistent
/// identity. See the module doc for why this exists instead of
/// `TunnelPool::datagram_dial()`.
#[derive(Debug)]
pub struct UdpEntranceHandle {
    /// Matches `crate::chain_build::entrance_chain_id` for the same index —
    /// log correlation only, never looked up by this value.
    pub chain_id: String,
    dial_addr: SocketAddr,
    server_pubkey: [u8; 32],
    client_static_priv: [u8; 32],
    mesh_id: Vec<u8>,
    camouflage: TransportCamouflage,
}

impl UdpEntranceHandle {
    /// Build a handle for `entrance`. Does not dial — see [`Self::dial`].
    pub fn try_new(
        chain_id: String,
        entrance: &ArtefactEntrance,
        mesh_id: Vec<u8>,
        client_static_priv: [u8; 32],
    ) -> Result<Self, UdpDialError> {
        let dial_addr = entrance.addr.parse::<SocketAddr>().map_err(|e| UdpDialError::BadAddr {
            chain_id: chain_id.clone(),
            addr: entrance.addr.clone(),
            reason: e.to_string(),
        })?;
        let server_pubkey = entrance.server_pubkey()?;
        Ok(Self {
            chain_id,
            dial_addr,
            server_pubkey,
            client_static_priv,
            mesh_id,
            camouflage: entrance.camouflage.clone(),
        })
    }

    /// This handle's camouflage — exposed so a caller can decide whether to
    /// even attempt [`Self::dial`] (e.g. picking the first dialable
    /// entrance out of several UDP-shaped ones) without eating the refusal.
    pub fn camouflage(&self) -> &TransportCamouflage {
        &self.camouflage
    }

    /// `true` only for [`TransportCamouflage::RawUdp`] — see the module doc
    /// for why every other UDP-shaped variant is refused at dial time.
    pub fn is_dialable(&self) -> bool {
        matches!(self.camouflage, TransportCamouflage::RawUdp { .. })
    }

    /// Dial a fresh datagram-native tunnel through this entrance, presenting
    /// the subscriber's persistent static key and mesh_id — see the module
    /// doc. One dial per call; nothing here pools or reuses tunnels, the
    /// same "one dial per destination session" contract
    /// `TunnelPool::datagram_dial()` documents for its own equivalent.
    pub async fn dial(&self) -> Result<DatagramTunnel, UdpDialError> {
        if !self.is_dialable() {
            return Err(UdpDialError::UnsupportedCamouflage {
                chain_id: self.chain_id.clone(),
                label: self.camouflage.label(),
            });
        }
        let tunnel = tokio::time::timeout(
            UDP_DIAL_TIMEOUT,
            establish_native_udp_tunnel_addressed(
                self.dial_addr,
                &self.server_pubkey,
                &self.client_static_priv,
                &self.mesh_id,
                SUITE_PREFS,
                None,
                None,
            ),
        )
        .await
        .map_err(|_| UdpDialError::Timeout {
            chain_id: self.chain_id.clone(),
            addr: self.dial_addr,
            timeout_secs: UDP_DIAL_TIMEOUT.as_secs(),
        })??;
        Ok(DatagramTunnel::Direct(tunnel))
    }
}

/// Wires [`UdpEntranceHandle`] directly into `mirage-client-core`'s
/// `Socks5Server` UDP ASSOCIATE support (`UdpAssociateSupport::
/// DatagramEntrance`) — see `crate::assembly` for where an `Arc<Self>` is
/// handed to the server as `Arc<dyn DatagramEntranceDialer>`.
#[async_trait]
impl DatagramEntranceDialer for UdpEntranceHandle {
    async fn dial_datagram(&self) -> Result<DatagramTunnel, MirageError> {
        self.dial().await.map_err(|e| MirageError::Protocol(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mirage_protocol::auth::v9::CLIENT_HELLO_LEN;
    use mirage_protocol::core::{run_server_handshake_v9_datagram, MirageCoreUdpTunnel, ServerHelloCache};
    use mirage_protocol::traits::{Tunnel, TransportInfo};
    use mirage_protocol::transport::udp::UdpPeerSocket;
    use tokio::net::UdpSocket;

    fn sample_entrance(addr: &str, camouflage: TransportCamouflage, pubkey_hex: String) -> ArtefactEntrance {
        ArtefactEntrance { addr: addr.to_owned(), server_pubkey_hex: pubkey_hex, camouflage }
    }

    // ── try_new ──────────────────────────────────────────────────────────

    #[test]
    fn try_new_parses_addr_and_server_pubkey() {
        let e = sample_entrance(
            "172.86.76.34:853",
            TransportCamouflage::RawUdp { bind_addr: "0.0.0.0:853".parse().expect("addr") },
            "22".repeat(32),
        );
        let handle =
            UdpEntranceHandle::try_new("entrance-1".to_owned(), &e, b"MESH2".to_vec(), [0x11u8; 32])
                .expect("valid entrance must build a handle");
        assert_eq!(handle.chain_id, "entrance-1");
        assert!(handle.is_dialable());
    }

    #[test]
    fn try_new_refuses_unparseable_addr() {
        let e = sample_entrance("not-an-addr", TransportCamouflage::RawUdp { bind_addr: "0.0.0.0:0".parse().expect("addr") }, "22".repeat(32));
        match UdpEntranceHandle::try_new("entrance-0".to_owned(), &e, b"MESH2".to_vec(), [0x11u8; 32]) {
            Err(UdpDialError::BadAddr { chain_id, .. }) => assert_eq!(chain_id, "entrance-0"),
            other => panic!("expected BadAddr, got {other:?}"),
        }
    }

    // ── is_dialable / dial refusal for non-RawUdp camouflage ───────────────

    #[test]
    fn is_dialable_is_true_only_for_raw_udp() {
        let raw = sample_entrance(
            "203.0.113.1:9000",
            TransportCamouflage::RawUdp { bind_addr: "0.0.0.0:9000".parse().expect("addr") },
            "22".repeat(32),
        );
        let quic = sample_entrance(
            "203.0.113.1:9001",
            TransportCamouflage::Quic { alpn: vec!["h3".into()], bind_addr: "0.0.0.0:9001".parse().expect("addr") },
            "22".repeat(32),
        );
        let raw_handle = UdpEntranceHandle::try_new("e-raw".into(), &raw, b"MESH2".to_vec(), [0x11u8; 32]).expect("build");
        let quic_handle = UdpEntranceHandle::try_new("e-quic".into(), &quic, b"MESH2".to_vec(), [0x11u8; 32]).expect("build");
        assert!(raw_handle.is_dialable());
        assert!(!quic_handle.is_dialable());
    }

    #[tokio::test]
    async fn dial_refuses_non_raw_udp_camouflage_by_name_without_any_network_io() {
        let quic = sample_entrance(
            "203.0.113.1:9001",
            TransportCamouflage::Quic { alpn: vec!["h3".into()], bind_addr: "0.0.0.0:9001".parse().expect("addr") },
            "22".repeat(32),
        );
        let handle = UdpEntranceHandle::try_new("e-quic".into(), &quic, b"MESH2".to_vec(), [0x11u8; 32]).expect("build");
        match handle.dial().await {
            Err(UdpDialError::UnsupportedCamouflage { chain_id, label }) => {
                assert_eq!(chain_id, "e-quic");
                assert_eq!(label, "quic");
            }
            Err(other) => panic!("expected UnsupportedCamouflage, got a different error: {other}"),
            Ok(_) => panic!("expected a refusal, dial succeeded"),
        }
    }

    // ── datagram round-trip against a local, hand-built V9 UDP server peer ─
    //
    // Same pattern `mirage2web/crates/mirage-dial-core/src/tunnel.rs` and
    // `mirage-client-core/src/socks5.rs` already use for this exact
    // function's lower-level twin (`establish_native_udp_tunnel`): the
    // server side is played by hand with `run_server_handshake_v9_datagram`
    // on a fixed rendezvous socket, proving this crate's dial path
    // completes a live, keyed tunnel that actually carries data.
    #[tokio::test]
    async fn dial_completes_a_live_tunnel_and_round_trips_data() {
        use mirage_core::crypto::KeyPair;
        use std::fmt::Write;

        let server_kp = KeyPair::generate();
        let server_pubkey_hex =
            server_kp.public.to_bytes().iter().fold(String::with_capacity(64), |mut s, b| {
                write!(s, "{b:02x}").expect("writing to a String cannot fail");
                s
            });
        let client_static_priv = KeyPair::generate().secret.to_bytes();

        let fixed = UdpSocket::bind("127.0.0.1:0").await.expect("bind fixed rendezvous socket");
        let fixed_addr = fixed.local_addr().expect("fixed socket local addr");

        let entrance = sample_entrance(
            &fixed_addr.to_string(),
            TransportCamouflage::RawUdp { bind_addr: fixed_addr },
            server_pubkey_hex,
        );
        let handle = UdpEntranceHandle::try_new(
            "entrance-live".to_owned(),
            &entrance,
            b"test-mesh-udp-dial".to_vec(),
            client_static_priv,
        )
        .expect("build handle");

        let cache = ServerHelloCache::new();
        let client_fut = handle.dial();
        let server_fut = async {
            let mut raw = [0u8; CLIENT_HELLO_LEN];
            let (n, client_addr) =
                fixed.recv_from(&mut raw).await.expect("recv ClientHello on the fixed socket");
            assert_eq!(n, CLIENT_HELLO_LEN, "ClientHello datagram must be exactly CLIENT_HELLO_LEN bytes");

            let mut server_socket = UdpPeerSocket::bind_server_ephemeral(client_addr)
                .await
                .expect("bind server per-peer ephemeral socket");
            let result = run_server_handshake_v9_datagram(
                &mut server_socket,
                &server_kp,
                b"test-mesh-udp-dial",
                SUITE_PREFS,
                |_ctx| true,
                &raw,
                &cache,
            )
            .await
            .expect("server handshake I/O must not error")
            .expect("server must accept the ClientHello");

            let info = TransportInfo::udp_raw(server_socket.remote_addr());
            MirageCoreUdpTunnel::new(
                server_socket,
                &result.session_keys,
                false,
                info,
                mirage_protocol::auth::AuthMode::Hmac,
            )
        };

        let (client_result, mut server_tunnel) = tokio::join!(client_fut, server_fut);
        let mut client_tunnel = client_result.expect("dial must complete against a live server peer");

        let ping = b"ping over UdpEntranceHandle::dial";
        client_tunnel.send(ping).await.expect("client send must succeed");
        let got = server_tunnel.recv_datagram().await.expect("server recv_datagram must succeed");
        assert_eq!(got, ping, "server must receive the exact client payload");

        let pong = b"pong back over the same tunnel";
        server_tunnel.send(pong).await.expect("server send must succeed");
        let got = client_tunnel.recv_datagram().await.expect("client recv_datagram must succeed");
        assert_eq!(got, pong, "client must receive the exact server payload");
    }
}
