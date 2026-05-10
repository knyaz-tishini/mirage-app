use std::collections::{HashMap, VecDeque};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};

use arc_swap::ArcSwap;
use mirage_core::config::ClientConfig;
use tokio_util::sync::CancellationToken;

use crate::mux_pool::MuxPool;
use crate::pool::TunnelPool;

/// Central runtime object for the mirage-client.
///
/// Holds the shared pools (one per chain), hot-swappable active chain
/// reference, and the loaded client configuration.  Passed as
/// `Arc<ClientRuntime>` to management handlers and any other code that needs
/// live client state.
pub struct ClientRuntime {
    /// All tunnel pools keyed by `chain_id`.
    ///
    /// Wrapped in `ArcSwap` so the monitor-link watcher can atomically swap
    /// the entire map when the chain config changes without stopping any
    /// in-flight connections.
    pub pools: Arc<ArcSwap<HashMap<String, Arc<TunnelPool>>>>,
    /// `chain_id` of the default chain (highest-priority at startup).
    pub default_chain_id: String,
    /// Optional mux pool — present when `config.use_mux` is true.
    /// Points at the *default chain's* mux pool. Kept for backward compat.
    /// New code should prefer [`Self::active_mux_pool`] which honours the
    /// hot-swappable `active_chain_id` for soft cross-chain failover.
    pub mux_pool: Option<Arc<MuxPool>>,
    /// All mux pools keyed by `chain_id`. Mirrors `pools` so the runtime
    /// can hold every chain's mux fast-path warm in parallel and the
    /// health_watcher's chain switch picks up the matching mux pool
    /// without reconnecting. Empty when `config.use_mux` is false (every
    /// chain falls back to the plain `TunnelPool` legacy path).
    pub mux_pools: ArcSwap<HashMap<String, Arc<MuxPool>>>,
    /// `chain_id` of the currently active chain — updated atomically on switch.
    pub active_chain_id: ArcSwap<String>,
    /// Current client configuration.  Stored via ArcSwap to allow future
    /// full-config hot-reload; for now it is only written once at startup.
    pub config: ArcSwap<ClientConfig>,
    /// Monotonically increasing counter incremented on every successful reconfig.
    pub config_version: AtomicU64,
    /// Whether the SOCKS5 listener is running.  SOCKS5 is always active at
    /// startup; it can only be disabled if another mode is also running.
    pub mode_socks5: AtomicBool,
    /// Whether the HTTP CONNECT proxy is running.
    pub mode_http: AtomicBool,
    /// Whether the TUN stack is running.
    pub mode_tun: AtomicBool,
    /// Cancellation token for the HTTP CONNECT proxy task.
    /// `None` when the HTTP proxy is not running.
    pub http_cancel: tokio::sync::Mutex<Option<CancellationToken>>,
    /// Cancellation token for the TUN stack task.
    /// `None` when the TUN stack is not running.
    pub tun_cancel: tokio::sync::Mutex<Option<CancellationToken>>,
    /// In-memory ring buffer of captured log lines (newest at back).
    pub log_buffer: Arc<std::sync::Mutex<VecDeque<String>>>,
}

impl ClientRuntime {
    /// Construct a new `ClientRuntime` with a single pool (backward-compat).
    pub fn new(
        pool: Arc<TunnelPool>,
        mux_pool: Option<Arc<MuxPool>>,
        active_chain_id: String,
        config: ClientConfig,
        mode_socks5: bool,
        mode_http: bool,
        mode_tun: bool,
        log_buffer: Arc<std::sync::Mutex<VecDeque<String>>>,
    ) -> Arc<Self> {
        let mut pools_map = HashMap::new();
        pools_map.insert(active_chain_id.clone(), pool);
        let mut mux_map: HashMap<String, Arc<MuxPool>> = HashMap::new();
        if let Some(mp) = mux_pool.clone() {
            mux_map.insert(active_chain_id.clone(), mp);
        }
        Arc::new(Self {
            pools: Arc::new(ArcSwap::new(Arc::new(pools_map))),
            default_chain_id: active_chain_id.clone(),
            mux_pool,
            mux_pools: ArcSwap::new(Arc::new(mux_map)),
            active_chain_id: ArcSwap::new(Arc::new(active_chain_id)),
            config: ArcSwap::new(Arc::new(config)),
            config_version: AtomicU64::new(0),
            mode_socks5: AtomicBool::new(mode_socks5),
            mode_http: AtomicBool::new(mode_http),
            mode_tun: AtomicBool::new(mode_tun),
            http_cancel: tokio::sync::Mutex::new(None),
            tun_cancel: tokio::sync::Mutex::new(None),
            log_buffer,
        })
    }

    /// Construct a `ClientRuntime` with multiple pools (multi-chain mode).
    ///
    /// `mux_pools` mirrors `pools` keyed by chain_id and is held warm in
    /// parallel — health_watcher chain switches pick up the matching mux
    /// pool without reconnecting. Pass an empty map when `use_mux` is off.
    /// `mux_pool` is the default chain's mux pool (legacy single-pool API).
    pub fn new_multi(
        pools: HashMap<String, Arc<TunnelPool>>,
        default_chain_id: String,
        mux_pool: Option<Arc<MuxPool>>,
        mux_pools: HashMap<String, Arc<MuxPool>>,
        config: ClientConfig,
        mode_socks5: bool,
        mode_http: bool,
        mode_tun: bool,
        log_buffer: Arc<std::sync::Mutex<VecDeque<String>>>,
    ) -> Arc<Self> {
        let active_chain_id = default_chain_id.clone();
        Arc::new(Self {
            pools: Arc::new(ArcSwap::new(Arc::new(pools))),
            default_chain_id,
            mux_pool,
            mux_pools: ArcSwap::new(Arc::new(mux_pools)),
            active_chain_id: ArcSwap::new(Arc::new(active_chain_id)),
            config: ArcSwap::new(Arc::new(config)),
            config_version: AtomicU64::new(0),
            mode_socks5: AtomicBool::new(mode_socks5),
            mode_http: AtomicBool::new(mode_http),
            mode_tun: AtomicBool::new(mode_tun),
            http_cancel: tokio::sync::Mutex::new(None),
            tun_cancel: tokio::sync::Mutex::new(None),
            log_buffer,
        })
    }

    /// Atomically load a snapshot of the current pools map.
    ///
    /// The returned `Arc` is cheap to clone and remains valid even after a
    /// hot-swap; callers can hold it across `.await` points safely.
    pub fn pool_snapshot(&self) -> Arc<HashMap<String, Arc<TunnelPool>>> {
        self.pools.load_full()
    }

    /// Atomically replace the entire pools map.
    ///
    /// In-flight connections keep their existing `Arc<TunnelPool>` references
    /// and complete normally.  The old map is dropped when all references to
    /// it are released.
    pub fn swap_pools(&self, new_pools: HashMap<String, Arc<TunnelPool>>) {
        self.pools.store(Arc::new(new_pools));
    }

    /// Borrow the pool for the given chain id, if it exists.
    pub fn pool_for_chain(&self, name: &str) -> Option<Arc<TunnelPool>> {
        self.pools.load().get(name).cloned()
    }

    /// Borrow the default chain's pool.
    ///
    /// # Panics
    ///
    /// Panics only if the runtime was constructed without any pools, which
    /// cannot happen through the public constructors.
    pub fn default_pool(&self) -> Arc<TunnelPool> {
        self.pools
            .load()
            .get(&self.default_chain_id)
            .cloned()
            .expect("default pool must exist — invariant upheld by constructors")
    }

    /// Borrow the pool for the currently active chain, falling back to the
    /// default pool when the active chain has no dedicated pool.
    pub fn active_pool(&self) -> Arc<TunnelPool> {
        let id = self.active_chain_id.load();
        self.pool_for_chain(id.as_ref())
            .unwrap_or_else(|| self.default_pool())
    }

    /// Borrow the mux pool for the currently active chain, when present.
    ///
    /// On a soft cross-chain failover the health_watcher rotates
    /// `active_chain_id`; this accessor immediately picks up the new
    /// chain's mux pool (if it has one) without waiting for a reconnect.
    /// Returns `None` when the active chain has no mux pool — caller
    /// drops to the plain `TunnelPool` legacy fallback.
    pub fn active_mux_pool(&self) -> Option<Arc<MuxPool>> {
        let id = self.active_chain_id.load();
        let map = self.mux_pools.load();
        map.get(id.as_ref()).cloned()
    }

    /// Atomically replace the entire mux-pool map.
    pub fn swap_mux_pools(&self, new_pools: HashMap<String, Arc<MuxPool>>) {
        self.mux_pools.store(Arc::new(new_pools));
    }

    /// Load current mode flags as plain bools.
    pub fn modes(&self) -> (bool, bool, bool) {
        (
            self.mode_socks5.load(Ordering::SeqCst),
            self.mode_http.load(Ordering::SeqCst),
            self.mode_tun.load(Ordering::SeqCst),
        )
    }
}
