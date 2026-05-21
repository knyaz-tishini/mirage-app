//! Stage 3 client-facing chain provisioning helpers.
//!
//! The sentinel uses these to:
//!   - register with the monitor under its stable `sentry_id`;
//!   - on first WS connect, POST an axis-spec to
//!     `/relay/clients/{client_id}/chains/request` so the monitor builds
//!     a chain we can dial;
//!   - periodically POST `/touch` so the chain's idle-TTL doesn't expire.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

/// Axis-spec body posted to `/relay/clients/{client_id}/chains/request`.
///
/// Mirror of `mirage2monitor::auto_chain::AxisSpec`; kept duplicated here
/// (without depending on the monitor crate) so the sentinel can ship as a
/// standalone binary without dragging the server side in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AxisRequest {
    pub transport: String,
    pub camouflage: String,
    pub hops: u8,
    pub relay_mode: String,
    pub exit_mode: String,
    #[serde(default = "default_aead")]
    pub aead_suite: String,
    #[serde(default = "default_bc")]
    pub byte_cipher: String,
    #[serde(default)]
    pub anchor: AxisAnchor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AxisAnchor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_node_id: Option<String>,
}

fn default_aead() -> String { "cha_cha20_poly1305".to_string() }
fn default_bc() -> String { "none".to_string() }

#[derive(Debug, Clone, Deserialize)]
pub struct ChainRequestResponse {
    pub chain_id: String,
    pub expires_at_ms: u64,
    #[serde(default)]
    pub cached: bool,
}

/// Body for `POST /relay/clients` (register).
#[derive(Debug, Serialize)]
pub struct RegisterBody<'a> {
    pub sentry_id: &'a str,
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
}

/// Try every URL in `monitor_urls` in order, return the first one that
/// accepts our registration. On every success the returned `client_id`
/// should match `sentry_id` (post May-2026 monitor accepts caller id
/// verbatim). On every failure we move on to the next URL.
pub async fn register_with_failover(
    http: &Client,
    monitor_urls: &[String],
    bearer: &str,
    body: &RegisterBody<'_>,
) -> Result<(String, String)> {
    let mut last_err: Option<anyhow::Error> = None;
    for url in monitor_urls {
        let endpoint = format!("{}/relay/clients", url.trim_end_matches('/'));
        let req = http.post(&endpoint).bearer_auth(bearer).json(body);
        match req.send().await {
            Ok(r) if r.status().is_success() => match r.json::<RegisterResponse>().await {
                Ok(resp) => return Ok((url.clone(), resp.client_id)),
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

/// POST the axis-spec; returns the assigned chain_id + hard expiry.
pub async fn request_chain(
    http: &Client,
    base_url: &str,
    client_id: &str,
    bearer: &str,
    axis: &AxisRequest,
) -> Result<ChainRequestResponse> {
    let endpoint = format!(
        "{}/relay/clients/{}/chains/request",
        base_url.trim_end_matches('/'),
        client_id
    );
    let resp = http
        .post(&endpoint)
        .bearer_auth(bearer)
        .json(axis)
        .send()
        .await
        .with_context(|| format!("POST {endpoint}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("chains/request returned {status}: {body}");
    }
    Ok(resp.json::<ChainRequestResponse>().await?)
}

/// `POST /relay/clients/{client_id}/chains/{chain_id}/touch` — bumps the
/// idle TTL. Returns `Ok(())` on 204, `Err` on anything else.
pub async fn touch_chain(
    http: &Client,
    base_url: &str,
    client_id: &str,
    chain_id: &str,
    bearer: &str,
) -> Result<()> {
    let endpoint = format!(
        "{}/relay/clients/{}/chains/{}/touch",
        base_url.trim_end_matches('/'),
        client_id,
        chain_id
    );
    let resp = http
        .post(&endpoint)
        .bearer_auth(bearer)
        .send()
        .await
        .with_context(|| format!("POST {endpoint}"))?;
    if resp.status().is_success() {
        Ok(())
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("touch returned {status}: {body}");
    }
}

/// Spawn a background task that touches `chain_id` every `interval`.
///
/// The handle stops when `cancel` flips to `true` or the task panics; on
/// failed touches it logs at warn level but keeps trying — short-lived
/// network blips are normal and the next tick will recover.
pub fn spawn_touch_loop(
    http: Client,
    base_url: String,
    client_id: String,
    chain_id: String,
    bearer: String,
    interval: Duration,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.tick().await; // skip immediate first
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if let Err(e) = touch_chain(
                        &http, &base_url, &client_id, &chain_id, &bearer,
                    ).await {
                        tracing::warn!(chain_id = %chain_id, error = %e, "touch failed");
                    }
                }
                _ = cancel.changed() => {
                    if *cancel.borrow() {
                        tracing::debug!(chain_id = %chain_id, "touch loop cancelled");
                        return;
                    }
                }
            }
        }
    });
}

/// Persist `client_id` under `${state_dir}/client-id.txt` so a restart can
/// skip re-register when the sentry_id flow already settled on a value.
/// Idempotent; safe to call on every boot.
pub fn persist_client_id(state_dir: &Path, client_id: &str) -> Result<()> {
    let path = state_dir.join("client-id.txt");
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, client_id).with_context(|| format!("write {:?}", tmp))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("rename {:?} -> {:?}", tmp, path))?;
    Ok(())
}
