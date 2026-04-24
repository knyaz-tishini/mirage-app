use std::sync::{
    atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Instant;

use mirage_core::{
    config::{ChainConfig, TunnelPoolConfig},
    error::MirageError,
};
use mirage_mux::MuxError;
use mirage_mux::tokio_compat::{SessionHandle, TokioMuxStream};
use mirage_protocol::{negotiation::ProtocolChain, traits::Tunnel};
use tokio::sync::Semaphore;
use tracing::{debug, error, info, warn};

use crate::relay::TunnelCompat;
use crate::tunnel::establish_raw_tunnel;

// ── Circuit breaker state constants ──────────────────────────────────────────
const CIRCUIT_CLOSED: u8 = 0;
const CIRCUIT_OPEN: u8 = 1;
const CIRCUIT_HALF_OPEN: u8 = 2;

/// Per-pool circuit breaker.
///
/// Transitions:
/// - Closed → Open after `failure_threshold` consecutive failures.
/// - Open → Half-Open after `open_duration_secs` elapsed since last failure.
/// - Half-Open → Closed on first success (resets counter).
/// - Half-Open → Open on failure.
struct CircuitBreaker {
    state: AtomicU8,
    consecutive_failures: AtomicU64,
    /// Protected by a plain Mutex — only held for nanoseconds (no await inside).
    last_failure: Mutex<Option<Instant>>,
    open_duration_secs: u64,
    failure_threshold: u64,
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
            chain_label,
        }
    }

    /// Returns `true` if the circuit is currently blocking requests.
    ///
    /// Also drives the Open → Half-Open transition by time.
    fn is_open(&self) -> bool {
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

// ── Pool internals ────────────────────────────────────────────────────────────

enum SessionSlot {
    Active(Arc<SessionHandle>),
    Reconnecting,
}

struct PoolInner {
    slots: Vec<SessionSlot>,
    chain: ChainConfig,
    chain_protocols: Arc<ProtocolChain>,
    config: TunnelPoolConfig,
}

/// Maximum number of open_stream attempts before giving up.
const MAX_OPEN_ATTEMPTS: u32 = 5;

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
}

impl MuxPool {
    pub async fn new(
        chain: ChainConfig,
        chain_protocols: Arc<ProtocolChain>,
        config: TunnelPoolConfig,
    ) -> Result<Arc<Self>, MirageError> {
        let is_onion = !chain.hops.is_empty();
        let pool_size = config.pool_size.max(1).min(8);

        let chain_label = chain
            .hops
            .first()
            .map(|h| h.addr.clone())
            .unwrap_or_else(|| "default".to_string());

        let mut join_set = tokio::task::JoinSet::new();
        for idx in 0..pool_size {
            let cp = Arc::clone(&chain_protocols);
            let ch = chain.clone();
            join_set.spawn(async move { (idx, build_session(&cp, &ch).await) });
        }

        let threshold = (pool_size / 5).max(1);
        let mut slots: Vec<(usize, SessionSlot)> = Vec::with_capacity(pool_size);
        let mut ready_count = 0usize;

        while let Some(result) = join_set.join_next().await {
            match result {
                Ok((idx, Ok(session))) => {
                    info!(slot = idx, ready = ready_count + 1, threshold, "mux session established");
                    slots.push((idx, SessionSlot::Active(Arc::new(session))));
                    ready_count += 1;
                }
                Ok((idx, Err(e))) => {
                    warn!(slot = idx, error = %e, "mux session failed on startup, marking reconnecting");
                    slots.push((idx, SessionSlot::Reconnecting));
                }
                Err(e) => {
                    warn!(error = %e, "mux session join error");
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
        }));

        let pool = Arc::new(Self {
            inner,
            next_idx: AtomicUsize::new(0),
            is_onion: AtomicBool::new(is_onion),
            stream_notify: Arc::new(tokio::sync::Notify::new()),
            reconnect_notify: Arc::new(tokio::sync::Notify::new()),
            circuit_breaker: Arc::new(CircuitBreaker::new(
                chain_label,
                5,  // open after 5 consecutive failures
                5,  // stay open for 5 seconds
            )),
            reconnect_limiter: Arc::new(Semaphore::new(RECONNECT_BURST_LIMIT)),
        });

        let pool_weak = Arc::downgrade(&pool);
        let reconnect_notify_clone = Arc::clone(&pool.reconnect_notify);
        tokio::spawn(async move {
            mux_maintenance_loop(pool_weak, reconnect_notify_clone).await;
        });

        if !join_set.is_empty() {
            let inner_clone = Arc::clone(&pool.inner);
            let notify_clone = Arc::clone(&pool.stream_notify);
            tokio::spawn(async move {
                while let Some(result) = join_set.join_next().await {
                    match result {
                        Ok((idx, Ok(session))) => {
                            let mut guard = inner_clone.lock().await;
                            if matches!(guard.slots.get(idx), Some(SessionSlot::Reconnecting)) {
                                guard.slots[idx] = SessionSlot::Active(Arc::new(session));
                                info!(slot = idx, "mux background warmup: session established");
                                drop(guard);
                                notify_clone.notify_waiters();
                            }
                        }
                        Ok((idx, Err(e))) => {
                            warn!(slot = idx, error = %e, "mux background warmup: session failed");
                        }
                        Err(e) => {
                            warn!(error = %e, "mux background warmup: join error");
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
            debug!("circuit breaker open: rejecting stream request immediately");
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

        for attempt in 0u32..MAX_OPEN_ATTEMPTS {
            let start = self.next_idx.fetch_add(1, Ordering::Relaxed) % pool_size;

            let sessions: Vec<(usize, Arc<SessionHandle>)> = {
                let guard = self.inner.lock().await;
                (0..pool_size)
                    .map(|i| (start + i) % pool_size)
                    .filter_map(|idx| match &guard.slots[idx] {
                        SessionSlot::Active(s) => Some((idx, Arc::clone(s))),
                        SessionSlot::Reconnecting => None,
                    })
                    .collect()
            };

            let mut marked_reconnecting = false;
            for (idx, session) in sessions {
                if session.is_closed() {
                    let mut guard = self.inner.lock().await;
                    guard.slots[idx] = SessionSlot::Reconnecting;
                    debug!(slot = idx, "mux session closed, marked reconnecting");
                    marked_reconnecting = true;
                    continue;
                }

                match session.open_stream().await {
                    Ok(stream) => {
                        debug!(slot = idx, "opened mux stream");
                        self.circuit_breaker.record_success();
                        return Ok(TokioMuxStream(stream));
                    }
                    Err(MuxError::Closed) | Err(MuxError::TooManyStreams) => {
                        debug!(slot = idx, "session not available for new stream, trying next");
                    }
                    Err(e) => {
                        warn!(slot = idx, error = %e, "open_stream error, trying next");
                    }
                }
            }

            if marked_reconnecting {
                self.reconnect_notify.notify_one();
            }

            if attempt < MAX_OPEN_ATTEMPTS - 1 {
                tokio::time::timeout(
                    std::time::Duration::from_millis(500),
                    self.stream_notify.notified(),
                )
                .await
                .ok();
            }
        }

        warn!("mux pool exhausted after {} attempts", MAX_OPEN_ATTEMPTS);
        self.circuit_breaker.record_failure();
        Err(MirageError::PoolExhausted)
    }

    pub async fn switch_chain(&self, new_chain: ChainConfig) {
        let mut guard = self.inner.lock().await;
        guard.chain = new_chain;
        for slot in guard.slots.iter_mut() {
            if matches!(slot, SessionSlot::Active(_)) {
                *slot = SessionSlot::Reconnecting;
            }
        }
        self.is_onion
            .store(!guard.chain.hops.is_empty(), Ordering::Release);
        info!("mux pool chain switched: active sessions marked reconnecting");
    }
}

async fn build_session(
    chain_protocols: &Arc<ProtocolChain>,
    chain: &ChainConfig,
) -> Result<SessionHandle, MirageError> {
    let tunnel: Box<dyn Tunnel> = establish_raw_tunnel(chain_protocols, chain).await?;
    let compat = TunnelCompat::wrap(tunnel);
    Ok(SessionHandle::client(compat))
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
                if let SessionSlot::Active(s) = slot {
                    if s.is_closed() {
                        debug!(slot = idx, "maintenance: session closed, reconnecting");
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

async fn reconnect_mux_slot(pool: Arc<MuxPool>, slot_idx: usize) {
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
        debug!(slot = slot_idx, attempt, "mux reconnect attempt");

        match build_session(&chain_protocols, &chain).await {
            Ok(session) => {
                info!(slot = slot_idx, attempt, "mux reconnect succeeded");
                {
                    let mut guard = pool.inner.lock().await;
                    guard.slots[slot_idx] = SessionSlot::Active(Arc::new(session));
                }
                pool.circuit_breaker.record_success();
                pool.stream_notify.notify_waiters();
                return;
            }
            Err(e) => {
                pool.circuit_breaker.record_failure();
                let limit_reached = max_attempts > 0 && attempt >= max_attempts;
                if limit_reached {
                    error!(
                        slot = slot_idx,
                        attempt,
                        error = %e,
                        "mux reconnect gave up after max attempts"
                    );
                    return;
                }
                let delay_ms = base_ms.saturating_mul(1u64 << attempt.min(10));
                warn!(
                    slot = slot_idx,
                    attempt,
                    delay_ms,
                    error = %e,
                    "mux reconnect failed, retrying"
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
        }
    }
}
