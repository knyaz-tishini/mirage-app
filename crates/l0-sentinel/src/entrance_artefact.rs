//! The commercial artefact branch — T1.1.
//!
//! `docs/mirage/plans/thin-customer-connector-2026-09-17.md` §4's own
//! placement decision: `l0-sentinel`'s `run()` gains a branch, parallel to
//! its existing `access_packet` consumption, that fires when the file at
//! `access_path` is shaped like an `EntranceArtefact` (the commercial
//! customer's delivery unit) rather than the legacy mesh-participant
//! `AccessPacket`. On that branch sentinel skips `MonitorLink` construction
//! entirely — a commercial subscriber never talks to a monitor — and instead
//! spawns `mirage-subscriber-client` the same way it spawns the cli2
//! supervisor today: [`crate::supervisor::spawn_child_with_flags_and_mirror`],
//! with a restart policy.
//!
//! This module owns two things:
//! 1. **Detection + materialisation** — [`detect_kind`] tells an
//!    `EntranceArtefact` from an `AccessPacket` BY SHAPE (which
//!    discriminating field is present), never by filename; [`materialise`]
//!    then writes the validated artefact to sentinel's own managed path —
//!    the only "local config" the connector needs, since the connector's
//!    entire input is the artefact file itself
//!    (`mirage_subscriber_client::bin::mirage-subscriber-client`'s `--artefact`
//!    flag).
//! 2. **The spawn branch** — [`build_connector_args`] builds the connector's
//!    argv, [`resolve_connector_binary`] finds the binary next to this one,
//!    and [`run`] spawns and supervises it with a rate-limited restart
//!    policy, the same shape sentinel's own cli respawn window already uses
//!    (`crate::supervisor::cli_may_respawn`: 5 respawns / 60s).
//!
//! **Parsing and validation are never duplicated here.** [`detect_kind`] only
//! sniffs which shape a file carries; the actual parse
//! (`mirage_subscriber_client::EntranceArtefact::from_json`) and validation
//! (`EntranceArtefact::validate`) are the SAME functions
//! `mirage-subscriber-client`'s own binary calls on the artefact it receives
//! — this crate depends on that crate's lib target rather than restating its
//! shape, so a malformed artefact is refused with the exact same named error
//! (`mirage_subscriber_client::ArtefactError`, e.g. "field
//! 'private_key_hex' is not valid hex") whether the refusal happens here or
//! in the connector itself.
//!
//! **The legacy `AccessPacket` path is untouched.** [`DeliveryKind::Packet`]
//! routes straight to the existing `access_packet::AccessPacket::from_json` /
//! `apply` discipline — this module never constructs or mutates an
//! `AccessPacket`.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use mirage_subscriber_client::EntranceArtefact;
use thiserror::Error;
use tracing::{error, info, warn};

use crate::supervisor::{self, CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW};

/// Filename (under `state_dir`) sentinel materialises the validated artefact
/// to before handing it to the connector. A stable, sentinel-owned path
/// (rather than passing `access_path` straight through) so a restart finds
/// the same file even if the originally-delivered artefact was moved or
/// deleted after being consumed — the same "disk-cached managed state"
/// convention `access_packet.rs`'s own files already follow.
const MATERIALISED_FILE: &str = "entrance-artefact-managed.json";

/// Default local SOCKS5 bind the connector is spawned with when
/// `SentinelConfig::subscriber_client_bind` is left at its default —
/// loopback-only, matching the connector's own refusal of any non-loopback
/// bind (`mirage-subscriber-client.rs`'s `--bind` check).
pub const DEFAULT_CONNECTOR_BIND: &str = "127.0.0.1:11080";

/// The full path sentinel materialises the validated artefact to.
pub fn materialised_path(state_dir: &Path) -> PathBuf {
    state_dir.join(MATERIALISED_FILE)
}

/// Which delivery shape a file on disk carries. Decided by which
/// discriminating, required-only-on-that-shape field is present — never by
/// filename, never by the shared `v`/`mesh_id`/`issued_at_ms`/`label` fields
/// both shapes happen to carry. See [`detect_kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryKind {
    /// Carries `entrances` (flat list of dialable faces) and no `monitor` —
    /// the commercial customer's `EntranceArtefact`.
    Artefact,
    /// Carries `monitor` (a single mesh monitor's wire coordinates) and no
    /// `entrances` — the legacy mesh-participant `AccessPacket`.
    Packet,
}

/// Every way [`detect_kind`] refuses — by name, never a panic.
#[derive(Debug, Error)]
pub enum DeliveryError {
    #[error("delivery file: invalid JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("delivery file: top-level value is not a JSON object")]
    NotAnObject,

    #[error(
        "delivery file: ambiguous shape — carries BOTH 'entrances' (EntranceArtefact) and 'monitor' (AccessPacket)"
    )]
    AmbiguousShape,

    #[error(
        "delivery file: unrecognised shape — carries neither 'entrances' (EntranceArtefact) nor 'monitor' (AccessPacket)"
    )]
    UnknownShape,
}

/// Shape-sniff a delivery file's raw content: `EntranceArtefact` when it
/// carries `entrances` and not `monitor`, legacy `AccessPacket` when it
/// carries `monitor` and not `entrances`. Never inspects a filename — the
/// caller may hand this the byte-identical content of `access.json`,
/// `--access <path>`, or any other name; only the JSON shape decides.
///
/// This is a cheap, generic pre-parse (`serde_json::Value`) purely to pick
/// which typed parser to call next — it does not itself construct or
/// validate either `EntranceArtefact` or `AccessPacket`.
pub fn detect_kind(raw: &str) -> Result<DeliveryKind, DeliveryError> {
    let value: serde_json::Value = serde_json::from_str(raw)?;
    let obj = value.as_object().ok_or(DeliveryError::NotAnObject)?;
    match (obj.contains_key("entrances"), obj.contains_key("monitor")) {
        (true, false) => Ok(DeliveryKind::Artefact),
        (false, true) => Ok(DeliveryKind::Packet),
        (true, true) => Err(DeliveryError::AmbiguousShape),
        (false, false) => Err(DeliveryError::UnknownShape),
    }
}

/// Write a validated `EntranceArtefact` to sentinel's own managed path,
/// atomically (`crate::write_atomic` — tmp-then-rename, same discipline
/// every other sentinel-managed file uses). This IS the "local config the
/// connector needs": the connector's only input is the artefact file itself
/// (`mirage-subscriber-client --artefact <path>`), so materialisation here
/// means giving it a stable, sentinel-controlled copy rather than pointing
/// it at wherever the artefact happened to be delivered.
///
/// Callers must validate (`EntranceArtefact::validate`) before calling this
/// — matching the `access_packet` discipline, this function does not
/// validate on the caller's behalf.
pub fn materialise(state_dir: &Path, artefact: &EntranceArtefact) -> Result<PathBuf> {
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("create state_dir {}", state_dir.display()))?;
    let json = artefact.to_json().context("serialise entrance artefact")?;
    let path = materialised_path(state_dir);
    crate::write_atomic(&path, &json)?;
    Ok(path)
}

/// Build the connector's argv: `--artefact <path> --bind <addr>`. Pure and
/// testable in isolation from process spawning — see
/// `mirage-subscriber-client/src/bin/mirage-subscriber-client.rs`'s own
/// `Args` shape for why these are its only two flags.
pub fn build_connector_args(materialised_path: &Path, bind: &str) -> Vec<String> {
    vec![
        "--artefact".to_owned(),
        materialised_path.to_string_lossy().into_owned(),
        "--bind".to_owned(),
        bind.to_owned(),
    ]
}

/// Resolve the connector binary: explicit `sentinel.toml` config wins, else
/// `<exe_dir>/mirage-subscriber-client[.exe]` — the same "next to this one"
/// convention as [`crate::supervisor::resolve_tun_binary`] and
/// [`crate::supervisor::resolve_supervisor_binary`].
pub fn resolve_connector_binary(configured: Option<&PathBuf>) -> Option<PathBuf> {
    if let Some(p) = configured {
        return Some(p.clone());
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    #[cfg(windows)]
    let name = "mirage-subscriber-client.exe";
    #[cfg(not(windows))]
    let name = "mirage-subscriber-client";
    Some(dir.join(name))
}

/// Restart-policy window — the same shape as sentinel's own cli respawn
/// limiter (`supervisor::CLI_MAX_RESPAWNS` / `CLI_RESPAWN_WINDOW`): at most
/// this many respawns inside this window before sentinel gives up and exits
/// (letting the OS/launcher respawn sentinel itself from a clean state).
const RESPAWN_MAX: u32 = 5;
const RESPAWN_WINDOW: Duration = Duration::from_secs(60);

/// Spawn and supervise `mirage-subscriber-client` — the artefact branch's
/// entire remaining lifecycle. Never returns on success (mirrors
/// `crate::run_client_passthrough`'s own shape); exits the process once the
/// respawn budget is exhausted, same as the cli path.
///
/// Deliberately self-contained rather than reusing `ChildState`/
/// `status_server`/`telemetry`/`log_upload`: every one of those exists to
/// report to a `MonitorLink` this branch never builds (see the module doc,
/// "skips MonitorLink entirely") — wiring the connector into that
/// infrastructure would mean inventing a monitor-less no-op path through
/// each of them, not reusing anything real.
pub async fn run(binary: PathBuf, args: Vec<String>) -> Result<()> {
    let mirror = supervisor::unified_log_path();
    let mut respawn_timestamps: VecDeque<Instant> = VecDeque::new();

    loop {
        info!(binary = ?binary, args = ?args, "[sentinel][entrance] spawning subscriber client");
        let mut handle = supervisor::spawn_child_with_flags_and_mirror(
            &binary,
            &args,
            CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB,
            mirror.clone(),
        )
        .with_context(|| format!("failed to spawn {:?}", binary))?;
        info!(pid = ?handle.process.id(), "[sentinel][entrance] subscriber client running");

        let exit_status = loop {
            let exited = handle.process.try_wait().ok().flatten();
            if let Some(status) = exited {
                break status;
            }

            let ctrl_c_hit = tokio::select! {
                _ = tokio::signal::ctrl_c() => true,
                _ = tokio::time::sleep(Duration::from_millis(500)) => false,
            };
            if ctrl_c_hit {
                info!("[sentinel][entrance] Ctrl-C — shutting down subscriber client");
                let _ = handle.process.kill().await;
                let _ = tokio::time::timeout(Duration::from_secs(5), handle.process.wait()).await;
                std::process::exit(0);
            }
        };

        warn!(
            code = ?exit_status.code(),
            "[sentinel][entrance] subscriber client exited"
        );

        let now = Instant::now();
        while let Some(&front) = respawn_timestamps.front() {
            if now.duration_since(front) > RESPAWN_WINDOW {
                respawn_timestamps.pop_front();
            } else {
                break;
            }
        }
        if respawn_timestamps.len() >= RESPAWN_MAX as usize {
            error!(
                "[sentinel][entrance] subscriber client exceeded respawn limit ({} in {:?}) — exiting",
                RESPAWN_MAX, RESPAWN_WINDOW
            );
            std::process::exit(1);
        }
        respawn_timestamps.push_back(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── detection: the two shapes, by field presence ────────────────────

    fn sample_artefact_json() -> String {
        serde_json::json!({
            "v": 1,
            "private_key_hex": "11".repeat(32),
            "mesh_id": "MESH2",
            "entrances": [{
                "addr": "203.0.113.10:8443",
                "server_pubkey_hex": "22".repeat(32),
                "camouflage": {"type": "none"}
            }],
            "mode": "full_auto",
            "label": "pilot-1",
            "issued_at_ms": 1_726_000_000_000u64
        })
        .to_string()
    }

    fn sample_packet_json() -> String {
        serde_json::json!({
            "v": 1,
            "mesh_id": "MESH2",
            "client_id": "c0ffee00-0000-4000-8000-000000000001",
            "client_token": "22".repeat(32),
            "monitor": {
                "wire_addr": "103.231.75.144:19444",
                "wire_pubkey_hex": "11".repeat(32)
            },
            "operator_pub_hex": "33".repeat(32),
            "issued_at_ms": 1_726_000_000_000u64,
            "label": "pilot-1"
        })
        .to_string()
    }

    /// The exact literal `mirage-subscriber-client`'s own
    /// `artefact::tests::parses_a_panel_produced_artefact_field_for_field`
    /// parses — the panel-produced wire fixture shared across the two
    /// crates. Detection must recognise it as an `EntranceArtefact` by shape
    /// alone, with zero knowledge of where it came from.
    fn shared_panel_fixture_json() -> String {
        serde_json::json!({
            "v": 1,
            "private_key_hex": "11".repeat(32),
            "mesh_id": "22".repeat(32),
            "entrances": [{
                "addr": "203.0.113.9:443",
                "server_pubkey_hex": "33".repeat(32),
                "camouflage": {"type": "tls_real", "sni": "entrance.example.com", "alpn": ["h2"]}
            }],
            "mode": "full_auto",
            "label": "Ada",
            "issued_at_ms": 1_726_000_000_000u64
        })
        .to_string()
    }

    #[test]
    fn detects_an_entrance_artefact_by_shape() {
        assert_eq!(detect_kind(&sample_artefact_json()).unwrap(), DeliveryKind::Artefact);
    }

    #[test]
    fn detects_a_legacy_access_packet_by_shape() {
        assert_eq!(detect_kind(&sample_packet_json()).unwrap(), DeliveryKind::Packet);
    }

    #[test]
    fn detects_the_shared_panel_fixture_as_an_entrance_artefact() {
        let raw = shared_panel_fixture_json();
        assert_eq!(detect_kind(&raw).unwrap(), DeliveryKind::Artefact);

        // Detection is a real precondition for the SAME parser the connector
        // uses actually accepting it — prove both halves together, not just
        // the shape sniff in isolation.
        let artefact = EntranceArtefact::from_json(&raw).expect("shared fixture must parse");
        artefact.validate().expect("shared fixture must validate");
        assert_eq!(artefact.entrances.len(), 1);
        assert_eq!(artefact.entrances[0].addr, "203.0.113.9:443");
    }

    #[test]
    fn refuses_invalid_json_by_name() {
        assert!(matches!(detect_kind("not json"), Err(DeliveryError::Json(_))));
    }

    #[test]
    fn refuses_a_non_object_top_level_value() {
        assert!(matches!(detect_kind("[1,2,3]"), Err(DeliveryError::NotAnObject)));
    }

    #[test]
    fn refuses_an_unrecognised_shape() {
        let raw = serde_json::json!({"v": 1, "label": "nothing discriminating here"}).to_string();
        assert!(matches!(detect_kind(&raw), Err(DeliveryError::UnknownShape)));
    }

    #[test]
    fn refuses_an_ambiguous_shape() {
        let mut v: serde_json::Value = serde_json::from_str(&sample_artefact_json()).unwrap();
        v["monitor"] = serde_json::json!({"wire_addr": "x:1", "wire_pubkey_hex": "aa"});
        assert!(matches!(detect_kind(&v.to_string()), Err(DeliveryError::AmbiguousShape)));
    }

    // ── malformed artefact: a named refusal, naming the field ──────────

    #[test]
    fn a_malformed_artefact_is_refused_by_field_name() {
        let mut v: serde_json::Value = serde_json::from_str(&sample_artefact_json()).unwrap();
        v["private_key_hex"] = serde_json::Value::String("not-hex".to_owned());
        let raw = v.to_string();

        assert_eq!(detect_kind(&raw).unwrap(), DeliveryKind::Artefact);
        let artefact = EntranceArtefact::from_json(&raw).expect("still parses — validate refuses, not parse");
        let err = artefact.validate().expect_err("malformed private_key_hex must be refused");
        assert!(
            err.to_string().contains("private_key_hex"),
            "refusal must name the offending field, got: {err}"
        );
    }

    // ── materialisation ──────────────────────────────────────────────────

    #[test]
    fn materialise_writes_the_validated_artefact_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let raw = sample_artefact_json();
        let artefact = EntranceArtefact::from_json(&raw).unwrap();
        artefact.validate().unwrap();

        let path = materialise(dir.path(), &artefact).unwrap();
        assert_eq!(path, materialised_path(dir.path()));

        let written = std::fs::read_to_string(&path).unwrap();
        let round_tripped = EntranceArtefact::from_json(&written).unwrap();
        assert_eq!(
            serde_json::to_value(&artefact).unwrap(),
            serde_json::to_value(&round_tripped).unwrap()
        );
    }

    #[test]
    fn materialise_creates_state_dir_if_missing() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("nested").join("state");
        let artefact = EntranceArtefact::from_json(&sample_artefact_json()).unwrap();

        let path = materialise(&nested, &artefact).unwrap();
        assert!(path.exists());
    }

    // ── spawn branch: argv + binary resolution ──────────────────────────

    #[test]
    fn build_connector_args_is_artefact_then_bind() {
        let path = PathBuf::from("/tmp/state/entrance-artefact-managed.json");
        let args = build_connector_args(&path, DEFAULT_CONNECTOR_BIND);
        assert_eq!(
            args,
            vec![
                "--artefact".to_owned(),
                "/tmp/state/entrance-artefact-managed.json".to_owned(),
                "--bind".to_owned(),
                "127.0.0.1:11080".to_owned(),
            ]
        );
    }

    #[test]
    fn build_connector_args_carries_a_custom_bind_through_unmodified() {
        let path = PathBuf::from("artefact.json");
        let args = build_connector_args(&path, "127.0.0.1:12345");
        assert_eq!(args[3], "127.0.0.1:12345");
    }

    #[test]
    fn resolve_connector_binary_prefers_the_configured_path() {
        let configured = PathBuf::from("C:/custom/mirage-subscriber-client.exe");
        assert_eq!(resolve_connector_binary(Some(&configured)), Some(configured));
    }

    #[test]
    fn default_connector_bind_is_loopback() {
        let addr: std::net::SocketAddr = DEFAULT_CONNECTOR_BIND.parse().unwrap();
        assert!(addr.ip().is_loopback());
    }
}
