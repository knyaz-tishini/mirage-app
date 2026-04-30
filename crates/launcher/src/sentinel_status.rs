//! Polls `mirage-l0-sentinel` `/status` endpoint every 500 ms and exposes
//! the current phase as a `watch::Receiver<SentinelPhaseLocal>`.

use std::time::Duration;

use serde::Deserialize;
use tokio::sync::watch;

const SENTINEL_STATUS_URL: &str = "http://127.0.0.1:19420/status";
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Local mirror of the sentinel phase discriminant.
///
/// The sentinel publishes `{ "phase": "<snake_case>", ... }`.  We parse only
/// the `phase` field and map it here; unknown variants fall through to
/// `Unknown`.
#[derive(Default, Clone, Debug, PartialEq)]
pub enum SentinelPhaseLocal {
    #[default]
    Unknown,
    Starting,
    FetchingConfig,
    ConfigReady,
    ChildRunning,
    /// `monitor_api_key` or `client_id` is missing in `sentinel.toml`.
    /// Launcher should prompt the user to paste an onboarding link.
    WaitingOnboard,
    /// Config was fetched successfully before but is now stale (monitor
    /// unreachable for too long). Children may still be running.
    ConfigStale,
    Error(String),
}

#[derive(Deserialize)]
struct SentinelStatusRaw {
    phase: String,
    #[serde(default)]
    error: Option<String>,
}

fn parse_phase(raw: SentinelStatusRaw) -> SentinelPhaseLocal {
    match raw.phase.as_str() {
        "starting" => SentinelPhaseLocal::Starting,
        "fetching_config" => SentinelPhaseLocal::FetchingConfig,
        "config_ready" => SentinelPhaseLocal::ConfigReady,
        "child_running" => SentinelPhaseLocal::ChildRunning,
        "waiting_onboard" => SentinelPhaseLocal::WaitingOnboard,
        "config_stale" => SentinelPhaseLocal::ConfigStale,
        "error" => SentinelPhaseLocal::Error(
            raw.error.unwrap_or_else(|| "sentinel error".into()),
        ),
        _ => SentinelPhaseLocal::Unknown,
    }
}

/// Spawns a background task that polls the sentinel `/status` endpoint.
/// Returns a `watch::Receiver` — the renderer reads `.borrow()` every frame.
pub fn spawn_poller(rt: &tokio::runtime::Handle) -> watch::Receiver<SentinelPhaseLocal> {
    let (tx, rx) = watch::channel(SentinelPhaseLocal::Unknown);

    rt.spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("reqwest client always builds");

        loop {
            let phase = match client.get(SENTINEL_STATUS_URL).send().await {
                Ok(resp) if resp.status().is_success() => {
                    match resp.json::<SentinelStatusRaw>().await {
                        Ok(raw) => parse_phase(raw),
                        Err(_) => SentinelPhaseLocal::Unknown,
                    }
                }
                _ => SentinelPhaseLocal::Unknown,
            };
            let _ = tx.send(phase);
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    });

    rx
}
