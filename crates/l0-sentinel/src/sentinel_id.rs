//! Stable sentinel_id flow — one UUID per physical box, persisted under
//! `${state_dir}/sentinel-id.txt`. Generated once on first boot; reused
//! verbatim on every subsequent register so the same box ends up with
//! the SAME `client_id` across every LM it talks to.
//!
//! Same pattern as monitor/node/mshd: UUID v4 minted once, file-based.

use std::path::Path;

use anyhow::{Context, Result};

pub const FILE_NAME: &str = "sentinel-id.txt";

/// Read or generate a persistent sentinel_id under `state_dir`.
///
/// On first call: generates UUID v4, writes `<state_dir>/sentinel-id.txt`
/// atomically (tmp + rename). On every subsequent call: returns the same
/// id from disk.
pub fn ensure_sentinel_id(state_dir: &Path) -> Result<String> {
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

/// Adopt an operator-issued id as this box's sentinel_id, overwriting whatever
/// was there.
///
/// Called only by an access packet's `apply` — and it has to exist, because the
/// id the packet carries is the one the credential in that same packet was
/// minted against. `ensure_sentinel_id` mints its own UUID on a fresh box, and
/// a box that mints its own id registers as somebody the issuer never heard of:
/// the monitor sees a first registration of an unknown id, mints a token for
/// it, and the token the packet carries belongs to a different row. Everything
/// downstream then fails in a way that reads like a credential problem and is
/// an identity problem.
///
/// Overwrites rather than refusing an existing file, because the packet IS the
/// issuance: handing a box a new packet is how a subscriber is re-issued, and a
/// stale self-minted id must not outrank it.
pub fn adopt_sentinel_id(state_dir: &Path, id: &str) -> Result<()> {
    let id = id.trim();
    if id.is_empty() {
        anyhow::bail!("adopt_sentinel_id: refusing to adopt an empty id");
    }
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("create state_dir {:?}", state_dir))?;
    let path = state_dir.join(FILE_NAME);
    let tmp = path.with_extension("adopt-tmp");
    std::fs::write(&tmp, id).with_context(|| format!("write {:?}", tmp))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("rename {:?} -> {:?}", tmp, path))?;
    Ok(())
}

#[cfg(test)]
mod adopt_tests {
    use super::*;

    #[test]
    fn adopt_overrides_a_self_minted_id_and_survives_a_reread() {
        let dir = std::env::temp_dir().join(format!("sentinel-adopt-{}", uuid::Uuid::new_v4()));
        let minted = ensure_sentinel_id(&dir).expect("mint");
        let issued = "11111111-2222-3333-4444-555555555555";
        assert_ne!(minted, issued);
        adopt_sentinel_id(&dir, issued).expect("adopt");
        assert_eq!(ensure_sentinel_id(&dir).expect("reread"), issued);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_id_is_refused_rather_than_written() {
        let dir = std::env::temp_dir().join(format!("sentinel-adopt-empty-{}", uuid::Uuid::new_v4()));
        assert!(adopt_sentinel_id(&dir, "   ").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
