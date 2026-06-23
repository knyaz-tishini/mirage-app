//! Health watcher — reacts to MonitorConfig health flags and auto-switches
//! the active chain when the current one becomes unhealthy.
//!
//! Call [`run_health_watcher`] as a background task.  The function runs
//! forever (until the watch channel is closed) and does not spawn anything
//! internally — spawning is the caller's responsibility.
//!
//! Policy pushing is handled by a separate task — see
//! [`crate::policy_push::run_policy_pusher`].

use std::sync::Arc;

use tokio::sync::watch;
use tracing::{error, info, warn};

use crate::monitor_link::MonitorConfig;
use crate::runtime::ClientRuntime;

/// Watch the monitor config channel and auto-switch the active chain whenever
/// the current chain is reported unhealthy.
///
/// # Behaviour
///
/// 1. Waits for the watch channel to change.
/// 2. On each update, checks whether the active chain is healthy.
/// 3. If unhealthy, iterates `MonitorConfig.chains` in order and switches to
///    the first healthy one found.
/// 4. If no healthy chain exists, logs an error but keeps the current chain
///    (degraded mode is better than no chain at all).
/// 5. Returns when the sender half of the watch channel is dropped (i.e. the
///    monitor link is shutting down).
pub async fn run_health_watcher(
    runtime: Arc<ClientRuntime>,
    mut config_rx: watch::Receiver<Option<MonitorConfig>>,
) {
    loop {
        // Block until there is a new config version to inspect.
        if config_rx.changed().await.is_err() {
            // Sender dropped — monitor link shut down.
            info!("health_watcher: config channel closed, exiting");
            return;
        }

        let config = config_rx.borrow().clone();
        let Some(cfg) = config else {
            // Channel initialised but no config pushed yet — ignore.
            continue;
        };

        let active_id = runtime.active_chain_id.load();
        let active_id: &str = active_id.as_ref();

        // Find the health status of the active chain in the received config.
        // Treat "monitor doesn't know this chain" as healthy — chain inventory
        // is owned by client-config.json, monitor only annotates known chains.
        let active_healthy = cfg
            .chains
            .iter()
            .find(|c| c.name() == active_id)
            .map(|c| c.healthy)
            .unwrap_or(true);

        if !active_healthy {
            warn!(chain = %active_id, "health_watcher: active chain is unhealthy, searching for replacement");

            // Find the first healthy chain whose pool is actually present in the runtime.
            let pools_snapshot = runtime.pool_snapshot();
            let candidate = cfg
                .chains
                .iter()
                .find(|c| c.healthy && pools_snapshot.contains_key(c.name()));

            match candidate {
                Some(chain) => {
                    info!(
                        from = %active_id,
                        to   = %chain.name(),
                        "health_watcher: switching active chain"
                    );
                    runtime.active_chain_id.store(Arc::new(chain.name().to_string()));
                }
                None => {
                    error!(
                        chain = %active_id,
                        "health_watcher: no healthy chain available, keeping current (degraded mode)"
                    );
                }
            }
        }
    }
}
