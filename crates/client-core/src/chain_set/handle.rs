//! `ChainHandle` — owns all runtime state for one chain.

use arc_swap::ArcSwap;
use std::sync::Arc;
use std::time::Instant;

use super::types::{ChainId, ChainMetrics, ChainState};
use crate::{mux_pool::MuxPool, pool::TunnelPool};
use mirage_core::config::ChainConfig;

/// Owns all runtime state for one chain.
///
/// ## Concurrency model
///
/// - `config`  — `ArcSwap<ChainConfig>` for lock-free reads on hot path;
///   written only by `ChainSet::upsert_chain`.
/// - `pool` / `mux` — `Arc`: multiple readers (health task, SOCKS5 handlers).
/// - `metrics` — all atomics, no mutex.
/// - `state` — `ArcSwap<ChainState>`: health task writes, Router reads.
///   ArcSwap chosen over RwLock because state reads are on the critical SOCKS5
///   path and writes are rare (once per health-probe cycle).
pub struct ChainHandle {
    pub id: ChainId,
    pub config: Arc<ArcSwap<ChainConfig>>,
    pub pool: Arc<TunnelPool>,
    pub mux: Option<Arc<MuxPool>>,
    pub metrics: Arc<ChainMetrics>,
    pub state: Arc<ArcSwap<ChainState>>,
}

impl ChainHandle {
    /// Load current state snapshot.
    pub fn current_state(&self) -> ChainState {
        (**self.state.load()).clone()
    }

    /// True when this chain can currently accept new flows.
    pub fn is_usable(&self) -> bool {
        self.current_state().is_usable()
    }

    /// Transition to Probing (health probe starting).
    pub fn mark_probing(&self) {
        self.state.store(Arc::new(ChainState::Probing {
            since: Instant::now(),
        }));
    }

    /// Transition to Active after a successful probe.
    pub fn mark_active(&self) {
        self.state.store(Arc::new(ChainState::Active));
    }

    /// Transition to Quarantined after consecutive probe failures.
    pub fn mark_quarantined(&self, recovery_secs: u64, reason: String) {
        let until = Instant::now() + std::time::Duration::from_secs(recovery_secs);
        self.state
            .store(Arc::new(ChainState::Quarantined { until, reason }));
    }

    /// Transition to Dormant when no flows have used this chain for a while.
    pub fn mark_dormant(&self) {
        self.state.store(Arc::new(ChainState::Dormant {
            since: Instant::now(),
        }));
    }

    /// Convenience: notify both pool and mux pool that the peer table changed.
    pub fn notify_peer_table_change(&self) {
        self.pool.notify_peer_table_change();
        if let Some(mp) = &self.mux {
            mp.notify_peer_table_change();
        }
    }
}
