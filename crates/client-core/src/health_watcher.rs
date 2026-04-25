//! Health watcher — reacts to MonitorConfig health flags and auto-switches
//! the active chain when the current one becomes unhealthy.
//!
//! Call [`run_health_watcher`] as a background task.  The function runs
//! forever (until the watch channel is closed) and does not spawn anything
//! internally — spawning is the caller's responsibility.

use std::sync::Arc;

use tokio::sync::watch;
use tracing::{error, info, warn};

use crate::monitor_link::MonitorConfig;
use crate::runtime::ClientRuntime;

/// Watch the monitor config channel and auto-switch the active chain whenever
/// the current chain is reported unhealthy.  When `tun_api_url` is provided,
/// also pushes any policy update from the monitor to the TUN daemon.
///
/// # Behaviour
///
/// 1. Waits for the watch channel to change.
/// 2. On each update, checks whether the active chain is healthy.
/// 3. If unhealthy, iterates `MonitorConfig.chains` in order and switches to
///    the first healthy one found.
/// 4. If no healthy chain exists, logs an error but keeps the current chain
///    (degraded mode is better than no chain at all).
/// 5. When `tun_api_url` is set and the config version changed and a policy is
///    present, POSTs the policy to `{tun_api_url}/policy/set`.
/// 6. Returns when the sender half of the watch channel is dropped (i.e. the
///    monitor link is shutting down).
pub async fn run_health_watcher(
    runtime: Arc<ClientRuntime>,
    mut config_rx: watch::Receiver<Option<MonitorConfig>>,
    tun_api_url: Option<String>,
) {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_default();

    let mut last_policy_version: u64 = 0;

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
            .find(|c| c.name == active_id)
            .map(|c| c.healthy)
            .unwrap_or(true);

        if !active_healthy {
            warn!(chain = %active_id, "health_watcher: active chain is unhealthy, searching for replacement");

            // Find the first healthy chain whose pool is actually present in the runtime.
            let pools_snapshot = runtime.pool_snapshot();
            let candidate = cfg
                .chains
                .iter()
                .find(|c| c.healthy && pools_snapshot.contains_key(&c.name));

            match candidate {
                Some(chain) => {
                    info!(
                        from = %active_id,
                        to   = %chain.name,
                        "health_watcher: switching active chain"
                    );
                    runtime.active_chain_id.store(Arc::new(chain.name.clone()));
                }
                None => {
                    error!(
                        chain = %active_id,
                        "health_watcher: no healthy chain available, keeping current (degraded mode)"
                    );
                }
            }
        }

        // Apply policy to TUN if provided by monitor and version changed.
        if cfg.version != last_policy_version {
            if let (Some(ref policy), Some(ref tun_url)) = (&cfg.policy, &tun_api_url) {
                let url = format!("{}/policy/set", tun_url);
                let body = serde_json::json!({
                    "rules": policy.rules,
                    "default_action": policy.default_action,
                });
                match http
                    .post(&url)
                    .header("Content-Type", "application/json")
                    .json(&body)
                    .send()
                    .await
                {
                    Ok(resp) if resp.status().is_success() => {
                        info!("health_watcher: pushed policy to TUN");
                    }
                    Ok(resp) => {
                        warn!(status = %resp.status(), "health_watcher: TUN policy push failed");
                    }
                    Err(e) => {
                        warn!(error = %e, "health_watcher: TUN policy push error");
                    }
                }
            }
            last_policy_version = cfg.version;
        }
    }
}
