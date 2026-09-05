use std::sync::{
    atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Instant;

use arc_swap::ArcSwap;
use mirage_core::{
    config::{ChainConfig, TunnelPoolConfig},
    crypto::credential::CredentialParams,
    error::MirageError,
    mirage::mesh::PeerTable,
    mirage::wire::CredentialScheme,
};
use mirage_mux::MuxError;
use mirage_mux::tokio_compat::{SessionHandle, TokioMuxStream};
use mirage_protocol::{negotiation::ProtocolChain, traits::Tunnel};
use tokio::sync::Semaphore;
use tracing::{debug, error, info, warn};

use crate::relay::TunnelCompat;
use crate::token_pool::{self, TokenPool};
use crate::tunnel::{establish_mux_tunnel, freshen_chain_pubkeys};

// ── Cold-start credential gate ────────────────────────────────────────────────

/// Maximum time to wait for a credential to become ready on the first dial.
/// 30s (see pool.rs CREDENTIAL_GATE_TIMEOUT): cli2 MonitorLink+relay latency can
/// reach ~11s, so an 8s cap timed out before the ring slot arrived.
const MUX_CREDENTIAL_GATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Poll interval while waiting for credential readiness.
const MUX_CREDENTIAL_GATE_POLL_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(200);

/// Returns `true` when a usable credential is available for the active scheme.
/// Mirrors the same logic in `pool.rs` for MuxPool.
fn mux_credential_is_ready(
    credential_params: &ArcSwap<Option<CredentialParams>>,
    token_pool: &ArcSwap<TokenPool>,
    ring_credential: &crate::token_pool::RingCredentialCell,
) -> bool {
    let params_guard = credential_params.load();
    let params = match params_guard.as_ref().as_ref() {
        Some(p) => p,
        None => return false,
    };

    let primary_scheme = params
        .authorities
        .iter()
        .map(|a| a.scheme)
        .find(|&s| s != CredentialScheme::Legacy);

    match primary_scheme {
        Some(CredentialScheme::RingMembership) => {
            ring_credential.load().as_ref().is_some()
        }
        Some(CredentialScheme::BlindRsaBatch) => {
            !token_pool.load().is_empty()
        }
        Some(CredentialScheme::EpochHmac)
        | Some(CredentialScheme::SignedCapability)
        | Some(CredentialScheme::Legacy)
        | None => true,
    }
}

/// Wait (bounded) for the mux session credential to become ready.
/// Only called for the initial pool slot construction, not reconnects.
async fn mux_wait_for_credential_ready(
    chain_id: &str,
    slot: usize,
    credential_params: &ArcSwap<Option<CredentialParams>>,
    token_pool: &ArcSwap<TokenPool>,
    ring_credential: &crate::token_pool::RingCredentialCell,
) {
    if mux_credential_is_ready(credential_params, token_pool, ring_credential) {
        return;
    }

    let scheme_name = {
        let params_guard = credential_params.load();
        match params_guard.as_ref().as_ref() {
            Some(p) => p
                .authorities
                .iter()
                .map(|a| a.scheme)
                .find(|&s| s != CredentialScheme::Legacy)
                .map(|s| s.name())
                .unwrap_or("unknown"),
            None => "credential_params",
        }
    };

    debug!(
        chain_id,
        slot,
        scheme = scheme_name,
        timeout_ms = MUX_CREDENTIAL_GATE_TIMEOUT.as_millis(),
        "mux cold-start gate: waiting for credential readiness before first dial"
    );

    let deadline = tokio::time::Instant::now() + MUX_CREDENTIAL_GATE_TIMEOUT;
    loop {
        tokio::time::sleep(MUX_CREDENTIAL_GATE_POLL_INTERVAL).await;
        if mux_credential_is_ready(credential_params, token_pool, ring_credential) {
            debug!(
                chain_id,
                slot,
                scheme = scheme_name,
                "mux cold-start gate: credential ready, proceeding to dial"
            );
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            warn!(
                chain_id,
                slot,
                scheme = scheme_name,
                "mux cold-start gate: timed out waiting for credential -- proceeding to dial anyway"
            );
            return;
        }
    }
}

// ── Credential-rejected retry cap ────────────────────────────────────────────

/// Maximum sleep interval (ms) between retries when every mux session dial
/// fails with CREDENTIAL_REJECTED on a credential-gated pool.  The slot never
/// permanently parks in this case -- it keeps retrying at this capped interval
/// until the ring credential hot-swap lands.
const MUX_CREDENTIAL_REJECTED_RETRY_CAP_MS: u64 = 20_000;

// ── Park-with-timeout knob (shared with pool.rs semantics) ───────────────────

/// Reads the same `MIRAGE_SLOT_PARK_TIMEOUT_MS` env var used by TunnelPool so
/// both pool types behave identically when a slot parks after max attempts
/// (bug 5 / F2 fix for the mux path).
fn mux_slot_park_timeout() -> std::time::Duration {
    let ms = std::env::var("MIRAGE_SLOT_PARK_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(15_000);
    std::time::Duration::from_millis(ms)
}

// ── Circuit breaker state constants ──────────────────────────────────────────
const CIRCUIT_CLOSED: u8 = 0;
const CIRCUIT_OPEN: u8 = 1;
const CIRCUIT_HALF_OPEN: u8 = 2;

/// Returns current time as milliseconds since the Unix epoch (for CB wake-grace).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Per-pool circuit breaker.
///
/// Transitions:
/// - Closed → Open after `failure_threshold` consecutive failures.
/// - Open → Half-Open after `open_duration_secs` elapsed since last failure.
/// - Half-Open → Closed on first success (resets counter).
/// - Half-Open → Open on failure.
///
/// Wake-grace suppression: after dormant→reconnecting wake, `wake_grace_until_ms`
/// is set 30s ahead. `is_open()` returns false during this window even if the
/// failure counter is high — prevents stale failures from blocking recovery.
struct CircuitBreaker {
    state: AtomicU8,
    consecutive_failures: AtomicU64,
    /// Protected by a plain Mutex — only held for nanoseconds (no await inside).
    last_failure: Mutex<Option<Instant>>,
    open_duration_secs: u64,
    failure_threshold: u64,
    /// ms-epoch deadline until which `is_open()` always returns false.
    /// Set by `note_wake()` to suppress stale CB state after dormant→reconnecting.
    wake_grace_until_ms: AtomicU64,
    /// Chain name for log messages.
    chain_label: String,
}

impl CircuitBreaker {
    fn new(chain_label: String, failure_threshold: u64, open_duration_secs: u64) -> Self {
        Self {
            state: AtomicU8::new(CIRCUIT_CLOSED),
            consecutive_failures: AtomicU64::new(0),
            last_failure: Mutex::new(None),
            open_duration_secs,
            failure_threshold,
            wake_grace_until_ms: AtomicU64::new(0),
            chain_label,
        }
    }

    /// Returns `true` if the circuit is currently blocking requests.
    ///
    /// Also drives the Open → Half-Open transition by time.
    fn is_open(&self) -> bool {
        // Wake-grace suppression: if we just woke dormant slots, bypass CB check
        // for 30s so stale failures don't block the recovery window.
        let now = now_ms();
        if now < self.wake_grace_until_ms.load(Ordering::Relaxed) {
            return false;
        }

        let state = self.state.load(Ordering::Acquire);
        match state {
            CIRCUIT_CLOSED => false,
            CIRCUIT_HALF_OPEN => false,
            CIRCUIT_OPEN => {
                // Check if cool-down has elapsed; if so transition to half-open.
                let elapsed = {
                    let guard = self.last_failure.lock()
                        .expect("circuit breaker mutex is not poisoned");
                    guard.map(|t| t.elapsed().as_secs()).unwrap_or(0)
                };
                if elapsed >= self.open_duration_secs {
                    // CAS to half-open; another thread may beat us, that's fine.
                    let _ = self.state.compare_exchange(
                        CIRCUIT_OPEN,
                        CIRCUIT_HALF_OPEN,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    info!(chain = %self.chain_label, "circuit breaker half-open: probing");
                    false
                } else {
                    true
                }
            }
            _ => false,
        }
    }

    /// Call immediately after waking dormant slots to reconnecting.
    /// Suppresses CB for 30s so recovery is not aborted by stale failure counter.
    fn note_wake(&self) {
        self.wake_grace_until_ms.store(now_ms() + 30_000, Ordering::Relaxed);
        debug!(chain = %self.chain_label, "circuit breaker wake-grace set: 30s suppression");
    }

    /// Call after a successful connection attempt.
    fn record_success(&self) {
        let prev = self.state.swap(CIRCUIT_CLOSED, Ordering::AcqRel);
        self.consecutive_failures.store(0, Ordering::Release);
        if prev != CIRCUIT_CLOSED {
            info!(chain = %self.chain_label, "circuit breaker closed: chain recovered");
        }
    }

    /// Call after a failed connection attempt.
    fn record_failure(&self) {
        {
            let mut guard = self.last_failure.lock()
                .expect("circuit breaker mutex is not poisoned");
            *guard = Some(Instant::now());
        }
        let failures = self.consecutive_failures.fetch_add(1, Ordering::AcqRel) + 1;
        let current = self.state.load(Ordering::Acquire);
        if current == CIRCUIT_HALF_OPEN {
            self.state.store(CIRCUIT_OPEN, Ordering::Release);
            info!(chain = %self.chain_label, "circuit breaker re-opened: probe failed");
        } else if current == CIRCUIT_CLOSED && failures >= self.failure_threshold {
            self.state.store(CIRCUIT_OPEN, Ordering::Release);
            info!(
                chain = %self.chain_label,
                consecutive_failures = failures,
                "circuit breaker opened: too many failures"
            );
        }
    }
}

// ── Reaper constants ──────────────────────────────────────────────────────────

/// How often the mux idle reaper wakes.
const MUX_POOL_IDLE_REAPER_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(60);

/// Mux sessions with no stream opened in this window are closed by the reaper.
/// Longer than the tunnel reaper (600 s vs 300 s) because re-establishing a mux
/// session is more expensive than a plain TCP tunnel.
const MUX_POOL_IDLE_KILL_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(600);

// ── Pool internals ────────────────────────────────────────────────────────────

enum SessionSlot {
    /// Live session with timestamp of last successfully opened stream.
    Active {
        session: Arc<SessionHandle>,
        /// When a stream was last opened on this session.
        last_open_stream_at: Instant,
    },
    /// Background task is re-establishing this slot.
    Reconnecting,
    /// Reaped by the idle reaper: session was closed, no reconnect in flight.
    /// Woken to `Reconnecting` on next `open_stream()` call when no `Active`
    /// slots remain.
    Dormant,
}

struct PoolInner {
    slots: Vec<SessionSlot>,
    chain: ChainConfig,
    chain_protocols: Arc<ProtocolChain>,
    config: TunnelPoolConfig,
    mesh_id: Vec<u8>,
    /// Live peer table shared with the monitor link (Phase 8.5).
    peer_table: Arc<ArcSwap<PeerTable>>,
    /// Operator-issued credential params pushed by MonitorLink.
    /// Shared via Arc::clone — pool snapshots it on each dial and mints a fresh
    /// per-dial token.  None = credential feature inactive (tolerant mode).
    credential_params: Arc<ArcSwap<Option<CredentialParams>>>,
    /// Blind-RSA token pool (Phase 4 spend).  Each mux session dial pops one
    /// token.  When empty, falls back to EpochHmac or no credential.
    token_pool: Arc<ArcSwap<TokenPool>>,
    /// Ring-membership (scheme 0x04) credential material. Static for process
    /// lifetime — doesn't deplete. Preferred over the blind token pool when a
    /// RingMembership authority is active in `credential_params`. Hot-swappable
    /// via ArcSwap — MonitorLink writes a new credential on epoch rotation.
    ring_credential: crate::token_pool::RingCredentialCell,
    /// Whether this pool requires a credential to be present before dialing.
    /// When `true`, CREDENTIAL_REJECTED errors are treated as transient and the
    /// slot keeps retrying at a capped interval instead of permanently parking.
    credential_gated: bool,
}

/// Semaphore capacity for the post-recovery burst limiter.
const RECONNECT_BURST_LIMIT: usize = 10;

pub struct MuxPool {
    inner: Arc<tokio::sync::Mutex<PoolInner>>,
    next_idx: AtomicUsize,
    pub is_onion: AtomicBool,
    stream_notify: Arc<tokio::sync::Notify>,
    /// Notified when a session is marked Reconnecting so maintenance wakes early.
    reconnect_notify: Arc<tokio::sync::Notify>,
    circuit_breaker: Arc<CircuitBreaker>,
    /// Limits burst of requests immediately after recovery to avoid thundering herd.
    reconnect_limiter: Arc<Semaphore>,
    /// Notified when the shared peer_table receives a new generation.
    ///
    /// Sleeping reconnect tasks subscribe to this so they wake immediately
    /// and retry with fresh peer data instead of waiting out exponential backoff.
    peer_table_notify: Arc<tokio::sync::Notify>,
    /// Cached chain ID for zero-cost log fields (avoids locking inner mutex).
    chain_id: Arc<str>,
}

impl MuxPool {
    /// `peer_table` — shared live peer table from the monitor link (Phase 8.5).
    /// Pass `Arc::new(ArcSwap::from_pointee(PeerTable::empty()))` when there
    /// is no monitor link.
    ///
    /// `token_pool` — shared blind-RSA token pool (Phase 4).  Pass
    /// `Arc::new(ArcSwap::from_pointee(TokenPool::default()))` when no
    /// MonitorLink is present.
    ///
    /// `ring_credential` — hot-swappable ring-membership (0x04) credential cell.
    /// Pass `Arc::new(ArcSwap::from_pointee(None))` when ring membership is not
    /// configured. MonitorLink writes into the same cell on epoch rotation.
    ///
    /// `credential_gated` — when `true`, each slot's FIRST dial waits (bounded
    /// by ~8 s) until the active credential scheme has a presentable credential
    /// before opening the mux session. Set `true` when `operator_pub_hex` is
    /// present in the client config. Set `false` for non-gated deployments.
    pub async fn new(
        chain: ChainConfig,
        chain_protocols: Arc<ProtocolChain>,
        config: TunnelPoolConfig,
        mesh_id: Vec<u8>,
        peer_table: Arc<ArcSwap<PeerTable>>,
        credential_params: Arc<ArcSwap<Option<CredentialParams>>>,
        token_pool: Arc<ArcSwap<TokenPool>>,
        ring_credential: crate::token_pool::RingCredentialCell,
        credential_gated: bool,
    ) -> Result<Arc<Self>, MirageError> {
        // Phase 9.7: onion = multi-hop chain. Single-hop direct exit
        // sends the address bytes raw (no length-prefix) so the
        // server's `handle_exit` auto-detector reads the first byte
        // as ASCII (>=0x30), not 0x00 (mux marker).
        let is_onion = chain.hops.len() > 1;
        let pool_size = config.pool_size.max(1).min(8);
        let chain_id: Arc<str> = Arc::from(chain.chain_id.as_str());

        let chain_label = chain
            .hops
            .first()
            .map(|h| h.addr.clone())
            .unwrap_or_else(|| "default".to_string());

        let mut join_set = tokio::task::JoinSet::new();
        for idx in 0..pool_size {
            let cp = Arc::clone(&chain_protocols);
            let ch = chain.clone();
            let mid = mesh_id.clone();
            // Shared arcs captured for the gate and the subsequent dial.
            let pt_arc = Arc::clone(&peer_table);
            let cred_params_arc = Arc::clone(&credential_params);
            let token_pool_arc = Arc::clone(&token_pool);
            let ring_cell_arc = Arc::clone(&ring_credential);
            let chain_id_log: Arc<str> = Arc::clone(&chain_id);
            let gated = credential_gated;
            join_set.spawn(async move {
                // Cold-start credential gate: wait (bounded) for the active
                // credential scheme to be presentable before the first mux dial.
                // Reconnects do NOT pass through this gate.
                if gated {
                    mux_wait_for_credential_ready(
                        &chain_id_log,
                        idx,
                        &cred_params_arc,
                        &token_pool_arc,
                        &ring_cell_arc,
                    ).await;
                }

                // Load a fresh peer_table snapshot AFTER the gate.
                let pt = pt_arc.load_full();
                // Snapshot credential_params for this dial attempt.
                let cred_snap = cred_params_arc.load_full();
                // Phase 4: pop one blind token for this initial mux session dial.
                let spent = token_pool::pop_one(&token_pool_arc);
                // Prefer ring-membership (0x04) when a RingMembership authority is
                // active; fall back to the blind token (0x03) otherwise.
                let ring_snap = ring_cell_arc.load_full();
                let pre_minted = crate::token_pool::build_ring_credential(
                    cred_snap.as_ref().as_ref(),
                    ring_snap.as_ref().as_ref(),
                    &crate::token_pool::ring_dial_context(&ch),
                ).or_else(|| spent.as_ref().map(|t| {
                    debug!(epoch_id = t.epoch_id, "mux_pool: spending blind token on initial dial");
                    mirage_protocol::driver::ClientCredential { scheme: 0x03, token: t.to_wire() }
                }));
                let cred_ref = cred_snap.as_ref().as_ref();
                (idx, build_session(&cp, &ch, &pt, &mid, pre_minted.as_ref(), cred_ref).await)
            });
        }

        // Wait for at least 2 warm slots before returning (mirrors TunnelPool #59 fix).
        let threshold = pool_size.min(2).max(1);
        let mut slots: Vec<(usize, SessionSlot)> = Vec::with_capacity(pool_size);
        let mut ready_count = 0usize;

        while let Some(result) = join_set.join_next().await {
            match result {
                Ok((idx, Ok(session))) => {
                    info!(chain_id = %chain_id, slot = idx, ready = ready_count + 1, threshold, "mux session established");
                    slots.push((idx, SessionSlot::Active {
                        session: Arc::new(session),
                        last_open_stream_at: Instant::now(),
                    }));
                    ready_count += 1;
                }
                Ok((idx, Err(e))) => {
                    warn!(chain_id = %chain_id, slot = idx, error = %e, "mux session failed on startup, marking reconnecting");
                    slots.push((idx, SessionSlot::Reconnecting));
                }
                Err(e) => {
                    warn!(chain_id = %chain_id, error = %e, "mux session join error");
                }
            }
            if ready_count >= threshold && !join_set.is_empty() {
                break;
            }
        }

        let collected: std::collections::HashSet<usize> = slots.iter().map(|(idx, _)| *idx).collect();
        for idx in 0..pool_size {
            if !collected.contains(&idx) {
                slots.push((idx, SessionSlot::Reconnecting));
            }
        }

        slots.sort_by_key(|(idx, _)| *idx);
        let slots: Vec<SessionSlot> = slots.into_iter().map(|(_, s)| s).collect();

        let inner = Arc::new(tokio::sync::Mutex::new(PoolInner {
            slots,
            chain,
            chain_protocols,
            config,
            mesh_id,
            peer_table,
            credential_params,
            token_pool,
            ring_credential,
            credential_gated,
        }));

        let pool = Arc::new(Self {
            inner,
            next_idx: AtomicUsize::new(0),
            is_onion: AtomicBool::new(is_onion),
            stream_notify: Arc::new(tokio::sync::Notify::new()),
            reconnect_notify: Arc::new(tokio::sync::Notify::new()),
            circuit_breaker: Arc::new(CircuitBreaker::new(
                chain_label,
                10, // open after 10 consecutive failures (was 5)
                30, // stay open for 30 seconds (was 5)
            )),
            reconnect_limiter: Arc::new(Semaphore::new(RECONNECT_BURST_LIMIT)),
            peer_table_notify: Arc::new(tokio::sync::Notify::new()),
            chain_id,
        });

        let pool_weak = Arc::downgrade(&pool);
        let reconnect_notify_clone = Arc::clone(&pool.reconnect_notify);
        tokio::spawn(async move {
            mux_maintenance_loop(pool_weak, reconnect_notify_clone).await;
        });

        // Spawn the idle reaper (closes sessions idle > MUX_POOL_IDLE_KILL_TIMEOUT).
        let pool_weak_reaper = Arc::downgrade(&pool);
        tokio::spawn(async move {
            mux_reaper_loop(pool_weak_reaper).await;
        });

        if !join_set.is_empty() {
            let inner_clone = Arc::clone(&pool.inner);
            let notify_clone = Arc::clone(&pool.stream_notify);
            let chain_id_warmup = Arc::clone(&pool.chain_id);
            tokio::spawn(async move {
                while let Some(result) = join_set.join_next().await {
                    match result {
                        Ok((idx, Ok(session))) => {
                            let mut guard = inner_clone.lock().await;
                            if matches!(guard.slots.get(idx), Some(SessionSlot::Reconnecting)) {
                                guard.slots[idx] = SessionSlot::Active {
                                    session: Arc::new(session),
                                    last_open_stream_at: Instant::now(),
                                };
                                info!(chain_id = %chain_id_warmup, slot = idx, "mux background warmup: session established");
                                drop(guard);
                                notify_clone.notify_waiters();
                            }
                        }
                        Ok((idx, Err(e))) => {
                            warn!(chain_id = %chain_id_warmup, slot = idx, error = %e, "mux background warmup: session failed");
                        }
                        Err(e) => {
                            warn!(chain_id = %chain_id_warmup, error = %e, "mux background warmup: join error");
                        }
                    }
                }
            });
        }

        Ok(pool)
    }

    pub async fn open_stream(&self) -> Result<TokioMuxStream, MirageError> {
        // Fast-reject if circuit is open.
        if self.circuit_breaker.is_open() {
            debug!(chain_id = %self.chain_id, "circuit breaker open: rejecting stream request immediately");
            return Err(MirageError::CircuitOpen);
        }

        // Acquire a burst-limiter permit so post-recovery load is spread.
        // `try_acquire` succeeds immediately while permits are available;
        // when all RECONNECT_BURST_LIMIT slots are taken, new requests wait
        // for one to be released. We hold the permit only until the stream is
        // obtained (not for the relay duration), so it turns over quickly.
        let _permit = self.reconnect_limiter.acquire().await
            .map_err(|_| MirageError::PoolExhausted)?;

        let pool_size = {
            let guard = self.inner.lock().await;
            guard.slots.len()
        };

        // ── Phase 1: snapshot active sessions ────────────────────────────────
        let initial_sessions: Vec<(usize, Arc<SessionHandle>)> = {
            let start = self.next_idx.fetch_add(1, Ordering::Relaxed) % pool_size;
            let guard = self.inner.lock().await;
            (0..pool_size)
                .map(|i| (start + i) % pool_size)
                .filter_map(|idx| match &guard.slots[idx] {
                    SessionSlot::Active { session, .. } => Some((idx, Arc::clone(session))),
                    SessionSlot::Reconnecting | SessionSlot::Dormant => None,
                })
                .collect()
        };

        // ── Phase 2: wake dormant slots if pool is empty ──────────────────────
        // Wake BEFORE the wait loop so the spawned reconnect task can populate
        // sessions while we wait on stream_notify.
        if initial_sessions.is_empty() {
            let mut guard = self.inner.lock().await;
            let mut woke = false;
            for slot in guard.slots.iter_mut() {
                if matches!(slot, SessionSlot::Dormant) {
                    *slot = SessionSlot::Reconnecting;
                    woke = true;
                }
            }
            if woke {
                debug!(chain_id = %self.chain_id, "open_stream: woke dormant mux sessions for reconnect");
                self.circuit_breaker.note_wake();
                self.reconnect_notify.notify_one();
            }
        }

        // ── Phase 3: try to open a stream on available sessions ───────────────
        // If we had active sessions up front, try them now.
        // If not (all dormant/reconnecting), skip to the 15s wait loop below.
        let mut had_active_sessions = !initial_sessions.is_empty();

        if had_active_sessions {
            let mut marked_reconnecting = false;
            for (idx, session) in &initial_sessions {
                let idx = *idx;
                if session.is_closed() {
                    let mut guard = self.inner.lock().await;
                    guard.slots[idx] = SessionSlot::Reconnecting;
                    debug!(chain_id = %self.chain_id, slot = idx, "mux session closed, marked reconnecting");
                    marked_reconnecting = true;
                    continue;
                }

                match session.open_stream().await {
                    Ok(stream) => {
                        debug!(chain_id = %self.chain_id, slot = idx, "opened mux stream");
                        {
                            let mut guard = self.inner.lock().await;
                            if let Some(SessionSlot::Active { last_open_stream_at, .. }) =
                                guard.slots.get_mut(idx)
                            {
                                *last_open_stream_at = Instant::now();
                            }
                        }
                        self.circuit_breaker.record_success();
                        return Ok(TokioMuxStream(stream));
                    }
                    Err(MuxError::Closed) | Err(MuxError::TooManyStreams) => {
                        debug!(chain_id = %self.chain_id, slot = idx, "session not available for new stream, trying next");
                    }
                    Err(e) => {
                        warn!(chain_id = %self.chain_id, slot = idx, error = %e, "open_stream error, trying next");
                    }
                }
            }
            if marked_reconnecting {
                self.reconnect_notify.notify_one();
            }
            // All active sessions were closed/busy — treat as empty for the wait below.
            had_active_sessions = false;
            let _ = had_active_sessions; // suppress lint; variable used for clarity
        }

        // ── Phase 4: 15s wake-and-wait loop ─────────────────────────────────
        // Onion mux cold-dial takes 2-5s; we wait up to 15s for stream_notify
        // (fired by reconnect_mux_slot on success) before giving up.
        let total_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            // Re-snapshot active sessions.
            let start = self.next_idx.fetch_add(1, Ordering::Relaxed) % pool_size;
            let sessions: Vec<(usize, Arc<SessionHandle>)> = {
                let guard = self.inner.lock().await;
                (0..pool_size)
                    .map(|i| (start + i) % pool_size)
                    .filter_map(|idx| match &guard.slots[idx] {
                        SessionSlot::Active { session, .. } => Some((idx, Arc::clone(session))),
                        SessionSlot::Reconnecting | SessionSlot::Dormant => None,
                    })
                    .collect()
            };

            if !sessions.is_empty() {
                let mut marked_reconnecting = false;
                for (idx, session) in sessions {
                    if session.is_closed() {
                        let mut guard = self.inner.lock().await;
                        guard.slots[idx] = SessionSlot::Reconnecting;
                        debug!(chain_id = %self.chain_id, slot = idx, "mux session closed, marked reconnecting");
                        marked_reconnecting = true;
                        continue;
                    }

                    match session.open_stream().await {
                        Ok(stream) => {
                            debug!(chain_id = %self.chain_id, slot = idx, "opened mux stream (after wake-wait)");
                            {
                                let mut guard = self.inner.lock().await;
                                if let Some(SessionSlot::Active { last_open_stream_at, .. }) =
                                    guard.slots.get_mut(idx)
                                {
                                    *last_open_stream_at = Instant::now();
                                }
                            }
                            self.circuit_breaker.record_success();
                            return Ok(TokioMuxStream(stream));
                        }
                        Err(MuxError::Closed) | Err(MuxError::TooManyStreams) => {
                            debug!(chain_id = %self.chain_id, slot = idx, "session not available for new stream, trying next");
                        }
                        Err(e) => {
                            warn!(chain_id = %self.chain_id, slot = idx, error = %e, "open_stream error, trying next");
                        }
                    }
                }
                if marked_reconnecting {
                    self.reconnect_notify.notify_one();
                }
                // Sessions existed but all failed — fall through to wait again.
            }

            let remaining = total_deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            tokio::select! {
                _ = self.stream_notify.notified() => {
                    debug!(chain_id = %self.chain_id, "open_stream: stream_notify woke wake-wait loop");
                }
                _ = tokio::time::sleep(remaining) => {
                    debug!(chain_id = %self.chain_id, "open_stream: wake-wait deadline reached (15s)");
                    break;
                }
            }
        }

        warn!(chain_id = %self.chain_id, "mux pool exhausted after 15s wake-wait window");
        self.circuit_breaker.record_failure();
        Err(MirageError::PoolExhausted)
    }

    /// Signal that the shared peer_table has received a new generation.
    ///
    /// Wakes all sleeping mux reconnect tasks so they retry immediately with
    /// fresh peer data instead of waiting out their exponential backoff delay.
    /// Also resets their attempt counter so they do not hit the max-attempts
    /// cap prematurely.
    ///
    /// Call this whenever `ArcSwap<PeerTable>::store(new_table)` is called on
    /// the table that was passed to [`MuxPool::new`].
    pub fn notify_peer_table_change(&self) {
        self.peer_table_notify.notify_waiters();
    }
}

async fn build_session(
    chain_protocols: &Arc<ProtocolChain>,
    chain: &ChainConfig,
    peer_table: &PeerTable,
    mesh_id: &[u8],
    pre_minted: Option<&mirage_protocol::driver::ClientCredential>,
    credential_params: Option<&CredentialParams>,
) -> Result<SessionHandle, MirageError> {
    // Onion chains send MUX\0 magic so the exit upgrades to a mux server.
    // Direct chains use the raw tunnel path (no magic needed — they connect
    // directly to a relay that already runs a mux server).
    let tunnel: Box<dyn Tunnel> = establish_mux_tunnel(chain_protocols, chain, peer_table, mesh_id, pre_minted, credential_params).await?;
    let compat = TunnelCompat::wrap(tunnel);
    Ok(SessionHandle::client(compat))
}

/// Background task: periodically closes long-idle mux sessions.
///
/// Sessions where no stream has been opened in [`MUX_POOL_IDLE_KILL_TIMEOUT`]
/// are transitioned to `Dormant`: the underlying TCP connection is closed and no
/// reconnect is immediately spawned.  The next `open_stream()` call that finds no
/// `Active` session will wake `Dormant` slots to `Reconnecting`.
///
/// Stops automatically when the pool is dropped (weak reference upgrade fails).
async fn mux_reaper_loop(pool_weak: std::sync::Weak<MuxPool>) {
    loop {
        tokio::time::sleep(MUX_POOL_IDLE_REAPER_INTERVAL).await;

        let Some(pool) = pool_weak.upgrade() else {
            return;
        };

        // Collect stale active slots and take ownership of their session handles.
        let stale_sessions: Vec<(usize, Arc<SessionHandle>)> = {
            let mut guard = pool.inner.lock().await;
            let mut collected = Vec::new();
            for (idx, slot) in guard.slots.iter_mut().enumerate() {
                if let SessionSlot::Active { session, last_open_stream_at } = slot {
                    if last_open_stream_at.elapsed() >= MUX_POOL_IDLE_KILL_TIMEOUT {
                        let session_clone = Arc::clone(session);
                        *slot = SessionSlot::Dormant;
                        collected.push((idx, session_clone));
                    }
                }
            }
            collected
        };

        for (idx, session) in stale_sessions {
            info!(
                chain_id = %pool.chain_id,
                slot = idx,
                idle_secs = MUX_POOL_IDLE_KILL_TIMEOUT.as_secs(),
                "mux reaper: closing long-idle session (→ Dormant)"
            );
            // Try to take exclusive ownership so we can call the async close.
            // If the Arc has other owners (a stream is still active), fall back
            // to a plain drop — the session will close when the last Arc drops.
            match Arc::try_unwrap(session) {
                Ok(handle) => {
                    if let Err(e) = handle.close().await {
                        debug!(chain_id = %pool.chain_id, slot = idx, error = %e, "mux reaper: session close error (ignored)");
                    }
                }
                Err(_arc) => {
                    // Other owners exist — drop our reference, not forcibly closed.
                    debug!(chain_id = %pool.chain_id, slot = idx, "mux reaper: session still referenced, releasing Arc");
                }
            }
        }
    }
}

async fn mux_maintenance_loop(
    pool_weak: std::sync::Weak<MuxPool>,
    reconnect_notify: Arc<tokio::sync::Notify>,
) {
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<usize>();
    let mut in_flight: std::collections::HashSet<usize> = std::collections::HashSet::new();

    loop {
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
            _ = reconnect_notify.notified() => {}
        }

        while let Ok(idx) = done_rx.try_recv() {
            in_flight.remove(&idx);
        }

        let Some(pool) = pool_weak.upgrade() else {
            return;
        };

        let reconnect_indices: Vec<usize> = {
            let mut guard = pool.inner.lock().await;
            let mut indices = Vec::new();
            for (idx, slot) in guard.slots.iter_mut().enumerate() {
                if let SessionSlot::Active { session, .. } = slot {
                    if session.is_closed() {
                        debug!(chain_id = %pool.chain_id, slot = idx, "maintenance: session closed, reconnecting");
                        *slot = SessionSlot::Reconnecting;
                        indices.push(idx);
                    }
                }
            }
            indices
        };

        let reconnecting_indices: Vec<usize> = {
            let guard = pool.inner.lock().await;
            guard
                .slots
                .iter()
                .enumerate()
                .filter_map(|(i, s)| {
                    if matches!(s, SessionSlot::Reconnecting) {
                        Some(i)
                    } else {
                        None
                    }
                })
                .collect()
        };

        let mut all_indices = reconnect_indices;
        for idx in reconnecting_indices {
            if !all_indices.contains(&idx) {
                all_indices.push(idx);
            }
        }

        for slot_idx in all_indices {
            if in_flight.contains(&slot_idx) {
                continue;
            }
            in_flight.insert(slot_idx);
            let pool_clone = Arc::clone(&pool);
            let done_tx = done_tx.clone();
            tokio::spawn(async move {
                reconnect_mux_slot(pool_clone, slot_idx).await;
                let _ = done_tx.send(slot_idx);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::tunnel::{establish_mux_tunnel, ONION_MUX_MAGIC};

    /// Assert the magic constant is the right 4 bytes.
    #[test]
    fn onion_mux_magic_correct() {
        assert_eq!(ONION_MUX_MAGIC, b"MUX\0");
    }

    /// When a chain has hops (onion), `establish_mux_tunnel` would pass
    /// ONION_MUX_MAGIC as initial_user_data.  This test is a compile-time guard
    /// that the constant is visible here and has the expected value.  The actual
    /// magic injection into OnionTunnel::connect is tested via e2e after deploy.
    #[test]
    fn onion_session_prepends_magic() {
        // Simulate: chain with hops → magic must be non-empty and start "MUX"
        let magic = ONION_MUX_MAGIC;
        assert!(!magic.is_empty());
        assert_eq!(&magic[..3], b"MUX");
        assert_eq!(magic[3], 0u8); // null terminator
    }

    /// `establish_mux_tunnel` for an onion chain where hops have
    /// `public_key = None` and matching `node_id` entries in the peer_table must
    /// NOT fail with the config error "hop public_key must be 64 hex chars, got 0".
    ///
    /// The chain has 2 hops (→ onion path), so `chain_protocols` is never called
    /// (onion connect goes directly over TCP).  The test confirms pubkey resolution
    /// succeeds and the error is network-level (connection refused), not a
    /// validation error from the onion builder.
    #[tokio::test]
    async fn mux_pool_peer_table_resolution_passes_pubkey_validation() {
        use arc_swap::ArcSwap;
        use mirage_core::{
            config::{
                ChainConfig, HandshakeStrategy, HopConfig, HopRole, ProtocolId, TcpProtocolId,
            },
            mirage::mesh::peer_table::{PeerTable, PeerTableEntry},
        };
        use mirage_protocol::negotiation::ProtocolChain;
        use std::sync::Arc;

        let pubkey = "b".repeat(64);
        let peer_table = Arc::new(ArcSwap::from_pointee(PeerTable {
            generation: 1,
            peers: vec![
                PeerTableEntry {
                    mesh_id: "test".into(),
                    responsible_monitor_id: String::new(),
                    node_id: "v2-entry".into(),
                    pubkey_hex: pubkey.clone(),
                    addr: "127.0.0.1:19988".into(),
                    labels: vec![],
                    updated_at_ms: 0,
                    aead_suite_preference: Vec::new(),
                    chain_face: None,
                    chain_face_udp: None,
                },
                PeerTableEntry {
                    mesh_id: "test".into(),
                    responsible_monitor_id: String::new(),
                    node_id: "v2-exit".into(),
                    pubkey_hex: "c".repeat(64),
                    addr: "127.0.0.1:19989".into(),
                    labels: vec![],
                    updated_at_ms: 0,
                    aead_suite_preference: Vec::new(),
                    chain_face: None,
                    chain_face_udp: None,
                },
            ],
        }));

        // Two hops → onion path in establish_mux_tunnel (no chain_protocols needed).
        let chain = ChainConfig {
            chain_id: "test-mux-peer".into(),
            hops: vec![
                HopConfig {
                    addr: "127.0.0.1:19988".into(),
                    // Deliberately None — must be resolved from peer_table.
                    public_key: None,
                    node_id: Some("v2-entry".into()),
                    sni: None,
                    role: HopRole::Entry,
                    display_name: None,
                    camouflage: None,
                    byte_cipher: None,
                    camo_server_pubkey: None,
                    relay_mode: mirage_core::mirage::wire::RelayMode::OnionPeel,
                },
                HopConfig {
                    addr: "127.0.0.1:19989".into(),
                    public_key: None,
                    node_id: Some("v2-exit".into()),
                    sni: None,
                    role: HopRole::Exit,
                    display_name: None,
                    camouflage: None,
                    byte_cipher: None,
                    camo_server_pubkey: None,
                    relay_mode: mirage_core::mirage::wire::RelayMode::OnionPeel,
                },
            ],
            handshake: HandshakeStrategy::Telescopic,
            reply: mirage_core::config::ReplyMode::Raw,
            protocol: ProtocolId::Tcp(TcpProtocolId::Mirage),
            priority: 0,
            shaping: Default::default(),
            camouflage: Default::default(),
            cover_traffic: Default::default(),
            socks5_bind: None,
            aead_suite: mirage_core::crypto::AeadSuite::ChaCha20Poly1305,
            kill_switch: None,
            group: None,
            nature: None,
        };

        // Build a minimal ProtocolChain (unused for the 2-hop onion path — onion
        // connect goes directly over TCP and never calls chain_protocols.connect).
        use mirage_core::config::{AuthMode, MirageEndpointConfig, TransportCamouflage, TransportKind};
        use mirage_protocol::endpoint_protocol::EndpointProtocol;
        let ep_cfg = MirageEndpointConfig {
            transport: TransportKind::Tcp,
            auth: AuthMode::Blake3,
            camouflage: TransportCamouflage::None,
            byte_cipher: None,
            byte_cipher_mode: None,
        };
        let chain_protocols = Arc::new(
            ProtocolChain::new(vec![Box::new(EndpointProtocol::new(
                ep_cfg,
                TcpProtocolId::Mirage,
                vec![],
            ))]),
        );

        let pt_snap = peer_table.load_full();
        let result = crate::tunnel::establish_mux_tunnel(
            &chain_protocols,
            &chain,
            &pt_snap,
            &[],
            None,
            None,
        )
        .await;

        match result {
            Ok(_) => panic!("expected a network-level error (no server on :19988/:19989)"),
            Err(e) => {
                let msg = format!("{e:?}");
                // Must NOT be a config error about "64 hex chars".
                assert!(
                    !msg.contains("64 hex chars"),
                    "pubkey resolution must have succeeded before dial; \
                     got unexpected config error: {msg}"
                );
                // Must be a network-level failure.
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

    // ── Additional mux_pool smoke tests ──────────────────────────────────────

    fn make_two_hop_chain(entry_id: &str, exit_id: &str) -> mirage_core::config::ChainConfig {
        use mirage_core::config::{
            ChainConfig, HandshakeStrategy, HopConfig, HopRole, ProtocolId,
            TcpProtocolId,
        };
        ChainConfig {
            chain_id: "test-mux-smoke".into(),
            hops: vec![
                HopConfig {
                    addr: "127.0.0.1:19981".into(),
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
                    addr: "127.0.0.1:19982".into(),
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
            handshake: HandshakeStrategy::Telescopic,
            reply: mirage_core::config::ReplyMode::Raw,
            protocol: ProtocolId::Tcp(TcpProtocolId::Mirage),
            priority: 0,
            shaping: Default::default(),
            camouflage: Default::default(),
            cover_traffic: Default::default(),
            socks5_bind: None,
            aead_suite: mirage_core::crypto::AeadSuite::ChaCha20Poly1305,
            kill_switch: None,
            group: None,
            nature: None,
        }
    }

    fn make_chain_protocols() -> std::sync::Arc<mirage_protocol::negotiation::ProtocolChain> {
        use mirage_core::config::{
            AuthMode, MirageEndpointConfig, TcpProtocolId, TransportCamouflage, TransportKind,
        };
        use mirage_protocol::{endpoint_protocol::EndpointProtocol, negotiation::ProtocolChain};
        let ep_cfg = MirageEndpointConfig {
            transport: TransportKind::Tcp,
            auth: AuthMode::Blake3,
            camouflage: TransportCamouflage::None,
            byte_cipher: None,
            byte_cipher_mode: None,
        };
        std::sync::Arc::new(ProtocolChain::new(vec![Box::new(EndpointProtocol::new(
            ep_cfg,
            TcpProtocolId::Mirage,
            vec![],
        ))]))
    }

    /// Empty peer_table → `establish_mux_tunnel` must return the specific
    /// "not in peer_table" error, NOT the generic "64 hex chars" config error.
    ///
    /// Regression guard for the "config error 'must be 64 hex chars'" bug
    /// that existed before Phase 9.6 (commit d438661).
    #[tokio::test]
    async fn mux_pool_empty_peer_table_returns_specific_error() {
        use mirage_core::mirage::mesh::PeerTable;

        let chain = make_two_hop_chain("v2-entry", "v2-exit");
        let chain_protocols = make_chain_protocols();
        let pt = PeerTable::empty();

        let result = establish_mux_tunnel(&chain_protocols, &chain, &pt, &[], None, None).await;

        assert!(result.is_err(), "must fail when peer_table is empty");
        let msg = format!("{:?}", result.err().expect("just checked is_err"));
        assert!(
            msg.contains("not in peer_table"),
            "error must mention 'not in peer_table' (got): {msg}"
        );
        assert!(
            !msg.contains("64 hex chars"),
            "must not be the pre-Phase-9.6 config validation error (got): {msg}"
        );
    }

    /// Mux session fails on startup (empty peer_table) → peer_table arrives with
    /// correct entries → next attempt must resolve pubkeys and fail at network
    /// level (not config level). Simulates the "Connection refused recovery race"
    /// fix from Phase 9.6.
    #[tokio::test]
    async fn mux_pool_recovery_after_peer_table_arrives() {
        use mirage_core::mirage::mesh::peer_table::{PeerTable, PeerTableEntry};

        let chain = make_two_hop_chain("v2-entry", "v2-exit");
        let chain_protocols = make_chain_protocols();

        // Phase 1: empty peer_table → config error (simulated startup failure).
        let pt_empty = PeerTable::empty();
        let err = establish_mux_tunnel(&chain_protocols, &chain, &pt_empty, &[], None, None).await;
        assert!(err.is_err());
        let startup_msg = format!("{:?}", err.err().expect("just checked is_err"));
        assert!(
            startup_msg.contains("not in peer_table"),
            "startup fail must cite peer_table absence: {startup_msg}"
        );

        // Phase 2: peer_table arrives (monitor push simulated as new snapshot).
        let pt_full = PeerTable {
            generation: 1,
            peers: vec![
                PeerTableEntry {
                    mesh_id: "test".into(),
                    responsible_monitor_id: String::new(),
                    node_id: "v2-entry".into(),
                    pubkey_hex: "a".repeat(64),
                    addr: "127.0.0.1:19981".into(),
                    labels: vec![],
                    updated_at_ms: 0,
                    aead_suite_preference: Vec::new(),
                    chain_face: None,
                    chain_face_udp: None,
                },
                PeerTableEntry {
                    mesh_id: "test".into(),
                    responsible_monitor_id: String::new(),
                    node_id: "v2-exit".into(),
                    pubkey_hex: "b".repeat(64),
                    addr: "127.0.0.1:19982".into(),
                    labels: vec![],
                    updated_at_ms: 0,
                    aead_suite_preference: Vec::new(),
                    chain_face: None,
                    chain_face_udp: None,
                },
            ],
        };

        // Phase 3: retry — pubkey resolve succeeds, fails at network (no server).
        let result = establish_mux_tunnel(&chain_protocols, &chain, &pt_full, &[], None, None).await;
        match result {
            Ok(_) => panic!("expected network error; no server on :19981/:19982"),
            Err(e) => {
                let msg = format!("{e:?}");
                assert!(
                    !msg.contains("64 hex chars"),
                    "config error must not reappear after peer_table arrives: {msg}"
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
    /// `peer_table_notify.notified()` in the mux pool.
    ///
    /// Mirrors the TunnelPool test.  The mechanism is symmetric: mux reconnect
    /// tasks that exhaust `max_attempts` block on `peer_table_notify` and are
    /// woken immediately when new peer data arrives.
    #[tokio::test]
    async fn mux_notify_peer_table_change_wakes_blocked_reconnect() {
        use super::MuxPool;
        use arc_swap::ArcSwap;
        use mirage_core::{config::TunnelPoolConfig, mirage::mesh::PeerTable};
        use std::sync::Arc;

        let chain = make_two_hop_chain("node-a", "node-b");
        let chain_protocols = make_chain_protocols();
        let peer_table = Arc::new(ArcSwap::from_pointee(PeerTable::empty()));
        let config = TunnelPoolConfig {
            pool_size: 1,
            max_reconnect_attempts: 1,
            reconnect_base_ms: 10,
            idle_timeout_secs: 300,
        };

        let credential_params = Arc::new(arc_swap::ArcSwap::from_pointee(None::<mirage_core::crypto::credential::CredentialParams>));
        let pool = MuxPool::new(
            chain,
            chain_protocols,
            config,
            vec![],
            peer_table,
            credential_params,
            Arc::new(arc_swap::ArcSwap::from_pointee(crate::token_pool::TokenPool::default())),
            Arc::new(arc_swap::ArcSwap::from_pointee(None::<crate::token_pool::RingCredential>)),
            false, // credential_gated: no gating in tests
        )
            .await
            .expect("MuxPool::new must not fail during construction");

        let notified = {
            let n = Arc::clone(&pool.peer_table_notify);
            tokio::spawn(async move {
                n.notified().await;
            })
        };

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        pool.notify_peer_table_change();

        tokio::time::timeout(
            std::time::Duration::from_millis(500),
            notified,
        )
        .await
        .expect("mux notify_peer_table_change must wake blocked task within 500 ms")
        .expect("task must not panic");
    }

    /// `notify_peer_table_change()` with no subscribers must not panic or block.
    #[tokio::test]
    async fn mux_notify_peer_table_change_no_subscribers_is_noop() {
        use super::MuxPool;
        use arc_swap::ArcSwap;
        use mirage_core::{config::TunnelPoolConfig, mirage::mesh::PeerTable};
        use std::sync::Arc;

        let chain = make_two_hop_chain("node-c", "node-d");
        let chain_protocols = make_chain_protocols();
        let peer_table = Arc::new(ArcSwap::from_pointee(PeerTable::empty()));
        let config = TunnelPoolConfig {
            pool_size: 1,
            max_reconnect_attempts: 1,
            reconnect_base_ms: 10,
            idle_timeout_secs: 300,
        };

        let credential_params = Arc::new(arc_swap::ArcSwap::from_pointee(None::<mirage_core::crypto::credential::CredentialParams>));
        let pool = MuxPool::new(
            chain,
            chain_protocols,
            config,
            vec![],
            peer_table,
            credential_params,
            Arc::new(arc_swap::ArcSwap::from_pointee(crate::token_pool::TokenPool::default())),
            Arc::new(arc_swap::ArcSwap::from_pointee(None::<crate::token_pool::RingCredential>)),
            false, // credential_gated: no gating in tests
        )
            .await
            .expect("MuxPool::new must not fail during construction");

        pool.notify_peer_table_change();
        pool.notify_peer_table_change();
    }
}

async fn reconnect_mux_slot(pool: Arc<MuxPool>, slot_idx: usize) {
    let chain_id = Arc::clone(&pool.chain_id);
    let (max_attempts, base_ms, chain, chain_protocols, mesh_id, peer_table_shared,
         peer_table_notify, credential_params_shared, token_pool_shared,
         ring_credential_shared, credential_gated) = {
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
            Arc::clone(&guard.ring_credential),
            guard.credential_gated,
        )
    };

    let is_onion = chain.hops.len() > 1;
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        debug!(chain_id = %chain_id, slot = slot_idx, attempt, is_onion, hops = chain.hops.len(), "mux reconnect attempt");

        // Load a fresh peer_table snapshot on each attempt so we pick up
        // any updates that arrived between retries (including the transition
        // from empty → populated when the monitor link first delivers peers).
        let peer_table_snap = peer_table_shared.load_full();

        // Freshen hop pubkeys from live peer_table — fixes UDP (and TCP) recovery
        // after ephemeral-key rotation without requiring switch_chain.
        let fresh_chain = match freshen_chain_pubkeys(&chain, &peer_table_snap) {
            Ok(c) => c,
            Err(e) => {
                debug!(chain_id = %chain_id, slot = slot_idx, error = %e, "mux: freshen_chain_pubkeys: using original chain");
                chain.clone()
            }
        };
        let cred_snap = credential_params_shared.load_full();
        let cred_ref = cred_snap.as_ref().as_ref();
        // Phase 4: pop one blind token for this mux reconnect dial.
        // A popped token must not be re-inserted on failure — it may have reached
        // the node's SpentSet and would be rejected as a replay on any retry.
        let spent = token_pool::pop_one(&token_pool_shared);
        if spent.is_none() {
            let creds = credential_params_shared.load();
            if let Some(cp) = creds.as_ref() {
                let has_blind = cp.authorities.iter().any(|a| {
                    a.scheme == mirage_core::mirage::wire::CredentialScheme::BlindRsaBatch
                });
                if has_blind {
                    warn!(
                        chain_id = %chain_id,
                        slot = slot_idx,
                        "mux: token pool empty but BlindRsaBatch authority is active — \
                         session may be rejected by enforce-mode nodes"
                    );
                }
            }
        }
        // Prefer ring-membership (0x04) when a RingMembership authority is
        // active; fall back to the blind token (0x03) otherwise.
        let ring_snap = ring_credential_shared.load_full();
        let pre_minted = crate::token_pool::build_ring_credential(
            cred_snap.as_ref().as_ref(),
            ring_snap.as_ref().as_ref(),
            &crate::token_pool::ring_dial_context(&fresh_chain),
        ).or_else(|| spent.as_ref().map(|t| {
            debug!(chain_id = %chain_id, slot = slot_idx, epoch_id = t.epoch_id, "mux_pool: spending blind token on reconnect");
            mirage_protocol::driver::ClientCredential { scheme: 0x03, token: t.to_wire() }
        }));
        match build_session(&chain_protocols, &fresh_chain, &peer_table_snap, &mesh_id, pre_minted.as_ref(), cred_ref).await {
            Ok(session) => {
                info!(chain_id = %chain_id, slot = slot_idx, attempt, "mux reconnect succeeded");
                {
                    let mut guard = pool.inner.lock().await;
                    guard.slots[slot_idx] = SessionSlot::Active {
                        session: Arc::new(session),
                        last_open_stream_at: Instant::now(),
                    };
                }
                pool.circuit_breaker.record_success();
                pool.stream_notify.notify_waiters();
                return;
            }
            Err(e) => {
                pool.circuit_breaker.record_failure();
                let limit_reached = max_attempts > 0 && attempt >= max_attempts;
                if limit_reached {
                    // CREDENTIAL_REJECTED on a credential-gated pool is transient:
                    // the ring slot is provisioned asynchronously and may arrive
                    // 15-45 s after the first dial.  Do NOT park on
                    // peer_table_change -- that signal never fires for credential
                    // delivery.  Retry at the capped interval so the next attempt
                    // picks up the hot-swapped RingCredential from the cell.
                    //
                    // B4b: same effective_credential_gated derivation as pool.rs --
                    // gate on CredentialParams presence (credential enforcement is
                    // unconditional, no `require` toggle) not on
                    // operator_pub_hex.is_some() (construction-time heuristic).
                    let effective_credential_gated = cred_snap
                        .as_ref()
                        .as_ref()
                        .map(|_p| true)
                        .unwrap_or(credential_gated);
                    if effective_credential_gated && crate::pool::is_credential_rejected(&e) {
                        debug!(
                            chain_id = %chain_id,
                            slot = slot_idx,
                            attempt,
                            "mux: credential not yet available (CREDENTIAL_REJECTED), \
                             will retry in {}ms",
                            MUX_CREDENTIAL_REJECTED_RETRY_CAP_MS,
                        );
                        // Reset attempt so regular backoff resumes; cap kicks in
                        // again if the credential still has not arrived.
                        attempt = 0;
                        tokio::select! {
                            _ = tokio::time::sleep(
                                std::time::Duration::from_millis(MUX_CREDENTIAL_REJECTED_RETRY_CAP_MS)
                            ) => {}
                            _ = peer_table_notify.notified() => {
                                info!(
                                    chain_id = %chain_id,
                                    slot = slot_idx,
                                    "mux: peer_table changed during credential-wait backoff \
                                     -- retrying immediately"
                                );
                            }
                        }
                        continue;
                    }

                    error!(
                        chain_id = %chain_id,
                        slot = slot_idx,
                        attempt,
                        is_onion,
                        error = %e,
                        "mux reconnect gave up after max attempts — parking (timeout={}ms)",
                        mux_slot_park_timeout().as_millis(),
                    );
                    // B4a: park with a bounded timeout instead of blocking
                    // forever on peer_table_notify.  For a dead or static entry
                    // the notify never fires, permanently stranding the slot.
                    // On timeout, re-resolve by resetting the attempt counter
                    // and re-entering the loop — freshen_chain_pubkeys at the
                    // top of the loop picks up any peer_table changes.  The
                    // timeout IS the retry cadence for dead-entry chains
                    // (bug 5 / F2 — mux path).
                    let park_timeout = mux_slot_park_timeout();
                    tokio::select! {
                        _ = tokio::time::sleep(park_timeout) => {
                            info!(
                                chain_id = %chain_id,
                                slot = slot_idx,
                                timeout_ms = park_timeout.as_millis(),
                                "mux: park timeout expired — re-resolving slot target and retrying",
                            );
                        }
                        _ = peer_table_notify.notified() => {
                            info!(chain_id = %chain_id, slot = slot_idx, "mux: peer_table changed — resetting attempt counter");
                        }
                    }
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
                    "mux reconnect failed, retrying"
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
                            "mux: peer_table changed during backoff — retrying immediately"
                        );
                        // Reset attempt so fresh peer data gets a clean counter.
                        attempt = 0;
                    }
                }
            }
        }
    }
}
