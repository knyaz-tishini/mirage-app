//! Register helpers used by sentinel boot.
//!
//! Sentinel is a passive participant after register — the monitor pushes
//! chains via WS. This module owns the failover register + client_id
//! persistence path; chain request / touch endpoints live on the monitor
//! and are dialed by operator/debug UIs directly, not by sentinel.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

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

impl<'a> WireRegisterCoords<'a> {
    /// Identifies this monitor in the per-monitor `client-tokens.json` store
    /// (see [`persist_client_token`] / [`load_client_token`]).
    ///
    /// Prefers `monitor_pubkey_hex` — the monitor's static wire public key is
    /// that monitor's own durable identity: it survives a DNS rebind, a NAT
    /// remap, a load-balancer move, or the process landing on a new host
    /// after a re-deploy. `monitor_addr` (`ip:port`) is the WEAKER key — an
    /// address can be reused by an entirely different monitor process after
    /// a re-deploy or mesh reshuffle, which would silently hand our token to
    /// that new monitor's registry. `monitor_pubkey_hex` is a plain `&str`
    /// (not `Option`), so an empty string is the only "absent" signal and is
    /// the sole case that falls back to `monitor_addr`.
    pub fn token_key(&self) -> &'a str {
        if self.monitor_pubkey_hex.trim().is_empty() {
            self.monitor_addr
        } else {
            self.monitor_pubkey_hex
        }
    }
}

/// Body for `POST /relay/clients` (register).
///
/// `client_token` mirrors `mirage_shared_types::ClientRegistration::client_token`
/// (field name + `skip_serializing_if` shape kept identical): it is the
/// ownership proof for a RE-registration of a `sentinel_id` the monitor
/// already has a row for. `None` on a first registration — the field is then
/// omitted from the wire body entirely rather than sent as `null`.
#[derive(Debug, Serialize)]
pub struct RegisterBody<'a> {
    pub sentinel_id: &'a str,
    pub role: &'a str,
    pub shortname: &'a str,
    pub os: &'a str,
    pub version: &'a str,
    pub supported_transports: Vec<&'a str>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_token: Option<&'a str>,
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
///
/// On a `401` refusal, the returned `anyhow::Error`'s message carries the
/// monitor's `HTTP 401` status (see [`is_unauthorized`]) — callers that sent
/// a stored `body.client_token` should treat that combination as a stale
/// token, not a transient failure.
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

/// True if `err`'s message indicates the monitor refused `POST /relay/clients`
/// with `401 Unauthorized` — the `register_wire: HTTP 401: ...` signature
/// bubbled up through [`register_with_failover`] from
/// `mirage_client_core::monitor_link::register_post_over_wire`.
///
/// That helper returns a plain `Result<_, String>` with no structured status
/// code, so this is a substring check on the wrapped message rather than a
/// typed match — the alternative is threading an HTTP status across the
/// `mirage-client-core` crate boundary, out of scope here. Callers use this
/// to tell "the stored `client_token` is stale" apart from every other
/// register failure (network, parse error, a non-auth HTTP status).
pub fn is_unauthorized(err: &anyhow::Error) -> bool {
    err.to_string().contains("HTTP 401")
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

// ── Per-monitor client_token store ───────────────────────────────────────────
//
// `${state_dir}/client-tokens.json` — a JSON object mapping a monitor's
// `WireRegisterCoords::token_key()` to the token THAT monitor most recently
// issued for this sentinel_id.
//
// One token per monitor, never one token overall: each monitor mints its OWN
// token per client_id at register time, and `PeerEvent::ClientUpsert` does
// not replicate `token_hash` between monitors (mirage2monitor@a5401b1). A
// single shared file (the pre-existing `client-token.txt`) meant the token
// minted by monitor B destroyed the entry monitor A had just issued for the
// same physical sentinel — A would then refuse this sentinel's next register
// forever, since it now enforces token possession on re-registration of a
// `sentinel_id` it already knows.

const TOKENS_FILE: &str = "client-tokens.json";

/// Legacy single-monitor store this replaces. Read once for migration (see
/// [`read_tokens`]), never written to again.
const LEGACY_TOKEN_FILE: &str = "client-token.txt";

fn tokens_path(state_dir: &Path) -> PathBuf {
    state_dir.join(TOKENS_FILE)
}

fn write_tokens(state_dir: &Path, tokens: &HashMap<String, String>) -> Result<()> {
    let path = tokens_path(state_dir);
    let tmp = path.with_extension("tmp");
    let body = serde_json::to_string_pretty(tokens).context("serialize client-tokens.json")?;
    std::fs::write(&tmp, body.as_bytes()).with_context(|| format!("write {:?}", tmp))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("rename {:?} -> {:?}", tmp, path))?;
    Ok(())
}

/// Load the whole keyed token store.
///
/// One-way compatibility migration: if `client-tokens.json` is absent AND the
/// legacy `client-token.txt` is present and non-empty, its content is
/// imported under `migrating_key` (the monitor currently being registered
/// with) and written out as the new keyed store — from then on only
/// `client-tokens.json` is read or written; the legacy file is left on disk
/// untouched but never consulted again. If `client-tokens.json` already
/// exists (even empty or malformed), the legacy file is not read at all —
/// the keyed store already won that box.
fn read_tokens(state_dir: &Path, migrating_key: &str) -> HashMap<String, String> {
    let path = tokens_path(state_dir);
    match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
            tracing::warn!("[sentinel] {:?} parse failed: {} — treating as empty", path, e);
            HashMap::new()
        }),
        Err(_) => {
            let legacy = state_dir.join(LEGACY_TOKEN_FILE);
            let Ok(raw) = std::fs::read_to_string(&legacy) else {
                return HashMap::new();
            };
            let token = raw.trim();
            if token.is_empty() {
                return HashMap::new();
            }
            let mut migrated = HashMap::new();
            migrated.insert(migrating_key.to_owned(), token.to_owned());
            match write_tokens(state_dir, &migrated) {
                Ok(()) => tracing::info!(
                    "[sentinel] migrated legacy {:?} into {:?} under monitor={}",
                    legacy, path, migrating_key
                ),
                Err(e) => tracing::warn!(
                    "[sentinel] migrate {:?} -> {:?} failed: {} (non-fatal)",
                    legacy, path, e
                ),
            }
            migrated
        }
    }
}

/// Persist `token` under `monitor_key` in the per-monitor store (atomic:
/// tmp + rename). Touches only `monitor_key`'s entry — every other monitor's
/// stored token is read back unchanged and rewritten as-is.
pub fn persist_client_token(state_dir: &Path, monitor_key: &str, token: &str) -> Result<()> {
    let mut tokens = read_tokens(state_dir, monitor_key);
    tokens.insert(monitor_key.to_owned(), token.to_owned());
    write_tokens(state_dir, &tokens)
}

/// Load the token stored for `monitor_key`. Returns `None` if the store (or
/// the migrated-in legacy file) has nothing under this key — including a
/// monitor being dialled for the first time.
pub fn load_client_token(state_dir: &Path, monitor_key: &str) -> Option<String> {
    read_tokens(state_dir, monitor_key).get(monitor_key).cloned()
}

/// Drop the stored token for `monitor_key`.
///
/// Used when the monitor refuses a register call with `401` while we sent
/// this token ([`is_unauthorized`]): the row it minted for was reset or
/// re-minted elsewhere on that monitor, and the stale token will keep being
/// refused. Removing it makes the NEXT register attempt for this monitor go
/// out as a first-timer (no `client_token` field) instead of repeating the
/// same refused proof.
pub fn drop_client_token(state_dir: &Path, monitor_key: &str) -> Result<()> {
    let mut tokens = read_tokens(state_dir, monitor_key);
    if tokens.remove(monitor_key).is_some() {
        write_tokens(state_dir, &tokens)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_token_round_trips_through_the_keyed_store() {
        let dir = tempfile::tempdir().unwrap();
        persist_client_token(dir.path(), "mon-a", "tok-a").unwrap();
        assert_eq!(load_client_token(dir.path(), "mon-a"), Some("tok-a".to_owned()));
    }

    #[test]
    fn two_monitors_coexist_without_clobbering_each_other() {
        let dir = tempfile::tempdir().unwrap();
        persist_client_token(dir.path(), "mon-a", "tok-a").unwrap();
        persist_client_token(dir.path(), "mon-b", "tok-b").unwrap();

        assert_eq!(load_client_token(dir.path(), "mon-a"), Some("tok-a".to_owned()));
        assert_eq!(load_client_token(dir.path(), "mon-b"), Some("tok-b".to_owned()));

        // Re-persisting A's token must not touch B's entry.
        persist_client_token(dir.path(), "mon-a", "tok-a-2").unwrap();
        assert_eq!(load_client_token(dir.path(), "mon-a"), Some("tok-a-2".to_owned()));
        assert_eq!(load_client_token(dir.path(), "mon-b"), Some("tok-b".to_owned()));
    }

    #[test]
    fn drop_client_token_removes_only_its_own_key() {
        let dir = tempfile::tempdir().unwrap();
        persist_client_token(dir.path(), "mon-a", "tok-a").unwrap();
        persist_client_token(dir.path(), "mon-b", "tok-b").unwrap();

        drop_client_token(dir.path(), "mon-a").unwrap();

        assert_eq!(load_client_token(dir.path(), "mon-a"), None);
        assert_eq!(load_client_token(dir.path(), "mon-b"), Some("tok-b".to_owned()));
    }

    #[test]
    fn legacy_single_file_migrates_under_the_current_monitor_key() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("client-token.txt"), "legacy-tok").unwrap();

        // client-tokens.json is absent -> migrate under "mon-new".
        assert_eq!(load_client_token(dir.path(), "mon-new"), Some("legacy-tok".to_owned()));

        // The migration wrote the keyed store to disk.
        let raw = std::fs::read_to_string(dir.path().join("client-tokens.json")).unwrap();
        let map: HashMap<String, String> = serde_json::from_str(&raw).unwrap();
        assert_eq!(map.get("mon-new"), Some(&"legacy-tok".to_owned()));

        // The legacy file is left on disk untouched, but is never written again.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("client-token.txt")).unwrap(),
            "legacy-tok"
        );
    }

    #[test]
    fn keyed_store_wins_when_both_files_are_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("client-token.txt"), "legacy-tok").unwrap();
        let mut existing = HashMap::new();
        existing.insert("mon-a".to_owned(), "fresh-tok".to_owned());
        write_tokens(dir.path(), &existing).unwrap();

        // client-tokens.json already exists -> legacy file is never consulted.
        assert_eq!(load_client_token(dir.path(), "mon-a"), Some("fresh-tok".to_owned()));
        assert_eq!(load_client_token(dir.path(), "mon-new"), None);
    }

    #[test]
    fn register_body_with_no_stored_token_omits_the_field_entirely() {
        let body = RegisterBody {
            sentinel_id: "s-1",
            role: "sentry-client",
            shortname: "s-1",
            os: "windows",
            version: "0.1.0",
            supported_transports: vec!["tcp", "udp"],
            client_token: None,
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(!json.contains("client_token"), "json={json}");
    }

    #[test]
    fn register_body_with_a_stored_token_serialises_it() {
        let body = RegisterBody {
            sentinel_id: "s-1",
            role: "sentry-client",
            shortname: "s-1",
            os: "windows",
            version: "0.1.0",
            supported_transports: vec!["tcp", "udp"],
            client_token: Some("tok-a"),
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("\"client_token\":\"tok-a\""), "json={json}");
    }
}
