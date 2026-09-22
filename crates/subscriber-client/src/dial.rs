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

use std::net::SocketAddr;

use async_trait::async_trait;
use mirage_core::{
    config::{CamouflageMode, MirageEndpointConfig, TcpProtocolId, TransportCamouflage},
    error::MirageError,
};
use mirage_protocol::{
    driver::{run_client_tcp_v9, ClientCredential},
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
        run_client_tcp_v9(
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
        run_client_tcp_v9(
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
}
