//! Persistent cache of the monitor's native-wire coordinates
//! (`monitor_wire_addr` + `monitor_wire_pubkey_hex` + `mesh_id`).
//!
//! On every config snapshot that carries a monitor peer_table entry the
//! sentinel extracts and persists the wire coords so a restart can activate
//! `MonitorTransport::Wire` without waiting for the first WS push.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "monitor_wire.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireCoords {
    pub monitor_addr: String,
    pub monitor_pubkey_hex: String,
    pub mesh_id: String,
}

pub fn path_for(state_dir: &Path) -> PathBuf {
    state_dir.join(FILE_NAME)
}

/// Read the persisted wire coords. Returns `None` if the file is missing /
/// malformed (best-effort cache — fall back to no-Wire).
pub fn load(state_dir: &Path) -> Option<WireCoords> {
    let p = path_for(state_dir);
    let raw = std::fs::read_to_string(&p).ok()?;
    match serde_json::from_str::<WireCoords>(&raw) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!("[sentinel] {:?} parse failed: {} — ignoring", p, e);
            None
        }
    }
}

/// Atomic-rename write of wire coords.
pub fn save(state_dir: &Path, coords: &WireCoords) -> anyhow::Result<()> {
    if let Err(e) = std::fs::create_dir_all(state_dir) {
        tracing::warn!("[sentinel] mkdir {:?} failed: {}", state_dir, e);
    }
    let p = path_for(state_dir);
    let tmp = p.with_extension("json.tmp");
    let body = serde_json::to_string_pretty(coords).context("serialize monitor_wire.json")?;
    std::fs::write(&tmp, body.as_bytes()).with_context(|| format!("write {:?}", tmp))?;
    std::fs::rename(&tmp, &p).with_context(|| format!("rename {:?} -> {:?}", tmp, p))?;
    Ok(())
}

/// Extract the monitor's wire entry from a peer_table snapshot.
///
/// Looks for the first entry whose:
/// - `labels` contains `"monitor"`, AND
/// - `addr` ends with `:19444`
///
/// Returns the extracted `WireCoords` when found, `None` otherwise.
pub fn extract_from_peer_table(
    peer_table: &mirage_core::mirage::mesh::PeerTable,
    mesh_id: &str,
) -> Option<WireCoords> {
    for entry in &peer_table.peers {
        let is_monitor = entry.labels.iter().any(|l| l == "monitor");
        let is_wire_port = entry.addr.ends_with(":19444");
        if is_monitor && is_wire_port {
            return Some(WireCoords {
                monitor_addr: entry.addr.clone(),
                monitor_pubkey_hex: entry.pubkey_hex.clone(),
                mesh_id: mesh_id.to_owned(),
            });
        }
    }
    None
}
