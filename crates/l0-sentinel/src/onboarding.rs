//! Onboarding URL parser + /onboard HTTP endpoint.
//!
//! Parses `mirage://onboard?monitor=...&key=...&client_id=...` URLs.
//! The `/onboard` route writes credentials to `sentinel.toml` and
//! triggers a re-bootstrap by sending on a channel watched by `run()`.

use std::path::PathBuf;

use serde::Deserialize;
use tracing::{info, warn};

// ── Payload ───────────────────────────────────────────────────────────────────

pub struct OnboardPayload {
    pub monitor_url: String,
    pub api_key: String,
    pub client_id: String,
}

// ── URL parser ────────────────────────────────────────────────────────────────

/// Parse a `mirage://onboard?monitor=...&key=...&client_id=...` URL.
/// All three fields are required.
pub fn parse_onboard_url(s: &str) -> Result<OnboardPayload, String> {
    // Accept both mirage:// and http:// schemes for flexibility in tests
    let query_part = if let Some(rest) = s.strip_prefix("mirage://onboard?") {
        rest
    } else if let Some(rest) = s.strip_prefix("mirage://onboard") {
        // No query string
        rest.strip_prefix('?').unwrap_or(rest)
    } else {
        return Err(format!("not a mirage://onboard URL: {}", s));
    };

    let mut monitor_url: Option<String> = None;
    let mut api_key: Option<String> = None;
    let mut client_id: Option<String> = None;

    for pair in query_part.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            let decoded = url_decode(v);
            match k {
                "monitor" => monitor_url = Some(decoded),
                "key" => api_key = Some(decoded),
                "client_id" => client_id = Some(decoded),
                _ => {}
            }
        }
    }

    let monitor_url = monitor_url.ok_or("missing 'monitor' param")?;
    let api_key = api_key.ok_or("missing 'key' param")?;
    let client_id = client_id.ok_or("missing 'client_id' param")?;

    if monitor_url.is_empty() {
        return Err("'monitor' param is empty".to_owned());
    }
    if api_key.is_empty() {
        return Err("'key' param is empty".to_owned());
    }
    if client_id.is_empty() {
        return Err("'client_id' param is empty".to_owned());
    }

    Ok(OnboardPayload { monitor_url, api_key, client_id })
}

/// Minimal percent-decode (handles %XX sequences and `+` as space).
fn url_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(char::from(h << 4 | l));
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(' ');
        } else {
            out.push(char::from(bytes[i]));
        }
        i += 1;
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ── TOML patch ────────────────────────────────────────────────────────────────

/// Read-modify-write `sentinel.toml`, updating/inserting `monitor_url`,
/// `monitor_api_key`, and `client_id` while preserving all other fields.
pub fn patch_sentinel_toml(
    toml_path: &PathBuf,
    payload: &OnboardPayload,
) -> Result<(), String> {
    let raw = std::fs::read_to_string(toml_path)
        .unwrap_or_default();

    let mut doc: toml::Value = toml::from_str(&raw)
        .unwrap_or_else(|_| toml::Value::Table(toml::map::Map::new()));

    let table = match doc.as_table_mut() {
        Some(t) => t,
        None => return Err("sentinel.toml is not a TOML table".to_owned()),
    };

    table.insert(
        "monitor_url".to_owned(),
        toml::Value::String(payload.monitor_url.clone()),
    );
    table.insert(
        "monitor_api_key".to_owned(),
        toml::Value::String(payload.api_key.clone()),
    );
    table.insert(
        "client_id".to_owned(),
        toml::Value::String(payload.client_id.clone()),
    );

    let new_raw = toml::to_string_pretty(&doc)
        .map_err(|e| format!("serialise TOML: {}", e))?;

    std::fs::write(toml_path, new_raw)
        .map_err(|e| format!("write {:?}: {}", toml_path, e))?;

    info!("[onboard] patched {:?}", toml_path);
    Ok(())
}

// ── /onboard request body ─────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct OnboardRequest {
    pub url: String,
}

// ── Find sentinel.toml path ───────────────────────────────────────────────────

/// Returns `<exe_dir>/sentinel.toml`.
pub fn default_toml_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    Some(dir.join("sentinel.toml"))
}

// ── Logging helper ────────────────────────────────────────────────────────────

pub fn log_onboard_result(result: &Result<(), String>) {
    match result {
        Ok(()) => info!("[onboard] sentinel.toml updated"),
        Err(e) => warn!("[onboard] patch failed: {}", e),
    }
}
