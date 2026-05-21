//! Stable sentry_id flow — one UUID per physical box, persisted under
//! `${state_dir}/sentry-id.txt`. Generated once on first boot; reused
//! verbatim on every subsequent register so the same box ends up with
//! the SAME `client_id` across every LM it talks to.
//!
//! Pattern lifted from the deprecated `crates/_deprecated/sentry/` —
//! kept lean: one read, one (optional) atomic write.

use std::path::Path;

use anyhow::{Context, Result};

pub const FILE_NAME: &str = "sentry-id.txt";

/// Read or generate a persistent sentry_id under `state_dir`.
///
/// On first call: generates UUID v4, writes `<state_dir>/sentry-id.txt`
/// atomically (tmp + rename). On every subsequent call: returns the same
/// id from disk.
pub fn ensure_sentry_id(state_dir: &Path) -> Result<String> {
    let path = state_dir.join(FILE_NAME);
    if let Ok(raw) = std::fs::read_to_string(&path) {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("create state_dir {:?}", state_dir))?;
    let id = uuid::Uuid::new_v4().to_string();
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &id).with_context(|| format!("write {:?}", tmp))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("rename {:?} -> {:?}", tmp, path))?;
    Ok(id)
}
