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

use arc_swap::ArcSwap;
use mirage_core::{
    config::{ChainConfig, TunnelPoolConfig},
    crypto::credential::CredentialParams,
    error::MirageError,
    mirage::mesh::PeerTable,
};
use mirage_protocol::{negotiation::ProtocolChain, traits::Tunnel};
use tokio::sync::{Mutex, Notify};
use tracing::{debug, error, info, warn};

use crate::token_pool::{self, TokenPool};
use crate::tunnel::{establish_raw_tunnel, freshen_chain_pubkeys};

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
    mesh_id: Vec<u8>,
    /// Live peer table shared with the monitor link.  Each dial loads a
    /// snapshot via `ArcSwap::load()` and passes it to `establish_raw_tunnel`.
    peer_table: Arc<ArcSwap<PeerTable>>,
    /// Operator-issued credential params pushed by MonitorLink.
    /// Shared via Arc::clone — pool snapshots it on each dial and mints a fresh
    /// per-dial token.  None = credential feature inactive (tolerant mode).
    credential_params: Arc<ArcSwap<Option<CredentialParams>>>,
    /// Blind-RSA token pool (Phase 4 spend).  Each dial pops one token when the
    /// pool is non-empty and presents it as a `pre_minted` BlindRsaBatch credential
    /// (scheme `0x03`).  When empty, the dial falls back to EpochHmac self-mint or
    /// proceeds without a credential (existing behaviour).
    token_pool: Arc<ArcSwap<TokenPool>>,
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
    /// Notified when the shared peer_table receives a new generation.
    ///
    /// Sleeping reconnect tasks subscribe to this so they wake immediately
    /// and retry with fresh peer data instead of waiting out exponential backoff.
    peer_table_notify: Arc<Notify>,
    /// Cached chain ID for zero-cost log fields (avoids locking inner mutex).
    chain_id: Arc<str>,
}

impl TunnelPool {
    /// Create the pool, fill it with `config.pool_size` tunnels, and spawn the
    /// idle-timeout background task.
    ///
    /// `peer_table` — shared live peer table from the monitor link (Phase 8.5).
    /// Pass `Arc::new(ArcSwap::from_pointee(PeerTable::empty()))` when there
    /// is no monitor link; only chains with `hop.public_key = Some(hex)` will
    /// be dialable in that case.
    ///
    /// `token_pool` — shared blind-RSA token pool (Phase 4).  Pass
    /// `Arc::new(ArcSwap::from_pointee(TokenPool::default()))` when no
    /// MonitorLink is present; the pool will always be empty and dials fall
    /// back to EpochHmac self-mint.
    pub async fn new(
        chain: ChainConfig,
        chain_protocols: Arc<ProtocolChain>,
        config: TunnelPoolConfig,
        mesh_id: Vec<u8>,
        peer_table: Arc<ArcSwap<PeerTable>>,
        credential_params: Arc<ArcSwap<Option<CredentialParams>>>,
        token_pool: Arc<ArcSwap<TokenPool>>,
    ) -> Result<Arc<Self>, MirageError> {
        // Phase 9.7: onion = multi-hop chain. Single-hop direct exit
        // (hops[0].role == Exit) reaches `handle_exit` server-side as a
        // classic direct tunnel — it must NOT prefix the address with
        // a 2-byte length, because that length's high byte (0x00 for
        // any address < 256 chars) collides with the mux-detect byte
        // and the server routes the connection into `run_mux_session`.
        let is_onion = chain.hops.len() > 1;
        let chain_id: Arc<str> = Arc::from(chain.chain_id.as_str());
        let pool_size = config.pool_size.max(1);

        // Spawn all tunnel establishes in parallel.
        let mut join_set = tokio::task::JoinSet::new();
        for idx in 0..pool_size {
            let cp = Arc::clone(&chain_protocols);
            let ch = chain.clone();
            let mid = mesh_id.clone();
            // Load a peer_table snapshot for the initial dial (Phase 8.5).
            let pt = peer_table.load_full();
            // Snapshot credential_params for this dial attempt.
            let cred_snap = credential_params.load_full();
            // Phase 4: pop one blind token for this dial (single-use).
            let spent = token_pool::pop_one(&token_pool);
            join_set.spawn(async move {
                let pre_minted = spent.as_ref().map(|t| {
                    debug!(epoch_id = t.epoch_id, "pool: spending blind token on initial dial");
                    mirage_protocol::driver::ClientCredential { scheme: 0x03, token: t.to_wire() }
                });
                let cred_ref = cred_snap.as_ref().as_ref();
                (idx, establish_raw_tunnel(&cp, &ch, &pt, &mid, pre_minted.as_ref(), cred_ref).await)
            });
        }

        // Wait for at least 2 warm slots before returning (or the whole pool if
        // smaller). The old 20% threshold returned after a single slot, so the
        // first request raced the background warm-up of the remaining slots and
        // frequently hit a cold/empty pool (#59). Two warm slots give the first
        // request a live tunnel plus a spare.
        let threshold = pool_size.min(2).max(1);
        let mut slots_partial: Vec<(usize, Slot)> = Vec::with_capacity(pool_size);
        let mut ready_count = 0usize;

        while let Some(result) = join_set.join_next().await {
            match result {
                Ok((idx, Ok(tunnel))) => {
                    slots_partial.push((idx, Slot::Idle { tunnel, last_used: Instant::now() }));
                    ready_count += 1;
                    info!(chain_id = %chain_id, slot = idx, ready = ready_count, threshold, "pool slot established");
                }
                Ok((idx, Err(e))) => {
                    warn!(chain_id = %chain_id, slot = idx, error = %e, "pool slot failed on startup");
                    slots_partial.push((idx, Slot::Reconnecting));
                }
                Err(e) => {
                    warn!(chain_id = %chain_id, error = %e, "pool slot join error");
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
            chain_id = %chain_id,
            ready = ready_count,
            total = pool_size,
            threshold,
            "pool ready (≥2-slot threshold), remaining slots warming in background"
        );

        let available = Arc::new(Notify::new());
        let inner = Arc::new(Mutex::new(PoolInner {
            slots,
            chain,
            chain_protocols,
            config,
            mesh_id,
            peer_table,
            credential_params,
            token_pool,
        }));

        let pool = Arc::new(Self {
            inner: Arc::clone(&inner),
            available: Arc::clone(&available),
            is_onion: AtomicBool::new(is_onion),
            peer_table_notify: Arc::new(Notify::new()),
            chain_id: Arc::clone(&chain_id),
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
            let chain_id_warmup = Arc::clone(&chain_id);
            tokio::spawn(async move {
                while let Some(result) = join_set.join_next().await {
                    match result {
                        Ok((idx, Ok(tunnel))) => {
                            let mut guard = inner_clone.lock().await;
                            if matches!(guard.slots.get(idx), Some(Slot::Reconnecting)) {
                                guard.slots[idx] =
                                    Slot::Idle { tunnel, last_used: Instant::now() };
                                info!(chain_id = %chain_id_warmup, slot = idx, "background warmup: slot established");
                                available_clone.notify_one();
                            }
                        }
                        Ok((idx, Err(e))) => {
                            warn!(chain_id = %chain_id_warmup, slot = idx, error = %e, "background warmup: slot failed");
                        }
                        Err(e) => {
                            warn!(chain_id = %chain_id_warmup, error = %e, "background warmup: join error");
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
                                chain_id = %self.chain_id,
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
                        debug!(chain_id = %self.chain_id, slot = idx, "leased tunnel from pool");
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
                    debug!(chain_id = %self.chain_id, "lease: woke dormant slots for reconnect");
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
        debug!(chain_id = %self.chain_id, slot = slot_idx, "tunnel returned to pool");
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
        debug!(chain_id = %self.chain_id, slot = slot_idx, "tunnel poisoned, slot marked for reconnect");
        // Notify to trigger the maintenance loop sooner.
        self.available.notify_one();
    }

    /// Signal that the shared peer_table has received a new generation.
    ///
    /// Wakes all sleeping reconnect tasks so they retry immediately with fresh
    /// peer data instead of waiting out their current exponential backoff delay.
    /// Also resets their attempt counter so they do not hit the max-attempts cap
    /// prematurely.
    ///
    /// Call this whenever `ArcSwap<PeerTable>::store(new_table)` is called on the
    /// table that was passed to [`TunnelPool::new`].
    pub fn notify_peer_table_change(&self) {
        self.peer_table_notify.notify_waiters();
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
                        chain_id = %pool.chain_id,
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
                chain_id = %pool.chain_id,
                slot = idx,
                idle_secs = TUNNEL_POOL_IDLE_KILL_TIMEOUT.as_secs(),
                "reaper: closing long-idle tunnel (→ Dormant)"
            );
            if let Err(e) = tunnel.close().await {
                debug!(chain_id = %pool.chain_id, slot = idx, error = %e, "reaper: tunnel close returned error (ignored)");
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
                        debug!(chain_id = %pool.chain_id, slot = idx, "idle timeout expired, reconnecting");
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
// ── Smoke tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod pool_smoke_tests {
    use mirage_core::config::{
        AuthMode, ChainConfig, HandshakeStrategy, HopConfig, HopRole, MirageEndpointConfig,
        ProtocolId, TcpProtocolId, TransportCamouflage, TransportKind,
    };
    use mirage_core::crypto::credential::CredentialParams;
    use mirage_protocol::{endpoint_protocol::EndpointProtocol, negotiation::ProtocolChain};
    use std::sync::Arc;

    /// Build a minimal `ProtocolChain` backed by a Blake3/TCP endpoint.
    fn minimal_chain_protocols() -> Arc<ProtocolChain> {
        let ep_cfg = MirageEndpointConfig {
            transport: TransportKind::Tcp,
            auth: AuthMode::Blake3,
            camouflage: TransportCamouflage::None,
            byte_cipher: None,
            byte_cipher_mode: None,
        };
        Arc::new(ProtocolChain::new(vec![Box::new(EndpointProtocol::new(
            ep_cfg,
            TcpProtocolId::Mirage,
            vec![],
        ))]))
    }

    /// Build a 2-hop onion `ChainConfig` whose hops have `public_key = None`
    /// and `node_id` set to the given ids.
    fn two_hop_chain(entry_id: &str, exit_id: &str) -> ChainConfig {
        ChainConfig {
            chain_id: "test-pool-smoke".into(),
            hops: vec![
                HopConfig {
                    addr: "127.0.0.1:19971".into(),
                    public_key: None,
                    node_id: Some(entry_id.into()),
                    sni: None,
                    role: HopRole::Entry,
                    display_name: None,
                    camouflage: None,
                    byte_cipher: None,
                    camo_server_pubkey: None,
                    relay_mode: mirage_core::mirage::wire::RelayMode::OnionPeel,
                },
                HopConfig {
                    addr: "127.0.0.1:19972".into(),
                    public_key: None,
                    node_id: Some(exit_id.into()),
                    sni: None,
                    role: HopRole::Exit,
                    display_name: None,
                    camouflage: None,
                    byte_cipher: None,
                    camo_server_pubkey: None,
                    relay_mode: mirage_core::mirage::wire::RelayMode::OnionPeel,
                },
            ],
            handshake: HandshakeStrategy::Ephemeral,
            reply: mirage_core::config::ReplyMode::Raw,
            protocol: ProtocolId::Tcp(TcpProtocolId::Mirage),
            priority: 0,
            shaping: Default::default(),
            camouflage: Default::default(),
            cover_traffic: Default::default(),
            socks5_bind: None,
            websocket_xray: None,
            shadowtls_v3: None,
            vless_reality: None,
            udp_hysteria2_native: None,
            aead_suite: mirage_core::crypto::AeadSuite::ChaCha20Poly1305,
            kill_switch: None,
            group: None,
        }
    }

    /// Empty peer_table → `establish_raw_tunnel` must fail with the specific
    /// "not in peer_table — node may not be registered yet" config error.
    ///
    /// This is the regression guard for the "Connection refused recovery race"
    /// bug: before Phase 9.6 the resolve step was missing, so the error was
    /// the generic "hop public_key must be 64 hex chars, got 0" from the onion
    /// builder, not the actionable peer_table message.
    #[tokio::test]
    async fn pool_empty_peer_table_returns_specific_error() {
        use mirage_core::mirage::mesh::PeerTable;
        use crate::tunnel::establish_raw_tunnel;

        let chain = two_hop_chain("v2-entry", "v2-exit");
        let pt = PeerTable::empty();
        let chain_protocols = minimal_chain_protocols();

        let result = establish_raw_tunnel(&chain_protocols, &chain, &pt, &[], None, None).await;

        assert!(result.is_err(), "must fail when peer_table is empty");
        let e = result.err().expect("just checked is_err");
        let msg = format!("{e:?}");
        assert!(
            msg.contains("not in peer_table"),
            "error must mention 'not in peer_table' (got): {msg}"
        );
        // Must NOT be the pre-Phase-9.6 config error about hex chars.
        assert!(
            !msg.contains("64 hex chars"),
            "must not be the old pubkey-validation error (got): {msg}"
        );
    }

    /// After `peer_table` contains a matching entry, `establish_raw_tunnel`
    /// must resolve pubkey and fail at network-level (connection refused),
    /// NOT at the config-validation stage.
    ///
    /// This proves that peer_table → pubkey resolve path works end-to-end and
    /// that the "must be 64 hex chars" config error is gone for valid entries.
    #[tokio::test]
    async fn pool_peer_table_resolve_passes_pubkey_validation() {
        use mirage_core::mirage::mesh::peer_table::{PeerTable, PeerTableEntry};
        use crate::tunnel::establish_raw_tunnel;

        let pubkey = "d".repeat(64);
        let pt = PeerTable {
            generation: 1,
            peers: vec![
                PeerTableEntry {
                    mesh_id: "test".into(),
                    responsible_monitor_id: String::new(),
                    node_id: "v2-entry".into(),
                    pubkey_hex: pubkey.clone(),
                    addr: "127.0.0.1:19971".into(),
                    labels: vec![],
                    updated_at_ms: 0,
                    aead_suite_preference: Vec::new(),
                },
                PeerTableEntry {
                    mesh_id: "test".into(),
                    responsible_monitor_id: String::new(),
                    node_id: "v2-exit".into(),
                    pubkey_hex: "e".repeat(64),
                    addr: "127.0.0.1:19972".into(),
                    labels: vec![],
                    updated_at_ms: 0,
                    aead_suite_preference: Vec::new(),
                },
            ],
        };

        let chain = two_hop_chain("v2-entry", "v2-exit");
        let chain_protocols = minimal_chain_protocols();

        let result = establish_raw_tunnel(&chain_protocols, &chain, &pt, &[], None, None).await;

        match result {
            Ok(_) => panic!("expected network-level error (no server on :19971/:19972)"),
            Err(e) => {
                let msg = format!("{e:?}");
                // Must NOT be a config error about "64 hex chars".
                assert!(
                    !msg.contains("64 hex chars"),
                    "pubkey resolution must have succeeded; got unexpected config error: {msg}"
                );
                // Must be network-level failure.
                assert!(
                    msg.contains("refused")
                        || msg.contains("timed out")
                        || msg.contains("Io")
                        || msg.contains("NodeUnreachable"),
                    "expected network-level error, got: {msg}"
                );
            }
        }
    }

    /// Recovery from reconnect-failed state: simulate that the pool starts with
    /// an empty peer_table (all slots fail → Reconnecting), then the peer_table
    /// is populated with correct entries. The next `establish_raw_tunnel` call
    /// (simulating what `reconnect_slot` would do) must resolve, not fail with
    /// the config error.
    ///
    /// We can't easily test pool internals without actually constructing a
    /// `TunnelPool` (which dials on startup), so we test the underlying
    /// `establish_raw_tunnel` function directly — the function that
    /// `reconnect_slot` calls — to confirm the state machine recovery path.
    #[tokio::test]
    async fn pool_recovery_after_peer_table_arrives() {
        use mirage_core::mirage::mesh::peer_table::{PeerTable, PeerTableEntry};
        use crate::tunnel::establish_raw_tunnel;

        let chain = two_hop_chain("v2-entry", "v2-exit");
        let chain_protocols = minimal_chain_protocols();

        // Step 1: empty peer_table — expect config error (simulates failed startup).
        let pt_empty = PeerTable::empty();
        let err = establish_raw_tunnel(&chain_protocols, &chain, &pt_empty, &[], None, None).await;
        assert!(err.is_err());
        let msg_empty = format!("{:?}", err.err().expect("just checked is_err"));
        assert!(
            msg_empty.contains("not in peer_table"),
            "startup fail must cite peer_table absence: {msg_empty}"
        );

        // Step 2: peer_table arrives (simulates monitor push via ArcSwap.store).
        let pt_full = PeerTable {
            generation: 2,
            peers: vec![
                PeerTableEntry {
                    mesh_id: "test".into(),
                    responsible_monitor_id: String::new(),
                    node_id: "v2-entry".into(),
                    pubkey_hex: "f".repeat(64),
                    addr: "127.0.0.1:19971".into(),
                    labels: vec![],
                    updated_at_ms: 0,
                    aead_suite_preference: Vec::new(),
                },
                PeerTableEntry {
                    mesh_id: "test".into(),
                    responsible_monitor_id: String::new(),
                    node_id: "v2-exit".into(),
                    pubkey_hex: "0".repeat(64),
                    addr: "127.0.0.1:19972".into(),
                    labels: vec![],
                    updated_at_ms: 0,
                    aead_suite_preference: Vec::new(),
                },
            ],
        };

        // Step 3: retry with filled peer_table — must reach network, not config error.
        let result = establish_raw_tunnel(&chain_protocols, &chain, &pt_full, &[], None, None).await;
        match result {
            Ok(_) => panic!("expected network error, no server running on test ports"),
            Err(e) => {
                let msg = format!("{e:?}");
                assert!(
                    !msg.contains("64 hex chars"),
                    "after peer_table arrives, config error must not reappear: {msg}"
                );
                assert!(
                    msg.contains("refused")
                        || msg.contains("timed out")
                        || msg.contains("Io")
                        || msg.contains("NodeUnreachable"),
                    "expected network-level error on retry, got: {msg}"
                );
            }
        }
    }

    // ── Stale-slot lockup reproducer (Track 2 fix, May 2026) ─────────────────

    /// `notify_peer_table_change()` must wake a task that is blocked on
    /// `peer_table_notify.notified()`.
    ///
    /// This is the core mechanism of the stale-slot lockup fix: reconnect tasks
    /// that hit `max_attempts` block on `peer_table_notify` instead of exiting,
    /// and `notify_peer_table_change()` unblocks them so they retry immediately
    /// with fresh peer data.
    #[tokio::test]
    async fn notify_peer_table_change_wakes_blocked_reconnect() {
        use super::TunnelPool;
        use arc_swap::ArcSwap;
        use mirage_core::{config::TunnelPoolConfig, mirage::mesh::PeerTable};
        use std::sync::Arc;

        // Build a pool with empty peer_table.  pool_size=1 so only one slot
        // is created; it will fail to connect and enter Reconnecting state.
        let chain = two_hop_chain("node-a", "node-b");
        let chain_protocols = minimal_chain_protocols();
        let peer_table = Arc::new(ArcSwap::from_pointee(PeerTable::empty()));
        let config = TunnelPoolConfig {
            pool_size: 1,
            // max_reconnect_attempts=1 forces slot to hit the cap quickly and
            // wait on peer_table_notify.
            max_reconnect_attempts: 1,
            reconnect_base_ms: 10,
            idle_timeout_secs: 300,
        };

        // Pool construction will fail all slots (empty peer_table) and put them
        // in Reconnecting.  That is expected — we just need the pool object.
        let pool = TunnelPool::new(
            chain,
            chain_protocols,
            config,
            vec![],
            Arc::clone(&peer_table),
            Arc::new(ArcSwap::from_pointee(None::<CredentialParams>)),
            Arc::new(ArcSwap::from_pointee(crate::token_pool::TokenPool::default())),
        )
        .await
        .expect("TunnelPool::new must not fail during construction");

        // Subscribe to the notify *before* calling notify_peer_table_change
        // so we don't miss the signal.
        let notified = {
            let n = Arc::clone(&pool.peer_table_notify);
            tokio::spawn(async move {
                n.notified().await;
            })
        };

        // Give the spawned task a moment to reach `.notified().await`.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        pool.notify_peer_table_change();

        // Task must complete within 500 ms.
        tokio::time::timeout(
            std::time::Duration::from_millis(500),
            notified,
        )
        .await
        .expect("notify_peer_table_change must wake blocked task within 500 ms")
        .expect("task must not panic");
    }

    /// `notify_peer_table_change()` with no subscribers must not panic or block.
    #[tokio::test]
    async fn notify_peer_table_change_no_subscribers_is_noop() {
        use super::TunnelPool;
        use arc_swap::ArcSwap;
        use mirage_core::{config::TunnelPoolConfig, mirage::mesh::PeerTable};
        use std::sync::Arc;

        let chain = two_hop_chain("node-c", "node-d");
        let chain_protocols = minimal_chain_protocols();
        let peer_table = Arc::new(ArcSwap::from_pointee(PeerTable::empty()));
        let config = TunnelPoolConfig {
            pool_size: 1,
            max_reconnect_attempts: 1,
            reconnect_base_ms: 10,
            idle_timeout_secs: 300,
        };

        let pool = TunnelPool::new(
            chain,
            chain_protocols,
            config,
            vec![],
            peer_table,
            Arc::new(ArcSwap::from_pointee(None::<CredentialParams>)),
            Arc::new(ArcSwap::from_pointee(crate::token_pool::TokenPool::default())),
        )
        .await
        .expect("TunnelPool::new must not fail during construction");

        // Must not panic or block.
        pool.notify_peer_table_change();
        pool.notify_peer_table_change();
    }

}

async fn reconnect_slot(pool: Arc<TunnelPool>, slot_idx: usize) {
    let chain_id = Arc::clone(&pool.chain_id);
    let (max_attempts, base_ms, chain, chain_protocols, mesh_id, peer_table_shared,
         peer_table_notify, credential_params_shared, token_pool_shared) = {
        let guard = pool.inner.lock().await;
        (
            guard.config.max_reconnect_attempts,
            guard.config.reconnect_base_ms,
            guard.chain.clone(),
            Arc::clone(&guard.chain_protocols),
            guard.mesh_id.clone(),
            Arc::clone(&guard.peer_table),
            Arc::clone(&pool.peer_table_notify),
            Arc::clone(&guard.credential_params),
            Arc::clone(&guard.token_pool),
        )
    };

    let is_onion = chain.hops.len() > 1;
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        debug!(chain_id = %chain_id, slot = slot_idx, attempt, is_onion, hops = chain.hops.len(), "reconnect attempt");

        // Load a fresh peer_table snapshot on each attempt so we pick up
        // any updates that arrived between retries (including the transition
        // from empty → populated when the monitor link first delivers peers).
        let peer_table_snap = peer_table_shared.load_full();

        // Freshen the chain's hop pubkeys from the live peer_table.  This is
        // necessary for ephemeral-key rotation recovery: when a node restarts its
        // X25519 keypair changes and the peer_table is updated, but the chain
        // stored in pool.inner.chain still carries the old static public_key.
        // freshen_chain_pubkeys() overrides any stale static key with the live
        // peer_table entry whenever node_id is set — fixing UDP (and TCP) pools
        // after rotation without requiring switch_chain or a full worker restart.
        let fresh_chain = match freshen_chain_pubkeys(&chain, &peer_table_snap) {
            Ok(c) => c,
            Err(e) => {
                // peer_table not populated yet — fall back to original chain and
                // let the dial fail with the usual "not in peer_table" error so
                // the retry/backoff logic handles it normally.
                debug!(chain_id = %chain_id, slot = slot_idx, error = %e, "freshen_chain_pubkeys: using original chain");
                chain.clone()
            }
        };
        let cred_snap = credential_params_shared.load_full();
        let cred_ref = cred_snap.as_ref().as_ref();
        // Phase 4: pop one blind token for this reconnect dial.
        // A popped token must not be re-inserted on failure — it may have reached
        // the node's SpentSet and would be rejected as a replay on any retry.
        let spent = token_pool::pop_one(&token_pool_shared);
        if spent.is_none() {
            // Warn once if credential_params advertises a BlindRsaBatch authority —
            // an empty pool means dials will be rejected by enforce-mode nodes.
            let creds = credential_params_shared.load();
            if let Some(cp) = creds.as_ref() {
                let has_blind = cp.authorities.iter().any(|a| {
                    a.scheme == mirage_core::mirage::wire::CredentialScheme::BlindRsaBatch
                });
                if has_blind {
                    warn!(
                        chain_id = %chain_id,
                        slot = slot_idx,
                        "token pool empty but BlindRsaBatch authority is active — \
                         dials may be rejected by enforce-mode nodes"
                    );
                }
            }
        }
        let pre_minted = spent.as_ref().map(|t| {
            debug!(chain_id = %chain_id, slot = slot_idx, epoch_id = t.epoch_id, "pool: spending blind token on reconnect");
            mirage_protocol::driver::ClientCredential { scheme: 0x03, token: t.to_wire() }
        });
        match establish_raw_tunnel(&chain_protocols, &fresh_chain, &peer_table_snap, &mesh_id, pre_minted.as_ref(), cred_ref).await {
            Ok(tunnel) => {
                info!(chain_id = %chain_id, slot = slot_idx, attempt, "reconnect succeeded");
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
                        chain_id = %chain_id,
                        slot = slot_idx,
                        attempt,
                        is_onion,
                        error = %e,
                        "reconnect gave up after max attempts — waiting for peer_table change"
                    );
                    // Block until a peer_table update arrives, then restart the
                    // attempt counter.  This prevents permanent lockup when the
                    // peer_table was empty at dial time and arrives later.
                    peer_table_notify.notified().await;
                    info!(chain_id = %chain_id, slot = slot_idx, "peer_table changed — resetting attempt counter");
                    attempt = 0;
                    continue;
                }
                let delay_ms = base_ms.saturating_mul(1u64 << attempt.min(10));
                warn!(
                    chain_id = %chain_id,
                    slot = slot_idx,
                    attempt,
                    is_onion,
                    delay_ms,
                    error = %e,
                    "reconnect failed, retrying"
                );
                // Sleep for the backoff delay, but wake early if the peer_table
                // is updated — stale-slot lockup fix (May 2026).
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => {}
                    _ = peer_table_notify.notified() => {
                        info!(
                            chain_id = %chain_id,
                            slot = slot_idx,
                            attempt,
                            "peer_table changed during backoff — retrying immediately"
                        );
                        // Reset attempt so fresh peer data gets a clean counter.
                        attempt = 0;
                    }
                }
            }
        }
    }
}
