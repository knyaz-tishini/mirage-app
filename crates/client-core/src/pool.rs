//! [`TunnelPool`] — pre-warmed tunnel pool with idle-timeout reconnect.
//!
//! Maintains `N` tunnels to the L1 relay in one of three states:
//!
//! - **Idle** — handshake completed, ready to accept a target address.
//! - **Leased** — currently in use by a SOCKS5 or HTTP-CONNECT handler.
//! - **Reconnecting** — tunnel died; background task is re-establishing it.
//!
//! ## Idle-timeout heartbeat (no server changes required)
//!
//! Each idle slot tracks `last_used: Instant`.  A background task wakes every
//! second and closes any tunnel that has been idle longer than
//! `idle_timeout_secs`, then spawns a reconnect for that slot.  This avoids
//! the need for a server-side ping/pong frame type.
//!
//! ## Leasing model
//!
//! `pool.lease()` returns a [`TunnelLease`].  The lease wraps the raw tunnel
//! and implements [`Tunnel`].  When it is dropped:
//! - clean drop → tunnel is returned to the pool as `Idle`.
//! - poisoned drop (caller called [`TunnelLease::poison`]) → slot enters
//!   `Reconnecting` and the background task replaces it.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};

use mirage_core::{
    config::{ChainConfig, TunnelPoolConfig},
    error::MirageError,
};
use mirage_protocol::{negotiation::ProtocolChain, traits::Tunnel};
use tokio::sync::{Mutex, Notify};
use tracing::{debug, error, info, warn};

use crate::tunnel::establish_raw_tunnel;

// ── Reaper constants ──────────────────────────────────────────────────────────

/// How often the idle reaper wakes to scan for stale idle slots.
const TUNNEL_POOL_IDLE_REAPER_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(60);

/// Idle slots unused longer than this are closed by the reaper (not immediately
/// reconnected — they go `Dormant` and reconnect on the next `lease()` call).
const TUNNEL_POOL_IDLE_KILL_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(300);

// ── Slot state ────────────────────────────────────────────────────────────────

enum Slot {
    /// Ready to be leased.  `last_used` is when the slot last became idle.
    Idle {
        tunnel: Box<dyn Tunnel>,
        last_used: Instant,
    },
    /// Currently held by a [`TunnelLease`].
    Leased,
    /// Background task is re-establishing this slot.
    Reconnecting,
    /// Reaped by the idle reaper: underlying TCP was closed, no active reconnect
    /// is in flight.  The slot will be woken (→ `Reconnecting`) on the next
    /// `lease()` call that finds no `Idle` slot available.
    Dormant,
}

// ── Pool inner state ──────────────────────────────────────────────────────────

struct PoolInner {
    slots: Vec<Slot>,
    chain: ChainConfig,
    chain_protocols: Arc<ProtocolChain>,
    config: TunnelPoolConfig,
}

// ── Pool statistics ───────────────────────────────────────────────────────────

/// Snapshot of pool slot-state counters.  Returned by [`TunnelPool::pool_stats`].
#[derive(Debug, Clone)]
pub struct PoolStats {
    /// Total number of slots in the pool.
    pub size: usize,
    /// Slots that are ready to be leased.
    pub idle: usize,
    /// Slots currently held by active requests.
    pub leased: usize,
    /// Slots undergoing reconnection.
    pub reconnecting: usize,
    /// Slots reaped (TCP closed, will reconnect on next `lease()` demand).
    pub dormant: usize,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// A pool of pre-established raw tunnels (handshake done, target not yet sent).
///
/// Construct with [`TunnelPool::new`], then call [`TunnelPool::lease`] to borrow
/// a tunnel for one request.
pub struct TunnelPool {
    inner: Arc<Mutex<PoolInner>>,
    /// Notified whenever a slot becomes `Idle` (lease returned or reconnect done).
    available: Arc<Notify>,
    /// True when the pool's chain uses multi-hop onion routing (`chain.hops` non-empty).
    ///
    /// Leased tunnels inherit this flag so callers (SOCKS5, HTTP proxy) can apply
    /// onion-specific framing when sending the target address.
    pub is_onion: AtomicBool,
}

impl TunnelPool {
    /// Create the pool, fill it with `config.pool_size` tunnels, and spawn the
    /// idle-timeout background task.
    pub async fn new(
        chain: ChainConfig,
        chain_protocols: Arc<ProtocolChain>,
        config: TunnelPoolConfig,
    ) -> Result<Arc<Self>, MirageError> {
        let is_onion = !chain.hops.is_empty();
        let pool_size = config.pool_size.max(1);

        // Spawn all tunnel establishes in parallel.
        let mut join_set = tokio::task::JoinSet::new();
        for idx in 0..pool_size {
            let cp = Arc::clone(&chain_protocols);
            let ch = chain.clone();
            join_set.spawn(async move { (idx, establish_raw_tunnel(&cp, &ch).await) });
        }

        // Collect until 20% threshold is reached, then return early.
        let threshold = (pool_size / 5).max(1);
        let mut slots_partial: Vec<(usize, Slot)> = Vec::with_capacity(pool_size);
        let mut ready_count = 0usize;

        while let Some(result) = join_set.join_next().await {
            match result {
                Ok((idx, Ok(tunnel))) => {
                    slots_partial.push((idx, Slot::Idle { tunnel, last_used: Instant::now() }));
                    ready_count += 1;
                    info!(slot = idx, ready = ready_count, threshold, "pool slot established");
                }
                Ok((idx, Err(e))) => {
                    warn!(slot = idx, error = %e, "pool slot failed on startup");
                    slots_partial.push((idx, Slot::Reconnecting));
                }
                Err(e) => {
                    warn!(error = %e, "pool slot join error");
                }
            }
            if ready_count >= threshold && !join_set.is_empty() {
                break;
            }
        }

        // Fill any indices not yet collected as Reconnecting placeholders.
        let collected: std::collections::HashSet<usize> =
            slots_partial.iter().map(|(idx, _)| *idx).collect();
        for idx in 0..pool_size {
            if !collected.contains(&idx) {
                slots_partial.push((idx, Slot::Reconnecting));
            }
        }
        slots_partial.sort_by_key(|(idx, _)| *idx);
        let slots: Vec<Slot> = slots_partial.into_iter().map(|(_, s)| s).collect();

        info!(
            ready = ready_count,
            total = pool_size,
            threshold,
            "pool ready at threshold, remaining slots warming in background"
        );

        let available = Arc::new(Notify::new());
        let inner = Arc::new(Mutex::new(PoolInner {
            slots,
            chain,
            chain_protocols,
            config,
        }));

        let pool = Arc::new(Self {
            inner: Arc::clone(&inner),
            available: Arc::clone(&available),
            is_onion: AtomicBool::new(is_onion),
        });

        // Spawn the background maintenance task (reconnects Reconnecting slots).
        let pool_weak = Arc::downgrade(&pool);
        tokio::spawn(async move {
            maintenance_loop(pool_weak).await;
        });

        // Spawn the idle reaper task (closes long-idle tunnels → Dormant).
        let pool_weak_reaper = Arc::downgrade(&pool);
        tokio::spawn(async move {
            reaper_loop(pool_weak_reaper).await;
        });

        // Drain remaining JoinSet results in background — fills Reconnecting placeholders.
        if !join_set.is_empty() {
            let inner_clone = Arc::clone(&inner);
            let available_clone = Arc::clone(&available);
            tokio::spawn(async move {
                while let Some(result) = join_set.join_next().await {
                    match result {
                        Ok((idx, Ok(tunnel))) => {
                            let mut guard = inner_clone.lock().await;
                            if matches!(guard.slots.get(idx), Some(Slot::Reconnecting)) {
                                guard.slots[idx] =
                                    Slot::Idle { tunnel, last_used: Instant::now() };
                                info!(slot = idx, "background warmup: slot established");
                                available_clone.notify_one();
                            }
                        }
                        Ok((idx, Err(e))) => {
                            warn!(slot = idx, error = %e, "background warmup: slot failed");
                        }
                        Err(e) => {
                            warn!(error = %e, "background warmup: join error");
                        }
                    }
                }
            });
        }

        // Wake any waiters for slots that came up ready.
        available.notify_waiters();

        Ok(pool)
    }

    /// Lease an idle tunnel from the pool.
    ///
    /// Waits (with backoff notifications) until a slot is `Idle`.  Returns
    /// [`MirageError::PoolExhausted`] only when every slot is `Reconnecting`
    /// and no idle slot becomes available within a reasonable number of wakes.
    ///
    /// Stale tunnels (idle longer than `idle_timeout_secs`) are poisoned rather
    /// than leased — a QUIC connection that has been silent for minutes is likely
    /// dead and would fail immediately on first use.
    pub async fn lease(self: &Arc<Self>) -> Result<TunnelLease, MirageError> {
        // Try up to 60 times, waiting for a notification each time.
        for _ in 0..60u32 {
            {
                let mut guard = self.inner.lock().await;
                let max_idle = std::time::Duration::from_secs(guard.config.idle_timeout_secs);
                for (idx, slot) in guard.slots.iter_mut().enumerate() {
                    if let Slot::Idle { last_used, .. } = slot {
                        if last_used.elapsed() >= max_idle {
                            // Tunnel has been idle too long — it is likely stale.
                            // Poison it so the maintenance loop reconnects it.
                            warn!(
                                slot = idx,
                                idle_secs = last_used.elapsed().as_secs(),
                                "lease: idle timeout exceeded, poisoning stale tunnel"
                            );
                            *slot = Slot::Reconnecting;
                            continue;
                        }
                        // Swap out the tunnel.
                        let old = std::mem::replace(slot, Slot::Leased);
                        let tunnel = match old {
                            Slot::Idle { tunnel, .. } => tunnel,
                            _ => unreachable!(),
                        };
                        debug!(slot = idx, "leased tunnel from pool");
                        return Ok(TunnelLease {
                            tunnel: Some(tunnel),
                            pool: Arc::clone(self),
                            slot_idx: idx,
                            poisoned: false,
                            is_onion: self.is_onion.load(Ordering::Acquire),
                        });
                    }
                }
            }
            // No idle slot — wake any dormant slots so maintenance reconnects them.
            {
                let mut guard = self.inner.lock().await;
                let mut woke = false;
                for slot in guard.slots.iter_mut() {
                    if matches!(slot, Slot::Dormant) {
                        *slot = Slot::Reconnecting;
                        woke = true;
                    }
                }
                if woke {
                    debug!("lease: woke dormant slots for reconnect");
                    self.available.notify_one();
                }
            }
            // Wait for a slot to become available (timeout 500 ms).
            tokio::select! {
                _ = self.available.notified() => {}
                _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
            }
        }

        Err(MirageError::PoolExhausted)
    }

    /// Return a tunnel to the pool.  Called by [`TunnelLease`] on drop.
    pub(crate) async fn return_tunnel(&self, slot_idx: usize, tunnel: Box<dyn Tunnel>) {
        let mut guard = self.inner.lock().await;
        guard.slots[slot_idx] = Slot::Idle {
            tunnel,
            last_used: Instant::now(),
        };
        debug!(slot = slot_idx, "tunnel returned to pool");
        self.available.notify_one();
    }

    /// Return a snapshot of current pool statistics.
    ///
    /// Acquires the pool mutex briefly to count slot states, then releases it.
    pub async fn pool_stats(&self) -> PoolStats {
        let guard = self.inner.lock().await;
        let mut stats = PoolStats {
            size: guard.slots.len(),
            idle: 0,
            leased: 0,
            reconnecting: 0,
            dormant: 0,
        };
        for slot in &guard.slots {
            match slot {
                Slot::Idle { .. } => stats.idle += 1,
                Slot::Leased => stats.leased += 1,
                Slot::Reconnecting => stats.reconnecting += 1,
                Slot::Dormant => stats.dormant += 1,
            }
        }
        stats
    }

    /// Mark a slot for reconnection.  Called by [`TunnelLease`] when poisoned.
    pub(crate) async fn poison_slot(&self, slot_idx: usize) {
        let mut guard = self.inner.lock().await;
        guard.slots[slot_idx] = Slot::Reconnecting;
        debug!(slot = slot_idx, "tunnel poisoned, slot marked for reconnect");
        // Notify to trigger the maintenance loop sooner.
        self.available.notify_one();
    }

    /// Switch to a new chain and immediately mark all idle slots `Reconnecting`.
    ///
    /// Leased slots are unaffected — they finish on the old chain and are
    /// replaced when they return.  The maintenance loop picks up `Reconnecting`
    /// slots and reconnects them to the new chain.
    pub async fn switch_chain(&self, new_chain: mirage_core::config::ChainConfig) {
        let mut guard = self.inner.lock().await;
        guard.chain = new_chain;
        for slot in guard.slots.iter_mut() {
            if matches!(slot, Slot::Idle { .. } | Slot::Dormant) {
                *slot = Slot::Reconnecting;
            }
        }
        self.is_onion.store(!guard.chain.hops.is_empty(), Ordering::Release);
        info!("chain switched (drain): idle and dormant slots marked for reconnect");
        self.available.notify_waiters();
    }

    /// Switch to a new chain lazily.
    ///
    /// Updates the chain reference only.  Existing idle slots keep their
    /// current tunnels until they expire via the idle-timeout mechanism.
    /// Only new reconnections will use the new chain.
    pub async fn switch_chain_lazy(&self, new_chain: mirage_core::config::ChainConfig) {
        let mut guard = self.inner.lock().await;
        guard.chain = new_chain;
        self.is_onion.store(!guard.chain.hops.is_empty(), Ordering::Release);
        info!("chain switched (lazy): new reconnects will use new chain");
    }
}

// ── TunnelLease ───────────────────────────────────────────────────────────────

/// RAII handle that holds one tunnel from the pool.
///
/// Implements [`Tunnel`] transparently.  On drop, returns the tunnel to the
/// pool unless [`TunnelLease::poison`] was called, in which case the slot is
/// marked for reconnection.
pub struct TunnelLease {
    tunnel: Option<Box<dyn Tunnel>>,
    pool: Arc<TunnelPool>,
    slot_idx: usize,
    poisoned: bool,
    /// Mirrors `TunnelPool::is_onion`. When true, callers must send the target
    /// address with a `[2B BE len][target_utf8]` prefix instead of raw bytes,
    /// because the onion exit reads a length-prefixed address from the raw
    /// forward stream.
    pub is_onion: bool,
}

impl TunnelLease {
    /// Mark this lease as failed.  The pool will replace the tunnel rather
    /// than reusing it.
    pub fn poison(&mut self) {
        self.poisoned = true;
    }

    /// Notify the tunnel stack whether a relay is in progress.
    ///
    /// When `active` is `true`, cover-traffic injection is suppressed so that
    /// no dummy bytes are written into the live application-data stream.
    /// Call with `true` before entering `relay_loop` and `false` after.
    ///
    /// Delegates to [`Tunnel::set_relay_active`]; if the inner tunnel does not
    /// override that method (e.g. cover is disabled), this is a no-op.
    pub fn set_relay_active(&mut self, active: bool) {
        if let Some(t) = self.tunnel.as_mut() {
            t.set_relay_active(active);
        }
    }

    /// Extract the inner tunnel, leaving `None` in its place.
    ///
    /// After this call the lease no longer owns a tunnel.  Drop will skip the
    /// return-to-pool path (the `None` branch in [`Drop`]).  Call
    /// [`TunnelLease::poison`] before dropping if the slot should reconnect.
    pub fn take_tunnel(&mut self) -> Option<Box<dyn Tunnel>> {
        self.tunnel.take()
    }
}

impl Drop for TunnelLease {
    fn drop(&mut self) {
        let pool = Arc::clone(&self.pool);
        let slot_idx = self.slot_idx;
        let poisoned = self.poisoned;
        let tunnel = self.tunnel.take();

        // Two distinct exit paths:
        // 1. tunnel still owned by lease → return to pool (or poison if marked).
        // 2. tunnel was extracted earlier via `take_tunnel()` (run_simple does
        //    this before wrapping in TunnelCompat for relay_loop) → can't put
        //    it back. The slot would otherwise stay Idle forever holding a
        //    dead tunnel (the canonical TCP exit closes the upstream after one
        //    request, so subsequent leases on that slot fail). When the caller
        //    flagged the lease as poisoned (always done by run_simple after
        //    relay_loop ends), poison the slot so the pool reconnects.
        //    Bug fix May 2026: previously this branch silently returned, and
        //    multi-request on the same chain was broken on every TCP chain
        //    incl. baseline Mirage.
        tokio::spawn(async move {
            match (tunnel, poisoned) {
                (Some(t), false) => pool.return_tunnel(slot_idx, t).await,
                (Some(_), true) => pool.poison_slot(slot_idx).await,
                (None, true) => pool.poison_slot(slot_idx).await,
                (None, false) => {
                    // Lease silently dropped its tunnel without poisoning —
                    // slot is stuck. Log + poison anyway to recover.
                    tracing::warn!(
                        slot = slot_idx,
                        "TunnelLease dropped without poison and without tunnel — \
                         poisoning slot to force reconnect"
                    );
                    pool.poison_slot(slot_idx).await;
                }
            }
        });
    }
}

#[async_trait::async_trait]
impl Tunnel for TunnelLease {
    async fn send(&mut self, data: &[u8]) -> Result<(), MirageError> {
        match self.tunnel.as_mut() {
            Some(t) => t.send(data).await,
            None => Err(MirageError::TunnelClosed),
        }
    }

    async fn recv(&mut self, buf: &mut [u8]) -> Result<usize, MirageError> {
        match self.tunnel.as_mut() {
            Some(t) => t.recv(buf).await,
            None => Err(MirageError::TunnelClosed),
        }
    }

    async fn close(&mut self) -> Result<(), MirageError> {
        match self.tunnel.as_mut() {
            Some(t) => t.close().await,
            None => Ok(()),
        }
    }

    fn transport_info(&self) -> &mirage_protocol::traits::TransportInfo {
        // Safety: the tunnel is only None during drop after we took it out.
        self.tunnel.as_ref().expect("transport_info called after tunnel taken").transport_info()
    }

    fn set_relay_active(&mut self, active: bool) {
        if let Some(t) = self.tunnel.as_mut() {
            t.set_relay_active(active);
        }
    }
}

// ── Background reaper loop ────────────────────────────────────────────────────

/// Background task: periodically closes long-idle tunnel slots.
///
/// Slots that have been `Idle` longer than [`TUNNEL_POOL_IDLE_KILL_TIMEOUT`] are
/// transitioned to `Dormant`: the TCP connection is closed immediately and no
/// reconnect is spawned.  The slot will be woken to `Reconnecting` on the next
/// `lease()` call that finds no live `Idle` slot, keeping the pool bursty-idle
/// friendly (resources freed when quiet, reconnected when needed).
///
/// Stops automatically when the pool is dropped (weak reference upgrade fails).
async fn reaper_loop(pool_weak: std::sync::Weak<TunnelPool>) {
    loop {
        tokio::time::sleep(TUNNEL_POOL_IDLE_REAPER_INTERVAL).await;

        let Some(pool) = pool_weak.upgrade() else {
            // Pool was dropped — exit cleanly.
            return;
        };

        // Collect slots that are long-idle and take ownership of their tunnels.
        let mut stale_tunnels: Vec<(usize, Box<dyn Tunnel>)> = Vec::new();
        {
            let mut guard = pool.inner.lock().await;
            for (idx, slot) in guard.slots.iter_mut().enumerate() {
                if let Slot::Idle { last_used, .. } = slot {
                    if last_used.elapsed() >= TUNNEL_POOL_IDLE_KILL_TIMEOUT {
                        // Swap out the tunnel so we can close it outside the lock.
                        let old = std::mem::replace(slot, Slot::Dormant);
                        if let Slot::Idle { tunnel, .. } = old {
                            stale_tunnels.push((idx, tunnel));
                        }
                    }
                }
            }
        }

        // Close each stale tunnel outside the lock (close is async).
        for (idx, mut tunnel) in stale_tunnels {
            info!(
                slot = idx,
                idle_secs = TUNNEL_POOL_IDLE_KILL_TIMEOUT.as_secs(),
                "reaper: closing long-idle tunnel (→ Dormant)"
            );
            if let Err(e) = tunnel.close().await {
                debug!(slot = idx, error = %e, "reaper: tunnel close returned error (ignored)");
            }
        }
    }
}

// ── Background maintenance loop ───────────────────────────────────────────────

/// Background task: checks idle-timeout expiry and triggers reconnects.
///
/// Runs every second.  Uses a weak reference so it stops automatically when
/// the pool is dropped.  Tracks in-flight reconnect tasks per slot to prevent
/// duplicate spawns when a slot stays `Reconnecting` across multiple ticks.
async fn maintenance_loop(pool_weak: std::sync::Weak<TunnelPool>) {
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<usize>();
    let mut in_flight: std::collections::HashSet<usize> = std::collections::HashSet::new();

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        // Drain completion notifications from finished reconnect tasks.
        while let Ok(idx) = done_rx.try_recv() {
            in_flight.remove(&idx);
        }

        let Some(pool) = pool_weak.upgrade() else {
            // Pool was dropped — stop.
            return;
        };

        // Collect slots that need reconnecting (idle-timeout expired).
        let mut all_indices: Vec<usize> = {
            let mut guard = pool.inner.lock().await;
            let idle_timeout =
                std::time::Duration::from_secs(guard.config.idle_timeout_secs);
            let mut indices = Vec::new();

            for (idx, slot) in guard.slots.iter_mut().enumerate() {
                if let Slot::Idle { last_used, .. } = slot {
                    if last_used.elapsed() >= idle_timeout {
                        debug!(slot = idx, "idle timeout expired, reconnecting");
                        *slot = Slot::Reconnecting;
                        indices.push(idx);
                    }
                }
            }
            indices
        };

        // Also pick up any slots already in Reconnecting state.
        // Dormant slots are intentionally skipped — they reconnect on demand.
        {
            let guard = pool.inner.lock().await;
            for (i, s) in guard.slots.iter().enumerate() {
                if matches!(s, Slot::Reconnecting) && !all_indices.contains(&i) {
                    all_indices.push(i);
                }
            }
        }

        for slot_idx in all_indices {
            if in_flight.contains(&slot_idx) {
                // Already has a live reconnect task — skip this tick.
                continue;
            }
            in_flight.insert(slot_idx);
            let pool_clone = Arc::clone(&pool);
            let done_tx = done_tx.clone();
            tokio::spawn(async move {
                reconnect_slot(pool_clone, slot_idx).await;
                let _ = done_tx.send(slot_idx);
            });
        }
    }
}

/// Attempt to reconnect a single slot with exponential backoff.
async fn reconnect_slot(pool: Arc<TunnelPool>, slot_idx: usize) {
    let (max_attempts, base_ms, chain, chain_protocols) = {
        let guard = pool.inner.lock().await;
        (
            guard.config.max_reconnect_attempts,
            guard.config.reconnect_base_ms,
            guard.chain.clone(),
            Arc::clone(&guard.chain_protocols),
        )
    };

    let mut attempt = 0u32;
    loop {
        attempt += 1;
        debug!(slot = slot_idx, attempt, "reconnect attempt");

        match establish_raw_tunnel(&chain_protocols, &chain).await {
            Ok(tunnel) => {
                info!(slot = slot_idx, attempt, "reconnect succeeded");
                let mut guard = pool.inner.lock().await;
                guard.slots[slot_idx] = Slot::Idle {
                    tunnel,
                    last_used: Instant::now(),
                };
                pool.available.notify_one();
                return;
            }
            Err(e) => {
                let limit_reached = max_attempts > 0 && attempt >= max_attempts;
                if limit_reached {
                    error!(
                        slot = slot_idx,
                        attempt,
                        error = %e,
                        "reconnect gave up after max attempts"
                    );
                    // Leave slot as Reconnecting — maintenance loop will retry later.
                    return;
                }
                let delay_ms = base_ms.saturating_mul(1u64 << attempt.min(10));
                warn!(
                    slot = slot_idx,
                    attempt,
                    delay_ms,
                    error = %e,
                    "reconnect failed, retrying"
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
        }
    }
}
