//! Core types for ChainSet: identifiers, state machine, metrics, prewarm strategy.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

// ── ChainId ───────────────────────────────────────────────────────────────────

/// Opaque identifier for a chain — mirrors `ChainConfig::chain_id`.
///
/// Newtype so we cannot accidentally use a node ID as a chain ID at call sites.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ChainId(pub String);

impl ChainId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ChainId {
    fn from(s: &str) -> Self {
        ChainId(s.to_owned())
    }
}

impl From<String> for ChainId {
    fn from(s: String) -> Self {
        ChainId(s)
    }
}

impl std::fmt::Display for ChainId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ── ChainState ────────────────────────────────────────────────────────────────

/// State machine for one chain.
///
/// ```text
///  Active ──probe-tick──► Probing ──probe-ok──► Active
///                             │
///                         probe-fail
///                             │
///                             ▼
///                       Quarantined { until }
///                             │
///                         until elapsed
///                             │
///                             ▼
///                          Probing
///
///  Active / Probing / Quarantined ──no-use-for-idle_keepalive──► Dormant
///  Dormant ──lease-demand──► Probing (immediate probe, then Active or Quarantined)
///
///  Any state ──forced-switch──► Active (supervisor chain update)
/// ```
#[derive(Debug, Clone)]
pub enum ChainState {
    Active,
    Probing { since: Instant },
    Quarantined { until: Instant, reason: String },
    Dormant { since: Instant },
}

impl ChainState {
    pub fn is_usable(&self) -> bool {
        matches!(self, ChainState::Active)
    }
}

// ── ChainMetrics ──────────────────────────────────────────────────────────────

/// All fields are atomics — updated from health probes and Router callbacks
/// without holding any mutex.
pub struct ChainMetrics {
    /// EMA-smoothed p50 latency in milliseconds. `u64::MAX` = no sample yet.
    pub p50_latency_ms: AtomicU64,
    /// EMA-smoothed p99 latency in milliseconds. `u64::MAX` = no sample yet.
    pub p99_latency_ms: AtomicU64,
    /// Total successful leases/streams since process start.
    pub success_count: AtomicU64,
    /// Total failed leases/streams since process start.
    pub failure_count: AtomicU64,
    /// Unix-millis timestamp of most recent success.
    pub last_success_at: AtomicU64,
    /// Unix-millis timestamp of most recent failure.
    pub last_failure_at: AtomicU64,
}

impl ChainMetrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            p50_latency_ms: AtomicU64::new(u64::MAX),
            p99_latency_ms: AtomicU64::new(u64::MAX),
            success_count: AtomicU64::new(0),
            failure_count: AtomicU64::new(0),
            last_success_at: AtomicU64::new(0),
            last_failure_at: AtomicU64::new(0),
        })
    }

    /// Record a successful operation and update EMA latency (alpha = 0.2).
    pub fn record_success(&self, latency_ms: u64) {
        self.success_count.fetch_add(1, Ordering::Relaxed);
        self.last_success_at
            .store(unix_millis_now(), Ordering::Relaxed);
        ema_update(&self.p50_latency_ms, latency_ms);
        // p99 uses a slower-moving EMA (alpha=0.05) — pessimistic tail estimate.
        ema_update_alpha(&self.p99_latency_ms, latency_ms, 0.05);
    }

    pub fn record_failure(&self) {
        self.failure_count.fetch_add(1, Ordering::Relaxed);
        self.last_failure_at
            .store(unix_millis_now(), Ordering::Relaxed);
    }

    pub fn success_rate(&self) -> f64 {
        let s = self.success_count.load(Ordering::Relaxed) as f64;
        let f = self.failure_count.load(Ordering::Relaxed) as f64;
        if s + f == 0.0 {
            return 1.0; // no samples → assume ok
        }
        s / (s + f)
    }
}

fn ema_update(cell: &AtomicU64, sample: u64) {
    ema_update_alpha(cell, sample, 0.2);
}

fn ema_update_alpha(cell: &AtomicU64, sample: u64, alpha: f64) {
    let prev = cell.load(Ordering::Relaxed);
    let next = if prev == u64::MAX {
        sample
    } else {
        (alpha * sample as f64 + (1.0 - alpha) * prev as f64) as u64
    };
    cell.store(next, Ordering::Relaxed);
}

fn unix_millis_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ── PrewarmStrategy ───────────────────────────────────────────────────────────

/// Type alias for a scoring closure. Wrapped in `Arc` so it is `Clone`.
pub type ScoreFn = Arc<dyn Fn(&ChainMetrics) -> f64 + Send + Sync>;

/// Controls which chains are kept pre-warmed (tunnels open) vs. lazy.
#[derive(Clone)]
pub enum PrewarmStrategy {
    /// Keep all chains prewarmed (default).
    All,
    /// Keep only the top-N chains warm, scored by the built-in `latency × success_rate` fn.
    TopN { n: usize },
    /// Warm chains only on demand; idle tunnels held for `idle_keepalive`
    /// before the slot is reaped to Dormant.
    OnDemand { idle_keepalive: Duration },
}

impl Default for PrewarmStrategy {
    fn default() -> Self {
        PrewarmStrategy::All
    }
}

// ── HealthProbeConfig ─────────────────────────────────────────────────────────

/// Configuration shared across all per-chain health probe tasks.
#[derive(Clone, Debug)]
pub struct HealthProbeConfig {
    /// Seconds between probes. Default: 30.
    pub interval_secs: u64,
    /// Number of consecutive failures before quarantine. Default: 3.
    pub quarantine_after_fails: u32,
    /// Seconds before a quarantined chain is re-probed. Default: 30.
    pub quarantine_recovery_secs: u64,
    /// Probe timeout in milliseconds. Default: 5000.
    pub probe_timeout_ms: u64,
}

impl Default for HealthProbeConfig {
    fn default() -> Self {
        Self {
            interval_secs: 30,
            quarantine_after_fails: 3,
            quarantine_recovery_secs: 30,
            probe_timeout_ms: 5_000,
        }
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_id_equality() {
        let a: ChainId = "c-tcp".into();
        let b = ChainId::from("c-tcp".to_string());
        assert_eq!(a, b);
    }

    #[test]
    fn chain_state_usable_only_when_active() {
        assert!(ChainState::Active.is_usable());
        assert!(!ChainState::Probing { since: Instant::now() }.is_usable());
        assert!(!ChainState::Quarantined {
            until: Instant::now(),
            reason: "test".into()
        }
        .is_usable());
        assert!(!ChainState::Dormant { since: Instant::now() }.is_usable());
    }

    #[test]
    fn metrics_success_rate_no_samples() {
        let m = ChainMetrics::new();
        assert!((m.success_rate() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn metrics_records_success_and_failure() {
        let m = ChainMetrics::new();
        m.record_success(50);
        m.record_success(100);
        m.record_failure();
        let rate = m.success_rate();
        // 2 success, 1 failure → 2/3
        assert!((rate - 2.0 / 3.0).abs() < 0.01);
        // p50 should be set (no longer MAX)
        assert_ne!(m.p50_latency_ms.load(Ordering::Relaxed), u64::MAX);
    }
}
