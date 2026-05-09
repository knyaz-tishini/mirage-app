//! Persistent cache of the dynamic monitor URL list pushed by the monitor as
//! `peer_lm_urls`. On startup, sentinel prefers this list over the static
//! `sentinel.toml monitor_urls`. On every config snapshot, the file is
//! atomically rewritten when the list changes.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "monitor_urls.json";

#[derive(Debug, Serialize, Deserialize)]
struct Persisted {
    urls: Vec<String>,
}

pub fn path_for(state_dir: &Path) -> PathBuf {
    state_dir.join(FILE_NAME)
}

/// Read the persisted URL list. Returns empty vec if file is missing /
/// malformed (best-effort cache).
pub fn load(state_dir: &Path) -> Vec<String> {
    let p = path_for(state_dir);
    let raw = match std::fs::read_to_string(&p) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    match serde_json::from_str::<Persisted>(&raw) {
        Ok(v) => v.urls,
        Err(e) => {
            tracing::warn!("[sentinel] {:?} parse failed: {} — ignoring", p, e);
            Vec::new()
        }
    }
}

/// Atomic-rename write of the URL list.
pub fn save(state_dir: &Path, urls: &[String]) -> anyhow::Result<()> {
    if let Err(e) = std::fs::create_dir_all(state_dir) {
        tracing::warn!("[sentinel] mkdir {:?} failed: {}", state_dir, e);
    }
    let p = path_for(state_dir);
    let tmp = p.with_extension("json.tmp");
    let body = serde_json::to_string_pretty(&Persisted { urls: urls.to_vec() })
        .context("serialize monitor_urls.json")?;
    std::fs::write(&tmp, body.as_bytes()).with_context(|| format!("write {:?}", tmp))?;
    std::fs::rename(&tmp, &p).with_context(|| format!("rename {:?} -> {:?}", tmp, p))?;
    Ok(())
}
