//! Policy pusher — forwards monitor-pushed `SplitPolicyConfig` to the local
//! TUN daemon on every config version bump.
//!
//! Call [`run_policy_pusher`] as a background task.  The function runs
//! forever (until the watch channel is closed) and does not spawn anything
//! internally — spawning is the caller's responsibility.

use tokio::sync::watch;
use tracing::{info, warn};

use crate::monitor_link::MonitorConfig;

/// Watch the monitor config channel and POST each new `SplitPolicyConfig` to
/// `{tun_api_url}/policy/set`.
///
/// # Behaviour
///
/// 1. Waits for the watch channel to change.
/// 2. On each update checks whether the version number advanced.  Equal
///    versions are skipped (monitor re-pushed without a change).
/// 3. When `MonitorConfig.policy` is `Some`: pushes the policy as-is.
/// 4. When `MonitorConfig.policy` is `None`: pushes an empty
///    `SplitPolicyConfig { rules: [], default_action: Bypass }` so the TUN
///    tears down all bypass routes (operator cleared the policy).
/// 5. Returns when the sender half of the watch channel is dropped (i.e. the
///    monitor link is shutting down).
pub async fn run_policy_pusher(
    mut config_rx: watch::Receiver<Option<MonitorConfig>>,
    tun_api_url: String,
) {
    use mirage_core::split_policy::{SplitPolicyAction, SplitPolicyConfig};

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let url = format!("{}/policy/set", tun_api_url);
    let mut last_version: u64 = 0;

    loop {
        if config_rx.changed().await.is_err() {
            // MonitorLink dropped.
            break;
        }
        let cfg_opt = config_rx.borrow().clone();
        let Some(cfg) = cfg_opt else { continue };
        if cfg.version == last_version {
            continue;
        }
        last_version = cfg.version;

        // Build the body: when policy is Some push it; when None push
        // an empty policy with Bypass default so the TUN tears down
        // all bypass routes (operator cleared the policy).
        let body: SplitPolicyConfig = match cfg.policy {
            Some(ref p) => p.clone(),
            None => SplitPolicyConfig {
                rules: vec![],
                default_action: SplitPolicyAction::Bypass,
            },
        };
        match serde_json::to_value(&body) {
            Ok(json_val) => {
                match http
                    .post(&url)
                    .header("Content-Type", "application/json")
                    .json(&json_val)
                    .send()
                    .await
                {
                    Ok(resp) if resp.status().is_success() => {
                        info!(
                            version = last_version,
                            rules = body.rules.len(),
                            "policy_pusher: pushed split policy to TUN"
                        );
                    }
                    Ok(resp) => {
                        warn!(
                            status = %resp.status(),
                            "policy_pusher: TUN policy push failed"
                        );
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            "policy_pusher: TUN policy push error"
                        );
                    }
                }
            }
            Err(e) => {
                warn!("policy_pusher: failed to serialize SplitPolicyConfig: {e}");
            }
        }
    }
}
