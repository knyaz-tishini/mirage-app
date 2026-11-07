//! [`StaticKeyProtocol`] — a client-side `TcpProtocol` adapter that presents
//! a caller-supplied, PERSISTENT X25519 static key on every dial, instead of
//! the fresh ephemeral key `mirage_protocol::endpoint_protocol::
//! EndpointProtocol` generates internally on every call
//! (`mirage2web/crates/mirage-protocol/src/endpoint_protocol.rs:99-100,
//! 118-119` — `x25519_dalek::StaticSecret::random_from_rng(OsRng)`).
//!
//! The subscriber gate on a box (`SubscriberKeyStore::contains`, per
//! `docs/mirage/plans/delivery-artefact-and-issuance-2026-09-17.md`) checks
//! the dialer's static public key against a fixed, opbox-issued list — a
//! subscriber must therefore present the SAME key on every dial, which
//! `EndpointProtocol` structurally cannot do.
//!
//! **This required no change to `mirage2web`.** The plan
//! (`docs/mirage/plans/thin-customer-connector-2026-09-17.md` §2.1) proposed
//! adding a sibling `connect_with_static_key` method to `EndpointProtocol`
//! itself. Reading the actual call chain shows that is unnecessary: the
//! lower-level driver function `EndpointProtocol` wraps,
//! [`mirage_protocol::driver::run_client_tcp_v9`], is ALREADY a fully public
//! function that takes `client_static_priv: &[u8; 32]` as a plain
//! caller-supplied parameter (`driver.rs:488-495`) — `EndpointProtocol`
//! simply never forwards one through. This type calls that already-public
//! function directly, mirroring `EndpointProtocol::connect_with_credential`'s
//! own body (`endpoint_protocol.rs:107-135`) with exactly one substitution:
//! the key comes from `self`, decoded once from the artefact, never from
//! `OsRng`.
//!
//! **The dial is transport-aware.** The subscriber may be handed an entrance
//! wearing ANY outer camouflage — a stream one (raw TCP, TLS, WebSocket,
//! xhttp, Reality, SSH, gRPC) or a datagram one (raw UDP, QUIC, HTTP/3,
//! WebTransport, WireGuard, a DNS tunnel, a raw ICMP echo socket).
//! The camouflage decides the wire transport; it is not a decoration that can
//! be bolted onto a TCP socket. This adapter therefore calls
//! [`mirage_protocol::client_transport::run_client_v9_transport_aware`],
//! the one dispatcher that routes by the camouflage: a stream camouflage goes
//! to `run_client_tcp_v9` (the config is passed through UNCHANGED, so the
//! bytes on the wire stay byte-for-byte the ones the TCP-only client has
//! always produced) and a datagram camouflage goes to `run_client_udp_v9`.
//! The persistent key model above is untouched by this: BOTH paths take the
//! same `client_static_priv` this adapter holds.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use mirage_core::{
    config::{CamouflageMode, MirageEndpointConfig, TcpProtocolId, TransportCamouflage},
    error::MirageError,
};
use mirage_protocol::{
    client_transport::run_client_v9_transport_aware,
    driver::ClientCredential,
    plane::{PacketPlane, PlaneConnector},
    traits::{AcceptResult, TcpProtocol, Tunnel},
};
use tokio::net::TcpStream;

/// Client-side [`TcpProtocol`] adapter that dials with a fixed static key.
///
/// Never carries a baked-in credential — per
/// `thin-customer-connector-2026-09-17.md` §3 ("no credential-scheme
/// policy"), this crate never imports `mirage_core::crypto::credential` and
/// never constructs a [`ClientCredential`]; the `credential` parameter on
/// [`TcpProtocol::connect_with_credential`] is honoured if a caller ever
/// passes one, but nothing in this crate does.
pub struct StaticKeyProtocol {
    config: MirageEndpointConfig,
    mesh_id: Vec<u8>,
    client_static_priv: [u8; 32],
}

impl StaticKeyProtocol {
    /// `client_static_priv` is presented byte-for-byte on every dial made
    /// through this instance — see [`EntranceArtefact::client_static_priv`]
    /// (`crate::artefact`) for where it comes from.
    pub fn new(config: MirageEndpointConfig, mesh_id: Vec<u8>, client_static_priv: [u8; 32]) -> Self {
        Self { config, mesh_id, client_static_priv }
    }

    /// The exact bytes this instance presents as `client_static_priv` on
    /// every dial. Exposed so callers (and tests) can prove the identity is
    /// stable — never regenerated — across independently constructed
    /// instances built from the same artefact.
    pub fn dial_static_key(&self) -> [u8; 32] {
        self.client_static_priv
    }
}

#[async_trait]
impl TcpProtocol for StaticKeyProtocol {
    async fn connect(
        &self,
        addr: SocketAddr,
        server_pubkey: &[u8; 32],
        sni: &str,
        _camouflage: CamouflageMode,
    ) -> Result<Box<dyn Tunnel>, MirageError> {
        run_client_v9_transport_aware(
            &self.config,
            addr,
            server_pubkey,
            &self.client_static_priv,
            sni,
            &self.mesh_id,
            None,
        )
        .await
    }

    async fn connect_with_credential(
        &self,
        addr: SocketAddr,
        server_pubkey: &[u8; 32],
        sni: &str,
        hop_camouflage: Option<TransportCamouflage>,
        credential: Option<ClientCredential>,
    ) -> Result<Box<dyn Tunnel>, MirageError> {
        // Per-entrance camouflage override, mirroring
        // `EndpointProtocol::connect_with_credential`: an explicit
        // `hop_camouflage` (always `Some`, in this crate's own dial path —
        // see `crate::chain_build`) wins; `None` falls back to whatever the
        // adapter was constructed with.
        let effective_config;
        let config_ref = if let Some(camo) = hop_camouflage {
            effective_config = MirageEndpointConfig { camouflage: camo, ..self.config.clone() };
            &effective_config
        } else {
            &self.config
        };
        run_client_v9_transport_aware(
            config_ref,
            addr,
            server_pubkey,
            &self.client_static_priv,
            sni,
            &self.mesh_id,
            credential.as_ref(),
        )
        .await
    }

    async fn accept(&self, _stream: TcpStream, _peeked: &[u8]) -> Result<AcceptResult, MirageError> {
        Err(MirageError::Protocol(
            "StaticKeyProtocol::accept called on client-side adapter".into(),
        ))
    }

    fn id(&self) -> TcpProtocolId {
        TcpProtocolId::Mirage
    }

    fn probe_resistant(&self) -> bool {
        true
    }
}

// -- the mono-protocol QUIC transport --------------------------------------

/// The ALPN list an entrance publishes, when it wears the QUIC camouflage.
///
/// The camouflage decides the transport: a QUIC entrance is the
/// mono-protocol plane - ONE connection carries the control wire AND the
/// whole IP packets. Every other camouflage keeps the old mux transport.
pub fn quic_alpn(camouflage: &TransportCamouflage) -> Option<Vec<String>> {
    match camouflage {
        TransportCamouflage::Quic { alpn, .. } => Some(alpn.clone()),
        _ => None,
    }
}

/// The mono-protocol QUIC session of one entrance.
///
/// ONE QUIC connection carries both halves of the tunnel: the control wire
/// (the Mirage V9 handshake and the edge hello/welcome) and the packet plane
/// (whole IP packets in QUIC datagrams, RFC 9221). The packets do not
/// travel through the control wire - they travel through the same
/// connection.
///
/// The connector owns the QUIC endpoint, and dropping the endpoint closes the
/// connection: the connector must outlive the session it opened.
pub struct PlaneDial {
    connector: PlaneConnector,
    tunnel: Box<dyn Tunnel>,
    plane: Arc<dyn PacketPlane>,
}

impl PlaneDial {
    /// Open the plane of one entrance.
    ///
    /// `alpn` is the ALPN list the face publishes. The server name is empty:
    /// the face is reached by its address, not by a name.
    pub async fn open(
        alpn: &[String],
        addr: SocketAddr,
        server_pubkey: &[u8; 32],
        client_static_priv: &[u8; 32],
        mesh_id: &[u8],
    ) -> Result<Self, MirageError> {
        let connector = PlaneConnector::new(alpn, "")?;
        let session = connector.connect(addr, server_pubkey, client_static_priv, mesh_id).await?;
        Ok(Self { connector, tunnel: session.tunnel, plane: session.plane })
    }

    /// The control wire of the connection - the hello and the welcome ride
    /// it.
    pub fn tunnel(&mut self) -> &mut Box<dyn Tunnel> {
        &mut self.tunnel
    }

    /// The packet plane of the connection - the whole IP packets.
    pub fn plane(&self) -> Arc<dyn PacketPlane> {
        Arc::clone(&self.plane)
    }

    /// The three pieces of the session: the control wire, the packet plane,
    /// and the connector that keeps the connection open.
    pub fn into_parts(self) -> (Box<dyn Tunnel>, Arc<dyn PacketPlane>, PlaneConnector) {
        (self.tunnel, self.plane, self.connector)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> MirageEndpointConfig {
        MirageEndpointConfig {
            transport: mirage_core::config::TransportKind::Tcp,
            auth: mirage_core::config::AuthMode::Blake3,
            camouflage: TransportCamouflage::None,
            byte_cipher: None,
            byte_cipher_mode: None,
        }
    }

    // ── the same static key presented across two independently built dials ──
    //
    // `TunnelPool` reuses ONE `StaticKeyProtocol` instance across every
    // reconnect on its `chain_protocols: Arc<ProtocolChain>`, so "the same
    // key on every dial" is structurally guaranteed within one running pool.
    // This proves the stronger, more meaningful property this crate actually
    // depends on: two SEPARATE instances built from the same source key
    // (e.g. two entrance chains under the same artefact, or two application
    // restarts) present byte-identical key material — never independently
    // regenerated the way `EndpointProtocol::connect` regenerates one on
    // every single call.
    #[test]
    fn same_static_key_presented_across_two_independently_built_dials() {
        let key = [0x42u8; 32];
        let a = StaticKeyProtocol::new(sample_config(), b"MESH2".to_vec(), key);
        let b = StaticKeyProtocol::new(sample_config(), b"MESH2".to_vec(), key);
        assert_eq!(a.dial_static_key(), b.dial_static_key());
        assert_eq!(a.dial_static_key(), key);
    }

    #[test]
    fn different_source_keys_yield_different_dial_keys() {
        let a = StaticKeyProtocol::new(sample_config(), b"MESH2".to_vec(), [0x01u8; 32]);
        let b = StaticKeyProtocol::new(sample_config(), b"MESH2".to_vec(), [0x02u8; 32]);
        assert_ne!(a.dial_static_key(), b.dial_static_key());
    }

    // ── the dial is transport-aware ─────────────────────────────────────

    /// A datagram-shaped entrance must be dialled over DATAGRAMS — the
    /// subscriber has to reach a box that publishes only those faces.
    ///
    /// The listener is a UDP socket on an ephemeral loopback port and nothing
    /// listens on TCP for it, so a TCP dial could not even reach the
    /// handshake. The datagram that arrives is the V9 `ClientHello`.
    #[tokio::test]
    async fn a_datagram_camouflage_dials_over_datagrams_not_tcp() {
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.expect("bind ephemeral udp port");
        let addr = sock.local_addr().expect("local_addr of the bound udp socket");
        let (tx, rx) = tokio::sync::oneshot::channel::<usize>();
        let server = tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let read = sock.recv(&mut buf).await.unwrap_or(0);
            let _ = tx.send(read);
        });

        // `transport: tcp` on purpose — the camouflage decides, not the config.
        let config = MirageEndpointConfig {
            transport: mirage_core::config::TransportKind::Tcp,
            auth: mirage_core::config::AuthMode::Blake3,
            camouflage: TransportCamouflage::RawUdp {
                bind_addr: "127.0.0.1:0".parse().expect("valid addr"),
            },
            byte_cipher: None,
            byte_cipher_mode: None,
        };
        let proto = StaticKeyProtocol::new(config, b"MESH2".to_vec(), [0x42u8; 32]);
        let dial = tokio::spawn(async move {
            let _ = proto.connect(addr, &[7u8; 32], "", CamouflageMode::Raw).await;
        });

        let read = tokio::time::timeout(std::time::Duration::from_secs(10), rx)
            .await
            .expect("the client must put a datagram on the wire")
            .expect("the oneshot sender must not be dropped");
        dial.abort();
        let _ = server.await;

        assert!(read > 0, "the V9 client hello must arrive as a non-empty datagram");
    }

    /// The stream path is untouched by the dispatcher: a stream camouflage
    /// still opens a TCP connection, and the bytes on the wire are the ones
    /// the TCP-only client has always produced.
    #[tokio::test]
    async fn a_stream_camouflage_still_dials_over_tcp() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral tcp port");
        let addr = listener.local_addr().expect("local_addr of the bound tcp listener");
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        let server = tokio::spawn(async move {
            let accepted = listener.accept().await.is_ok();
            let _ = tx.send(accepted);
        });

        let proto = StaticKeyProtocol::new(sample_config(), b"MESH2".to_vec(), [0x42u8; 32]);
        let dial = tokio::spawn(async move {
            let _ = proto.connect(addr, &[7u8; 32], "", CamouflageMode::Raw).await;
        });

        let accepted = tokio::time::timeout(std::time::Duration::from_secs(10), rx)
            .await
            .expect("the client must open a TCP connection")
            .expect("the oneshot sender must not be dropped");
        dial.abort();
        let _ = server.await;

        assert!(accepted, "the TCP listener must have accepted the client's connection");
    }
}
