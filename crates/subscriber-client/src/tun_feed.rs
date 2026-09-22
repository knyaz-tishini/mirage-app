//! Optional feed into `mirage-client-core`'s `TunBridge` — points the TUN
//! virtual interface at this crate's own local, router-backed SOCKS5
//! listener.
//!
//! **Chosen default for an explicitly unresolved product question.**
//! `thin-customer-connector-2026-09-17.md` §5 and §8 #5 leave "TUN vs the
//! N-way sticky router" undecided: TUN's own chain registry
//! (`client-core/src/tun_bridge.rs::chain_to_json`) wants a per-chain
//! `socks5_addr` — a LOCAL listener address — so wiring one `MonitorChain`
//! per entrance would mean standing up a dedicated SOCKS5 listener per face
//! in addition to the shared, sharded one, which the plan explicitly does
//! not decide. This module instead feeds TUN a single named chain
//! (`TUN_FEED_CHAIN_ID`) whose `socks5_addr` points at THIS crate's own
//! shared listener — TUN gets one chain that already shards across every
//! entrance internally via the router, rather than zero sharding or a second
//! listener set per face. Easy to replace with a one-chain-per-entrance feed
//! once that product question is settled; nothing here forecloses it.
//!
//! Only the two plain data structs `MonitorConfig`/`MonitorChain` are
//! imported from `mirage_client_core::monitor_link` — never `MonitorLink`
//! itself. No monitor WS dial, no debug-tunnel listener, anywhere in this
//! module.

use std::net::SocketAddr;
use std::sync::Arc;

use mirage_client_core::monitor_link::{MonitorChain, MonitorConfig};
use mirage_client_core::tun_bridge::TunBridge;
use mirage_core::config::{ChainConfig, HandshakeStrategy, ProtocolId, ReplyMode, TcpProtocolId};
use mirage_core::crypto::AeadSuite;
use tokio::sync::watch;

/// Chain name reported to TUN for the shared, router-backed local listener.
pub const TUN_FEED_CHAIN_ID: &str = "subscriber-entrances";

/// Owns the background watcher task that pushes chain-registry updates to
/// TUN's `/chains/update`. Dropping this stops future pushes (the watcher
/// task exits once the `watch::Sender` half is gone); it does not reach out
/// and unregister the chain TUN already has.
pub struct TunFeed {
    _tx: watch::Sender<Option<MonitorConfig>>,
    _watcher: tokio::task::JoinHandle<()>,
}

impl TunFeed {
    /// Start feeding `tun_api_url` with one named chain pointing at
    /// `socks5_bind` — see the module doc for why one shared chain, not one
    /// per entrance.
    pub fn start(tun_api_url: String, socks5_bind: SocketAddr) -> Self {
        let (tx, rx) = watch::channel(None);
        let bridge = Arc::new(TunBridge::new(tun_api_url));
        let watcher = bridge.spawn_watcher(rx);

        // Never dialed directly — `chain_to_json` only reads `chain_id`,
        // `socks5_bind`, `protocol`, `kill_switch`, `group`. `hops` is left
        // empty on purpose: this `ChainConfig` describes a registry entry,
        // not a dial target.
        let chain = ChainConfig {
            chain_id: TUN_FEED_CHAIN_ID.to_owned(),
            hops: Vec::new(),
            handshake: HandshakeStrategy::Telescopic,
            reply: ReplyMode::Raw,
            protocol: ProtocolId::Tcp(TcpProtocolId::Mirage),
            priority: 0,
            shaping: Default::default(),
            camouflage: Default::default(),
            cover_traffic: Default::default(),
            socks5_bind: Some(socks5_bind.to_string()),
            aead_suite: AeadSuite::ChaCha20Poly1305,
            kill_switch: None,
            group: None,
            nature: None,
        };
        let config = MonitorConfig {
            version: 1,
            chains: vec![MonitorChain { chain, healthy: true, latency_ms: None }],
            policy: None,
            camouflage: None,
            shaping: None,
            metadata_policy: None,
        };
        // `watch::channel` never fails on send while a receiver is held —
        // `rx` was just moved into `spawn_watcher` and the resulting task
        // owns it for the task's lifetime, so this send always has a live
        // receiver.
        let _ = tx.send(Some(config));

        Self { _tx: tx, _watcher: watcher }
    }
}
