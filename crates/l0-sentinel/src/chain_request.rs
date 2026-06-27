//! Register helpers used by sentinel boot.
//!
//! Sentinel is a passive participant after register — the monitor pushes
//! chains via WS. This module owns the failover register + client_id
//! persistence path; chain request / touch endpoints live on the monitor
//! and are dialed by operator/debug UIs directly, not by sentinel.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

/// Wire coordinates for register-over-wire (R1).
///
/// Supplied by the sentinel at startup when `monitor_wire_addr` and
/// `monitor_wire_pubkey_hex` are present in config or `monitor_wire.json`
/// was persisted from a previous run.
pub struct WireRegisterCoords<'a> {
    pub monitor_addr: &'a str,
    pub monitor_pubkey_hex: &'a str,
    pub mesh_id: &'a str,
}

/// Body for `POST /relay/clients` (register).
#[derive(Debug, Serialize)]
pub struct RegisterBody<'a> {
    pub sentinel_id: &'a str,
    pub role: &'a str,
    pub shortname: &'a str,
    pub os: &'a str,
    pub version: &'a str,
    pub supported_transports: Vec<&'a str>,
}

/// Register the sentinel with the mesh over the Mirage **native wire**
/// (`:19444`, V9, mutual-auth + AEAD).  `wire_coords` MUST be present: the
/// legacy HTTPS register path (`POST /relay/clients` on the monitor's :19451
/// sidecar) was retired when the full HTTPS sidecar was closed
/// (`bind_tls_sidecar = false`).  opbox now bakes `monitor_wire_addr` +
/// `monitor_wire_pubkey_hex` into every issued client config (`/client/issue`),
/// and the sentinel self-heals fresh coords from the peer_table over the live
/// WS — so there is nothing left for an HTTPS fallback to do.
///
/// Returns `(active_url, client_id, client_token)`.  `active_url` is a
/// `wire://<addr>` marker (MonitorLink uses the Wire transport for every
/// subsequent call).  `client_token` is `None` on monitors that do not issue
/// tokens — callers treat absence as backward-compat (no token in subscribe).
pub async fn register_with_failover(
    body: &RegisterBody<'_>,
    wire_coords: Option<&WireRegisterCoords<'_>>,
) -> Result<(String, String, Option<String>)> {
    let wc = wire_coords.ok_or_else(|| {
        anyhow::anyhow!(
            "register: no monitor wire coords in config — reissue the client \
             config from opbox (legacy HTTPS register retired)"
        )
    })?;

    let body_json = serde_json::to_vec(body).context("serialize register body for wire")?;

    let (client_id, _poll_secs, client_token) =
        mirage_client_core::monitor_link::register_post_over_wire(
            wc.monitor_addr,
            wc.monitor_pubkey_hex,
            wc.mesh_id,
            &body_json,
        )
        .await
        .map_err(|e| anyhow::anyhow!("register over wire (:19444): {e}"))?;

    tracing::info!("[sentinel] registered over wire: client_id={client_id}");
    Ok((format!("wire://{}", wc.monitor_addr), client_id, client_token))
}

/// Persist `client_id` under `${state_dir}/client-id.txt` so a restart can
/// confirm the registered id without a fresh round-trip to the monitor.
pub fn persist_client_id(state_dir: &Path, client_id: &str) -> Result<()> {
    let path = state_dir.join("client-id.txt");
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, client_id).with_context(|| format!("write {:?}", tmp))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("rename {:?} -> {:?}", tmp, path))?;
    Ok(())
}

/// Persist `client_token` under `${state_dir}/client-token.txt` (atomic).
/// Idempotent: re-persists on every register so a fresh token from a new
/// monitor always wins. Callers should treat write failure as non-fatal.
pub fn persist_client_token(state_dir: &Path, token: &str) -> Result<()> {
    let path = state_dir.join("client-token.txt");
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, token).with_context(|| format!("write {:?}", tmp))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("rename {:?} -> {:?}", tmp, path))?;
    Ok(())
}

/// Load a previously persisted `client_token` from `${state_dir}/client-token.txt`.
/// Returns `None` if the file is absent, empty, or unreadable.
pub fn load_client_token(state_dir: &Path) -> Option<String> {
    let path = state_dir.join("client-token.txt");
    let s = std::fs::read_to_string(&path).ok()?;
    let s = s.trim().to_owned();
    if s.is_empty() { None } else { Some(s) }
}
