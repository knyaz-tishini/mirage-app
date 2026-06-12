//! Runtime chain reconciliation.
//!
//! Diffs the desired chain set (from a freshly loaded config) against the
//! running worker set and applies the minimum set of changes:
//!
//! - **New** chain id → spawn worker via `WorkerHandle::spawn` + `send_start`
//!   + `push_tun_registry`.
//! - **Removed** chain id → graceful drain (`send_drain`) + `wait_for_drained`
//!   + `kill`.
//! - **Changed** chain (same id, different `socks5_bind` or `config_path`) →
//!   stop old worker **first** (drain→kill), wait for exit, *then* spawn new.
//!   This serialisation prevents port-contention when the `socks5_bind` port
//!   is re-used (bug 16 from the capability map).
//!
//! All decisions are logged with `chain_id` so every adoption is traceable.

use std::{collections::HashMap, path::Path, time::Duration};

use tokio::time::timeout;
use tracing::{info, warn};

use crate::{
    config::{ChainConfig, Config, MonitorTransportConfig},
    state,
    tun_registry::push_tun_registry,
    worker_handle::{HandleState, WorkerHandle},
};

use mirage_chain_ipc::MonitorTransportSpec;

/// Seconds supervisor waits for `Drained` during a reconcile stop/replace.
const RECONCILE_DRAIN_SECS: u64 = 30;

/// Reconcile running workers against `new_chains`.
///
/// `handles`     — live handle map; updated in place.
/// `config`      — current loaded supervisor config (used to build ML transport
///                 specs when delegating `SetupMonitorLink` to a freshly spawned
///                 chain that has `monitor_link_host = true`).
/// `binary_path` — worker binary used for all new spawns.
/// `state_dir`   — supervisor state directory (for `last-peer-table.json`).
pub async fn reconcile(
    handles: &mut HashMap<String, WorkerHandle>,
    config: &Config,
    binary_path: &Path,
    state_dir: &Path,
) {
    let new_chains: &[ChainConfig] = &config.chains;

    // ── 1. Classify every chain id ─────────────────────────────────────────────

    let mut to_spawn: Vec<&ChainConfig> = Vec::new();
    let mut to_remove: Vec<String> = Vec::new();
    let mut to_replace: Vec<&ChainConfig> = Vec::new();

    // Check each desired chain.
    for chain in new_chains {
        match handles.get(&chain.id) {
            None => {
                // Chain id not currently running → spawn fresh.
                info!(chain_id = %chain.id, "reconcile: new chain — will spawn");
                to_spawn.push(chain);
            }
            Some(handle) if handle.state == HandleState::Exited => {
                // Handle exists but the worker already exited — treat as new.
                info!(chain_id = %chain.id, "reconcile: exited worker — will respawn");
                to_spawn.push(chain);
            }
            Some(handle) => {
                // Running handle exists. Compare config fingerprints to detect
                // same-id config changes (e.g. monitor pushes a replacement
                // chain spec under the same chain id — B3 feature).
                let new_fp = crate::worker_handle::chain_fingerprint(chain);
                let old_fp = handle.config_fingerprint;
                if old_fp != new_fp {
                    info!(
                        chain_id = %chain.id,
                        old_fingerprint = old_fp,
                        new_fingerprint = new_fp,
                        "reconcile: config changed — will drain+respawn"
                    );
                    to_replace.push(chain);
                }
                // Fingerprints match → worker is running the correct config.
            }
        }
    }

    // Check for removed chain ids (running but not in new_chains).
    let desired_ids: std::collections::HashSet<&str> =
        new_chains.iter().map(|c| c.id.as_str()).collect();
    for chain_id in handles.keys() {
        if !desired_ids.contains(chain_id.as_str()) {
            info!(chain_id = %chain_id, "reconcile: chain removed — will drain+kill");
            to_remove.push(chain_id.clone());
        }
    }

    // ── 2. Drain+kill removed workers ─────────────────────────────────────────

    for chain_id in &to_remove {
        drain_and_kill(handles, chain_id, "removed").await;
    }

    // ── 3. Drain+kill changed workers BEFORE spawning replacements ─────────────
    //
    // This ordering is the fix for bug 16 (port-contention): we stop the old
    // worker and wait for its OS process to release the port before spawning the
    // new one.

    for chain in &to_replace {
        drain_and_kill(handles, &chain.id, "changed — stopping before respawn").await;
    }

    // ── 4. Spawn new / replacement workers ────────────────────────────────────

    let mut newly_ready: Vec<String> = Vec::new();
    let spawn_list: Vec<&ChainConfig> = to_spawn.iter().copied().chain(to_replace.iter().copied()).collect();

    let initial_pt = state::load_peer_table(state_dir);

    for chain in spawn_list {
        info!(chain_id = %chain.id, "reconcile: spawning worker");
        let mut handle = match WorkerHandle::spawn(binary_path, chain) {
            Ok(h) => h,
            Err(e) => {
                warn!(chain_id = %chain.id, "reconcile: spawn failed: {e:#}");
                continue;
            }
        };

        if let Err(e) = handle.send_start(chain, initial_pt.clone()).await {
            warn!(chain_id = %chain.id, "reconcile: send_start failed: {e:#}");
            handles.insert(chain.id.clone(), handle);
            continue;
        }

        // Wait for Ready (up to 60 s) before proceeding.
        if wait_for_ready_reconcile(&mut handle).await {
            info!(chain_id = %chain.id, "reconcile: worker ready");
            newly_ready.push(chain.id.clone());
        } else {
            warn!(chain_id = %chain.id, "reconcile: worker did not reach Ready within timeout");
        }

        handles.insert(chain.id.clone(), handle);
    }

    // ── 5. Push updated TUN registry if anything changed ──────────────────────

    let any_change = !to_remove.is_empty() || !to_replace.is_empty() || !newly_ready.is_empty();
    if any_change {
        push_tun_registry(config).await;
    }

    // ── 6. Delegate SetupMonitorLink to newly-spawned ML-host worker ──────────
    //
    // If the ML-host chain was just spawned (and is now Ready), send
    // SetupMonitorLink so it picks up the MonitorLink without requiring a
    // supervisor restart.

    for chain_id in &newly_ready {
        let is_ml_host = config
            .chains
            .iter()
            .any(|c| &c.id == chain_id && c.monitor_link_host);
        if !is_ml_host {
            continue;
        }

        info!(chain_id = %chain_id, "reconcile: delegating SetupMonitorLink to newly-spawned ML-host worker");

        let ipc_specs = build_monitor_transport_specs(config);
        match serde_json::to_string(&ipc_specs) {
            Err(e) => {
                warn!(chain_id = %chain_id, "reconcile: failed to serialize monitor_transports: {e}");
            }
            Ok(transports_json) => {
                if let Some(handle) = handles.get_mut(chain_id.as_str()) {
                    if let Err(e) = handle
                        .send_setup_monitor_link(
                            vec![config.monitor_url.clone()],
                            config.monitor_api_key.clone(),
                            transports_json,
                        )
                        .await
                    {
                        warn!(chain_id = %chain_id, "reconcile: SetupMonitorLink send failed: {e:#}");
                    }
                }
            }
        }
    }
}

/// Drain+kill one worker by chain_id, waiting for exit.
/// No-op if the handle is not present or already exited.
async fn drain_and_kill(
    handles: &mut HashMap<String, WorkerHandle>,
    chain_id: &str,
    reason: &str,
) {
    let Some(handle) = handles.get_mut(chain_id) else {
        return;
    };
    if handle.state == HandleState::Exited {
        return;
    }

    info!(chain_id, reason, "reconcile: draining worker");
    if let Err(e) = handle.send_drain(RECONCILE_DRAIN_SECS).await {
        warn!(chain_id, "reconcile: send_drain failed: {e:#} — killing directly");
    } else {
        wait_for_drained_reconcile(handle).await;
    }
    handle.kill().await;
    info!(chain_id, "reconcile: worker stopped");
}

/// Wait up to `RECONCILE_DRAIN_SECS + 5` for the handle to reach Exited.
async fn wait_for_drained_reconcile(handle: &mut WorkerHandle) {
    let deadline = Duration::from_secs(RECONCILE_DRAIN_SECS + 5);
    let _ = timeout(deadline, async {
        loop {
            match handle.rx.recv().await {
                None => break,
                Some(event) => {
                    handle.apply_msg(&event.msg);
                    if handle.state == HandleState::Exited {
                        break;
                    }
                }
            }
        }
    })
    .await;
}

/// Wait up to 60 s for the handle to reach Ready. Returns `true` on success.
async fn wait_for_ready_reconcile(handle: &mut WorkerHandle) -> bool {
    let deadline = Duration::from_secs(60);
    timeout(deadline, async {
        loop {
            match handle.rx.recv().await {
                None => return false,
                Some(event) => {
                    handle.apply_msg(&event.msg);
                    if handle.state == HandleState::Ready {
                        return true;
                    }
                }
            }
        }
    })
    .await
    .unwrap_or(false)
}

/// Build the IPC MonitorTransportSpec list from supervisor config.
/// Mirrors the logic in `main.rs` SetupMonitorLink block.
fn build_monitor_transport_specs(config: &Config) -> Vec<MonitorTransportSpec> {
    let mut specs: Vec<MonitorTransportSpec> = Vec::new();

    // Wire transport from flat fields (prepended first as preferred).
    if let (Some(wire_addr), Some(wire_pubkey)) = (
        config.monitor_wire_addr.clone(),
        config.monitor_wire_pubkey_hex.clone(),
    ) {
        let mesh_id = config
            .monitor_transports
            .iter()
            .find_map(|t| {
                if let MonitorTransportConfig::Wire { mesh_id, .. } = t {
                    Some(mesh_id.clone())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "v3-staging".to_string());
        specs.push(MonitorTransportSpec::Wire {
            monitor_addr: wire_addr,
            monitor_pubkey_hex: wire_pubkey,
            mesh_id,
        });
    }

    for t in &config.monitor_transports {
        match t {
            MonitorTransportConfig::DirectWs { .. } => {}
            MonitorTransportConfig::OverChain { chain_id, monitor_addr } => {
                if let Some(chain_cfg) = config.chains.iter().find(|c| &c.id == chain_id) {
                    if let Ok(socks5) = chain_cfg.socks5_bind.parse() {
                        specs.push(MonitorTransportSpec::OverChain {
                            worker_socks5: socks5,
                            monitor_addr: *monitor_addr,
                            chain_id: chain_id.clone(),
                        });
                    }
                }
            }
            MonitorTransportConfig::Wire { monitor_addr, monitor_pubkey_hex, mesh_id } => {
                // Skip if already inserted from flat fields.
                let already = config.monitor_wire_addr.is_some()
                    && config.monitor_wire_pubkey_hex.is_some();
                if !already {
                    specs.push(MonitorTransportSpec::Wire {
                        monitor_addr: monitor_addr.clone(),
                        monitor_pubkey_hex: monitor_pubkey_hex.clone(),
                        mesh_id: mesh_id.clone(),
                    });
                }
            }
        }
    }

    specs
}

/// Reload the config from `path` and return a fresh `Config`, or `None` on error.
pub fn try_reload_config(path: &str) -> Option<Config> {
    match Config::load(path) {
        Ok(c) => Some(c),
        Err(e) => {
            warn!(path, "reconcile: config reload failed: {e:#}");
            None
        }
    }
}
