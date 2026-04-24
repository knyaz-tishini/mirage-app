//! `EndpointProtocol` — adapter that bridges `MirageEndpointConfig` to the
//! legacy `TcpProtocol` trait on the client side.
//!
//! The old per-struct protocol types (MirageProtocol, MirageShadowTlsProtocol,
//! etc.) have been removed.  This adapter wraps a `MirageEndpointConfig` and
//! delegates to `mirage_protocol::driver::run_client_tcp` for outbound tunnels.
//!
//! Only the client-side `connect` path is needed here — the `accept` path is
//! a server-only concern.

use std::net::SocketAddr;

use async_trait::async_trait;
use mirage_core::{
    config::{CamouflageMode, MirageEndpointConfig, TcpProtocolId},
    error::MirageError,
};
use mirage_protocol::{
    driver::run_client_tcp,
    traits::{AcceptResult, TcpProtocol, Tunnel},
};
use tokio::net::TcpStream;

/// Client-side adapter that wraps a `MirageEndpointConfig` and implements
/// [`TcpProtocol`].
///
/// Only `connect` is meaningful on the client side.  `accept` is included to
/// satisfy the trait but will always return an error.
pub struct EndpointProtocol {
    config: MirageEndpointConfig,
    protocol_id: TcpProtocolId,
}

impl EndpointProtocol {
    /// Create a new client-side adapter.
    pub fn new(config: MirageEndpointConfig, protocol_id: TcpProtocolId) -> Self {
        Self { config, protocol_id }
    }
}

#[async_trait]
impl TcpProtocol for EndpointProtocol {
    async fn connect(
        &self,
        addr: SocketAddr,
        server_pubkey: &[u8; 32],
        sni: &str,
        _camouflage: CamouflageMode,
    ) -> Result<Box<dyn Tunnel>, MirageError> {
        run_client_tcp(&self.config, addr, server_pubkey, sni).await
    }

    async fn accept(
        &self,
        _stream: TcpStream,
        _peeked: &[u8],
    ) -> Result<AcceptResult, MirageError> {
        // Client-side protocol adapters are never used for server accept.
        Err(MirageError::Protocol(
            "EndpointProtocol::accept called on client-side adapter".into(),
        ))
    }

    fn id(&self) -> TcpProtocolId {
        self.protocol_id
    }

    fn probe_resistant(&self) -> bool {
        true
    }
}
