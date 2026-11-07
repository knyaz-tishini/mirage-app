//! Background health-probe task — one instance per `ChainHandle`.
//!
//! Probe mechanism (per coordinator decision §13.3): open a fresh tunnel
//! through the chain via `pool.lease()`, measure handshake latency, close.
//! No echo frame needed — the successful lease IS the probe.

use std::sync::Weak;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use super::handle::ChainHandle;
use super::types::ChainState;
use crate::pool::TunnelLease;

/// Background task: one instance per ChainHandle, spawned by `ChainSet::spawn_health_probes`.
///
/// Holds a `Weak` ref to `ChainHandle`. When the handle is dropped from
/// `ChainSet` the task exits on the next tick.
pub async fn health_probe_task(handle: Weak<ChainHandle>, config: super::types::HealthProbeConfig) {
    let mut consecutive_failures: u32 = 0;

    loop {
        tokio::time::sleep(Duration::from_secs(config.interval_secs)).await;

        let Some(h) = handle.upgrade() else {
            return;
        };

        // Skip probing if already quarantined and backoff not yet elapsed.
        if let ChainState::Quarantined { until, .. } = h.current_state() {
            if Instant::now() < until {
                continue;
            }
        }

        h.mark_probing();
        debug!(chain_id = %h.id, "health probe: start");

        let probe_result = run_probe(&h, config.probe_timeout_ms).await;

        match probe_result {
            Ok(latency_ms) => {
                consecutive_failures = 0;
                h.metrics.record_success(latency_ms);
                h.mark_active();
                debug!(chain_id = %h.id, latency_ms, "health probe: ok");
            }
            Err(reason) => {
                consecutive_failures += 1;
                h.metrics.record_failure();
                warn!(
                    chain_id = %h.id,
                    consecutive_failures,
                    reason = %reason,
                    "health probe: fail"
                );
                if consecutive_failures >= config.quarantine_after_fails {
                    info!(chain_id = %h.id, "health probe: quarantining chain");
                    h.mark_quarantined(config.quarantine_recovery_secs, reason);
                    consecutive_failures = 0;
                } else {
                    // Below threshold — stay Active so traffic still flows.
                    h.mark_active();
                }
            }
        }
    }
}

/// Open a tunnel lease and measure latency. Drops the lease immediately —
/// the successful lease warms the pool slot.
async fn run_probe(h: &ChainHandle, timeout_ms: u64) -> Result<u64, String> {
    let start = Instant::now();
    let timeout = Duration::from_millis(timeout_ms);

    let result = tokio::time::timeout(timeout, lease_once(h)).await;

    match result {
        Ok(Ok(_lease)) => {
            // Lease succeeded — drop it (returns slot to pool as idle).
            Ok(start.elapsed().as_millis() as u64)
        }
        Ok(Err(e)) => Err(e),
        Err(_) => Err(format!("probe timeout after {timeout_ms}ms")),
    }
}

/// One probe = one real lease through the chain.
///
/// This used to open a mux stream first, "to confirm the session is alive",
/// and then take a pool lease in BOTH branches — so the mux half decided
/// nothing and the outcome was the lease either way. It also could not have
/// confirmed what it claimed: `SessionHandle::open_stream` opens lazily and
/// sends no frame, so a success proved only that the driver task had not
/// already torn the session down, never that the transport still carried
/// anything. Its one real effect was to burn a stream id against
/// `max_num_streams` and put an RST on the wire when the unused stream
/// dropped.
///
/// The lease is the probe, exactly as this module's header says.
async fn lease_once(h: &ChainHandle) -> Result<TunnelLease, String> {
    h.pool.lease().await.map_err(|e| e.to_string())
}
