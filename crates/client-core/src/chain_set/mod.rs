//! ChainSet — central registry of all chains the client is configured to use.
//!
//! # Architecture
//!
//! Each chain is represented by a `ChainHandle` that owns:
//! - `TunnelPool` — pre-warmed tunnel slots.
//! - Optional `MuxPool` — mux sessions over tunnels.
//! - `ChainMetrics` — atomic latency/success counters.
//! - `ChainState` — health-state machine via ArcSwap.
//!
//! `ChainSet` wraps a `DashMap<ChainId, Arc<ChainHandle>>` for lock-free
//! concurrent reads (hot SOCKS5 path) and rare writes (monitor config push).

pub mod handle;
pub mod health;
pub mod router;
pub mod types;

use std::sync::Arc;

use arc_swap::ArcSwap;
use dashmap::DashMap;
use mirage_core::mirage::mesh::PeerTable;
use tokio::sync::Notify;
use tracing::info;

pub use handle::ChainHandle;
pub use router::{
    BypassMatcher, FlowKey, GeoTag, ProtocolHint, Router, RouterImpl, RoutingPolicy, TargetKey,
};
pub use types::{ChainId, ChainMetrics, ChainState, HealthProbeConfig, PrewarmStrategy, ScoreFn};

// ── ChainSet ──────────────────────────────────────────────────────────────────

/// Central registry of all chains the client is configured to use.
pub struct ChainSet {
    /// All chain handles keyed by chain ID.
    pub chains: Arc<DashMap<ChainId, Arc<ChainHandle>>>,
    /// Prewarm strategy for pool maintenance.
    pub prewarm: PrewarmStrategy,
    /// Shared peer table — written by MonitorLink, read by pool reconnect tasks.
    pub peer_table: Arc<ArcSwap<PeerTable>>,
    /// Notify woken on each peer-table generation bump.
    pub peer_table_notify: Arc<Notify>,
    /// Mesh ID baked into every handshake.
    pub mesh_id: Vec<u8>,
    /// Health probe config shared across all per-chain probe tasks.
    pub probe_config: HealthProbeConfig,
}

impl ChainSet {
    /// Construct an empty `ChainSet` with no chains.
    ///
    /// Used as a no-op placeholder when a component needs an `Arc<ChainSet>`
    /// but does not use OverChain transports (e.g. the legacy CLI, tests).
    pub fn empty() -> Arc<Self> {
        use arc_swap::ArcSwap;
        use dashmap::DashMap;
        Arc::new(Self {
            chains: Arc::new(DashMap::new()),
            prewarm: PrewarmStrategy::default(),
            peer_table: Arc::new(ArcSwap::from_pointee(PeerTable::default())),
            peer_table_notify: Arc::new(Notify::new()),
            mesh_id: Vec::new(),
            probe_config: HealthProbeConfig::default(),
        })
    }

    /// Enumerate only currently Active chains.
    pub fn active_chains(&self) -> Vec<Arc<ChainHandle>> {
        self.chains
            .iter()
            .filter(|e| e.value().is_usable())
            .map(|e| Arc::clone(e.value()))
            .collect()
    }

    /// Enumerate all chains regardless of health state.
    pub fn all_chains(&self) -> Vec<Arc<ChainHandle>> {
        self.chains
            .iter()
            .map(|e| Arc::clone(e.value()))
            .collect()
    }

    /// Add or replace a chain (monitor push / operator reconfigure).
    ///
    /// If a handle with the same `chain_id` already exists, it is replaced
    /// atomically. The old handle is dropped when the last Arc reference goes
    /// away — in-flight leases on the old pool complete normally.
    pub async fn upsert_chain(&self, handle: Arc<ChainHandle>) {
        info!(chain_id = %handle.id, "chain_set: upsert chain");
        self.chains.insert(handle.id.clone(), handle);
    }

    /// Remove a chain (monitor says it is decommissioned).
    pub async fn remove_chain(&self, id: &ChainId) {
        info!(chain_id = %id, "chain_set: remove chain");
        self.chains.remove(id);
    }

    /// Spawn one `health_probe_task` per chain.
    ///
    /// Each task holds a `Weak<ChainHandle>`. When a chain is removed from
    /// the DashMap and the last strong Arc dropped, the task exits cleanly
    /// on its next iteration.
    pub fn spawn_health_probes(self: &Arc<Self>) {
        for entry in self.chains.iter() {
            let handle_weak = Arc::downgrade(entry.value());
            let config = self.probe_config.clone();
            tokio::spawn(health::health_probe_task(handle_weak, config));
        }
    }

    /// Notify all chain pools that the peer table has a new generation.
    pub fn notify_all_peer_table_change(&self) {
        for entry in self.chains.iter() {
            entry.value().notify_peer_table_change();
        }
        self.peer_table_notify.notify_waiters();
    }
}
