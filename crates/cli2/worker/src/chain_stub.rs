//! Stub chain logic placeholder.
//!
// TODO: wire real chain logic from mirage2app client-core once integration phase begins.

use std::sync::Arc;
use tokio::net::TcpStream;
use tracing::debug;

/// Stub for the real chain runtime.
/// Owns exactly one chain's identity and handles SOCKS5 connections for it.
pub struct ChainStub {
    pub chain_id: String,
}

impl ChainStub {
    pub fn new(chain_id: String) -> Self {
        Self { chain_id }
    }

    /// Handle an incoming SOCKS5 connection.
    /// Stub implementation: logs the peer address and drops the connection.
    pub async fn handle_connection(self: Arc<Self>, stream: TcpStream) {
        let peer = stream
            .peer_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "<unknown>".to_string());
        debug!(
            chain_id = %self.chain_id,
            peer = %peer,
            "accepted connection (stub — dropping)"
        );
        drop(stream);
    }
}
