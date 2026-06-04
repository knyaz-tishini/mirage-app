//! Register helpers used by sentinel boot.
//!
//! Sentinel is a passive participant after register — the monitor pushes
//! chains via WS. This module owns the failover register + client_id
//! persistence path; chain request / touch endpoints live on the monitor
//! and are dialed by operator/debug UIs directly, not by sentinel.

use std::path::Path;

use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

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

#[derive(Debug, Deserialize)]
pub struct RegisterResponse {
    pub client_id: String,
    #[serde(default)]
    pub monitor_poll_interval_secs: Option<u64>,
    /// Per-client token issued once at registration (Phase 2 monitor feature).
    /// Present only when the monitor supports `require_client_token`.
    /// Old monitors return `None` — callers must treat absence as backward-compat.
    #[serde(default)]
    pub client_token: Option<String>,
}

/// Try every URL in `monitor_urls` in order, return the first one that
/// accepts our registration. On every success the returned `client_id`
/// should match `sentinel_id` (post May-2026 monitor accepts caller id
/// verbatim). On every failure we move on to the next URL.
///
/// Returns `(active_url, client_id, client_token)`. `client_token` is
/// `None` on older monitors that do not issue tokens yet — callers must
/// treat absence as backward-compat (no token in subscribe params).
///
/// When `wire_coords` is `Some`, the function tries `POST /relay/clients`
/// over the Mirage native wire (`:19444`, V9, no bearer) **first** (R1).
/// On wire success the HTTPS URL loop is skipped entirely.  On wire
/// failure the code falls through to the HTTPS URL loop unchanged —
/// backward-compatible cold-bootstrap path.
pub async fn register_with_failover(
    http: &Client,
    monitor_urls: &[String],
    bearer: &str,
    body: &RegisterBody<'_>,
    wire_coords: Option<&WireRegisterCoords<'_>>,
) -> Result<(String, String, Option<String>)> {
    // R1: Wire-preferred register.
    if let Some(wc) = wire_coords {
        let body_json = match serde_json::to_vec(body) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("[sentinel] register: serialize body for wire: {e}");
                vec![]
            }
        };
        if !body_json.is_empty() {
            match mirage_client_core::monitor_link::register_post_over_wire(
                wc.monitor_addr,
                wc.monitor_pubkey_hex,
                wc.mesh_id,
                &body_json,
            )
            .await
            {
                Ok((client_id, _poll_secs, client_token)) => {
                    tracing::info!(
                        "[sentinel] registered over wire (R1): client_id={client_id}"
                    );
                    // Use the wire addr as the active "URL" so callers have a
                    // non-empty string.  The actual HTTPS URL is irrelevant once
                    // MonitorLink uses Wire transport for all subsequent calls.
                    let wire_url =
                        format!("wire://{}", wc.monitor_addr);
                    return Ok((wire_url, client_id, client_token));
                }
                Err(e) => {
                    tracing::warn!(
                        "[sentinel] register wire failed ({e}) — falling back to HTTPS"
                    );
                }
            }
        }
    }

    // HTTPS failover (unchanged original path).
    let mut last_err: Option<anyhow::Error> = None;
    for url in monitor_urls {
        let endpoint = format!("{}/relay/clients", url.trim_end_matches('/'));
        let req = http.post(&endpoint).bearer_auth(bearer).json(body);
        match req.send().await {
            Ok(r) if r.status().is_success() => match r.json::<RegisterResponse>().await {
                Ok(resp) => return Ok((url.clone(), resp.client_id, resp.client_token)),
                Err(e) => last_err = Some(anyhow::anyhow!("decode {url}: {e}")),
            },
            Ok(r) => {
                last_err = Some(anyhow::anyhow!(
                    "register at {url} returned HTTP {}: {}",
                    r.status(),
                    r.text().await.unwrap_or_default()
                ));
            }
            Err(e) => last_err = Some(anyhow::anyhow!("send to {url}: {e}")),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no monitor URLs configured")))
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
