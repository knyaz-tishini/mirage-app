//! Background task that polls for OTA updates at a configurable interval.

use mirage_chain_ota::{fetch_manifest, is_newer, platform_key};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

/// Notification sent when a new version is available.
#[derive(Debug, Clone)]
pub struct UpdateAvailable {
    pub version: String,
    pub monitor_url: String,
    pub ota_pubkey: String,
}

/// Spawn the OTA poller task.
///
/// Sends an [`UpdateAvailable`] on `tx` when the manifest reports a version
/// newer than `current_version`. Sends at most once per discovered version
/// (does not repeat the same version).
pub fn spawn_ota_poller(
    monitor_url: String,
    ota_pubkey: String,
    poll_interval_secs: u64,
    current_version: String,
    tx: mpsc::Sender<UpdateAvailable>,
) {
    tokio::spawn(async move {
        let interval = tokio::time::Duration::from_secs(poll_interval_secs);
        let mut last_notified: Option<String> = None;

        loop {
            tokio::time::sleep(interval).await;

            match fetch_manifest(&monitor_url).await {
                Err(e) => {
                    warn!("OTA manifest fetch failed: {e:#}");
                }
                Ok(manifest) => {
                    let key = platform_key();
                    if manifest.platforms.get(key).is_none() {
                        warn!(
                            platform = %key,
                            "no artifact for this platform in manifest version {}",
                            manifest.version
                        );
                        continue;
                    }

                    if !is_newer(&current_version, &manifest.version) {
                        info!(
                            current = %current_version,
                            manifest = %manifest.version,
                            "OTA: up-to-date"
                        );
                        continue;
                    }

                    // Only notify once per discovered version.
                    if last_notified.as_deref() == Some(&manifest.version) {
                        continue;
                    }

                    info!(
                        current = %current_version,
                        new = %manifest.version,
                        "OTA update available"
                    );
                    last_notified = Some(manifest.version.clone());

                    let notif = UpdateAvailable {
                        version: manifest.version,
                        monitor_url: monitor_url.clone(),
                        ota_pubkey: ota_pubkey.clone(),
                    };
                    if tx.send(notif).await.is_err() {
                        error!("OTA poller: receiver dropped — stopping");
                        break;
                    }
                }
            }
        }
    });
}
