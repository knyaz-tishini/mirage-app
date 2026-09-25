//! mirage-l0-sentinel library — embeddable sentinel logic.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use arc_swap::ArcSwap;
use mirage_client_core::{
    chain_set::ChainSet,
    monitor_link::{MonitorLink, MonitorLinkOptions, MonitorTransport},
};
use mirage_core::mirage::mesh::PeerTable;
use mirage_runtime_guard::{MemoryGuard, MemoryGuardConfig};
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Mutex};
use tracing::{debug, error, info, warn};

pub mod access_packet;
pub mod chain_request;
pub mod crash_report;
pub mod debug_shell;
pub mod entrance_artefact;
pub mod log_upload;
pub mod monitor_wire;
pub mod onboarding;
pub mod pid_file;
pub mod sentinel_id;
pub mod status_server;
pub mod supervisor;
pub mod telemetry;
pub mod tun_config;

use crash_report::{report_crash, spawn_queue_drain};
use status_server::{MonitorInfo, TunSpawnCfg, reboot_channel};
use supervisor::{
    adopt_child, build_crash_report, cli_may_respawn, kill_tun, log_crash, record_cli_respawn,
    resolve_supervisor_binary, spawn_child, ChildState,
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW,
};

// ── Phase enum ────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case", tag = "phase")]
pub enum SentinelPhase {
    Starting,
    FetchingConfig { monitor_url: String },
    ConfigReady { version: u64, chains: usize, materialised_at_ms: u128 },
    ChildRunning { pid: Option<u32> },
    Error { message: String },
    WaitingOnboard,
    ConfigStale { since_ms: u64 },
}

// ── Public config ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SentinelConfig {
    #[serde(default)]
    pub monitor_api_key: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    pub client_config_path: PathBuf,
    pub client_template: PathBuf,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_respect_user_config")]
    pub respect_user_config: bool,
    #[serde(default = "default_install_panic_hook")]
    pub install_panic_hook: bool,
    #[serde(default = "default_exit_on_memory_fatal")]
    pub exit_on_memory_fatal: bool,
    #[serde(default)]
    pub tun_binary: Option<PathBuf>,
    /// Cross-platform default; used when no per-OS override matches.
    #[serde(default = "default_tun_args")]
    pub tun_args: Vec<String>,
    /// Windows-only override. Win mirage2tun is a service binary with subcommands
    /// (`debug-run`, `run`, ...) — `--config` style args are rejected.
    #[serde(default)]
    pub tun_args_windows: Option<Vec<String>>,
    /// macOS-only override. macOS mirage2tun takes `--config <path>`.
    #[serde(default)]
    pub tun_args_macos: Option<Vec<String>>,
    #[serde(default = "default_telemetry_interval")]
    pub telemetry_interval_secs: u64,
    /// How often (seconds) to upload cli/tun stderr tails to monitor.
    /// Set to 0 to disable. Default: 60.
    #[serde(default = "default_logs_upload_interval")]
    pub logs_upload_interval_secs: u64,
    /// Path to the chain-supervisor binary.  Defaults to
    /// `<exe_dir>/mirage-chain-supervisor.exe` on Windows (or without .exe on
    /// other platforms) when this field is `None`.
    #[serde(default)]
    pub supervisor_binary: Option<PathBuf>,
    /// Args passed to the supervisor.  The `{client_config_path}` token is
    /// substituted just like in `client_args`.
    ///
    /// Note: `mirage-chain-supervisor` reads its config as a positional
    /// argument (`argv[1]`), not via `--config`.  The default here is
    /// `["{client_config_path}"]` (bare path, no flag).
    #[serde(default = "default_supervisor_args")]
    pub supervisor_args: Vec<String>,
    /// Path where sentinel writes auto-generated tun.json (relay_addrs etc).
    /// If None, sentinel does not generate tun.json.
    #[serde(default)]
    pub tun_config_path: Option<PathBuf>,
    /// Template tun.json — copied into tun_config_path with relay_addrs filled in.
    /// If None, sentinel writes a minimal default tun.json stub.
    #[serde(default)]
    pub tun_template_path: Option<PathBuf>,
    /// Path to the `mirage-debug-shell` binary spawned on
    /// `WsServerFrame::DebugTunnelOpen`. When `None`, debug-shell features are
    /// silently disabled (incoming frames are logged + ignored).
    #[serde(default)]
    pub debug_shell_binary: Option<PathBuf>,
    /// Local opt-in for operator-triggered debug tunnels
    /// (`WsServerFrame::DebugTunnelOpen`/`DebugTunnelClose`).
    ///
    /// `false` by default. This is the ONLY switch that can enable the
    /// debug-shell path — a monitor pushing the frame, or any field carried
    /// in an `AccessPacket`, can never turn it on: `MonitorLink` refuses (and
    /// logs once per monitor) every `DebugTunnelOpen`/`DebugTunnelClose`
    /// frame before it ever reaches this process's frame handling when this
    /// is `false`, regardless of whether `debug_shell_binary` is configured.
    /// A mode-3/customer issuance of this config never sets this field —
    /// flip it by hand, locally, only for an operator-fleet box that is
    /// meant to accept a reverse-SSH debug tunnel.
    #[serde(default)]
    pub allow_operator_debug_shell: bool,
    /// Directory where sentinel persists transient state — `monitor_wire.json`
    /// (Wire transport coordinates) and `debug-tunnel.json` (DebugTunnelOpen
    /// payload). Defaults to the directory of `client_config_path`.
    #[serde(default)]
    pub state_dir: Option<PathBuf>,
    /// K2: explicit path to an access packet (see [`access_packet`]) to
    /// consume on this boot. Set from the binary's `--access <path>` flag;
    /// `None` here (the default) falls back to the unconsumed
    /// `${state_dir}/access.json`, if any. Both entry points go through the
    /// identical `access_packet::AccessPacket::apply`.
    #[serde(default)]
    pub access_packet_path: Option<PathBuf>,

    // ── T1.1: the commercial artefact branch ──────────────────────────
    //
    // Fires instead of the whole MonitorLink/cli2 bootstrap above when the
    // file at `access_packet_path` (or its default) is shaped like an
    // `EntranceArtefact` rather than a legacy `AccessPacket` — see
    // `entrance_artefact::detect_kind`, called from `run()` before any
    // monitor-related state is touched.
    /// Path to the `mirage-subscriber-client` binary. Defaults to
    /// `<exe_dir>/mirage-subscriber-client[.exe]` when `None` (the same
    /// "next to this one" convention as `tun_binary`/`supervisor_binary`).
    #[serde(default)]
    pub subscriber_client_binary: Option<PathBuf>,
    /// Local SOCKS5 bind sentinel spawns the connector with. Must be
    /// loopback — the connector itself refuses any other bind by name, this
    /// default simply matches it. See
    /// `entrance_artefact::DEFAULT_CONNECTOR_BIND`.
    #[serde(default = "default_subscriber_client_bind")]
    pub subscriber_client_bind: String,

    // ── Stage 3 identity (sent on register; nothing axis-related here) ───
    //
    // K1 (2026-09-17): the sentinel now ALSO asks the monitor to build a
    // chain for this client (`chain_ask_*` knobs below, `chain_request::
    // spawn_chain_asker`) instead of only waiting for the monitor's own push
    // schedule. Asking does not replace the push — the materialised chain
    // still arrives the same way, through `WsServerFrame::Config`; asking
    // only gives the monitor something to build and push. See
    // `docs/mirage/plans/client-track-checklist-2026-09-17.md` K1.
    /// Mesh id sent on register + carried in handshakes. Defaults to
    /// `"v3-staging"` to match the current staging mesh.
    #[serde(default = "default_mesh_id")]
    pub mesh_id: String,
    /// IP:port of the monitor's Mirage wire listener, e.g. `"103.231.75.144:19444"`.
    ///
    /// When both `monitor_wire_addr` and `monitor_wire_pubkey_hex` are set, the
    /// sentinel builds a [`MonitorTransport::Wire`] entry and passes it to
    /// `MonitorLink` as the Wire transport. Wire is the primary transport;
    /// `monitor_api_key` is not needed — the Mirage wire handshake provides
    /// mutual authentication without a bearer token.
    #[serde(default)]
    pub monitor_wire_addr: Option<String>,
    /// 64-character hex of the monitor's static X25519 public key.
    ///
    /// Obtain via `GET <monitor_url>/relay/me` → field `pubkey_hex`, or from
    /// the operator-box inventory.  Required when `monitor_wire_addr` is set.
    #[serde(default)]
    pub monitor_wire_pubkey_hex: Option<String>,
    /// Human label that surfaces in /relay/clients responses
    /// (`sentry-XXX`). Hardcoded per build profile or set in toml.
    #[serde(default)]
    pub shortname: Option<String>,
    /// `sentry-client | sentry-server-lm | sentry-server-node`.
    /// Default `sentry-client` matches a desktop client.
    #[serde(default = "default_role")]
    pub role: String,
    /// Operator Ed25519 verifying key as a 64-character lowercase hex string.
    ///
    /// When set, `MonitorLink` will verify every incoming `CredentialParams`
    /// frame against this key before applying it.  Params that fail verification
    /// or whose `generation` is not strictly greater than the last-accepted
    /// generation are silently discarded (a `warn!` is emitted).
    ///
    /// Obtain the value from the operator-box: it is the public half of the
    /// Ed25519 keypair used when the operator calls `CredentialParams::sign`.
    /// This value is static operator identity — it must NOT be fetched from
    /// the monitor (the monitor is the entity being verified).
    ///
    /// When absent (default), params are applied without operator-signature
    /// verification; a one-time `warn!` is emitted as a reminder.
    #[serde(default)]
    pub operator_pub_hex: Option<String>,
    /// Enrolment token used for blind-RSA batch credential requests.
    ///
    /// When set, `MonitorLink` will request a batch of blind tokens from the
    /// monitor whenever the local token pool falls below 16 tokens and the
    /// monitor has pushed `CredentialParams` with a `BlindRsaBatch` authority.
    /// When absent (default), no batch requests are made and existing behavior
    /// is preserved (pool stays empty, spending returns an error).
    #[serde(default)]
    pub enrol_token: Option<String>,
    /// Ring-membership (scheme 0x04) member secret — 64-char hex of the member's
    /// ristretto scalar, from operator-box `POST /credential/ring-enrol`. Injected
    /// into the materialised ClientConfig so the spawned cli can derive a 0x04
    /// credential at dial time. Mirrors `enrol_token` for the blind path.
    #[serde(default)]
    pub ring_member_sk_hex: Option<String>,
    /// Ring-membership Merkle inclusion path (leaf→root), from `/credential/ring-enrol`.
    /// Injected into the materialised ClientConfig alongside `ring_member_sk_hex`.
    #[serde(default)]
    pub ring_path: Option<Vec<mirage_core::config::RingPathStep>>,
    /// Epoch id whose Merkle tree `ring_path` corresponds to. Injected into the
    /// materialised ClientConfig so the node can find the matching published root.
    #[serde(default)]
    pub ring_epoch_id: Option<u32>,
    /// When `false`, sentinel performs bootstrap + identity + config materialisation +
    /// MonitorLink + TUN normally, but **does not spawn the cli** child process.
    /// Use this when an external supervisor (e.g. `mirage-l0-orchestrator`) owns
    /// the cli lifecycle directly.
    ///
    /// Defaults to `true` (existing behaviour — sentinel spawns cli on its own).
    #[serde(default = "default_spawn_cli")]
    pub spawn_cli: bool,

    // ── K1: chain ask ──────────────────────────────────────────────────
    //
    // The sentinel asks the monitor to BUILD a chain for this client
    // instead of only waiting for the monitor's own push schedule to hand
    // one over (see the identity block above). Asking does not replace the
    // push — see `chain_request::spawn_chain_asker`.
    /// Whether the sentinel asks at all. Default `true` — this IS the point
    /// of K1; set `false` to restore pure wait-for-push behaviour.
    #[serde(default = "default_chain_ask_enabled")]
    pub chain_ask_enabled: bool,
    /// Hop count sent in the chain-ask `AxisSpec`. Default 2 — matches the
    /// monitor's own default
    /// (`mirage2monitor::auto_chain::default_auto_axis_spec`).
    #[serde(default = "default_chain_ask_hops")]
    pub chain_ask_hops: u8,
    /// How long (seconds) to wait for an asked-for chain to appear in the
    /// pushed chain set before logging a named WARN and scheduling a
    /// re-ask. Default 90.
    #[serde(default = "default_chain_ask_wait_secs")]
    pub chain_ask_wait_secs: u64,
    /// Floor (seconds) of the re-ask backoff — never re-asks more often
    /// than this. Default 30.
    #[serde(default = "default_chain_ask_retry_floor_secs")]
    pub chain_ask_retry_floor_secs: u64,
    /// Cap (seconds) of the re-ask backoff. Default 300 (5 minutes).
    #[serde(default = "default_chain_ask_retry_cap_secs")]
    pub chain_ask_retry_cap_secs: u64,
    /// K4: the egress country the box asks the monitor for, e.g. `"nl"`.
    /// Sent as `anchor.exit_region` on every chain ask. `None` (default) asks
    /// for no region at all — full-auto, the monitor's own picker decides.
    ///
    /// Box-local for now: the customer names a country to the box (out of
    /// band, today a hand-set config value; K2's access packet is the
    /// natural carrier once the customer-to-box path exists), and the box
    /// carries that name into every ask. This field is that carrier, so the
    /// later step is a new writer of the same field, not a new field.
    #[serde(default)]
    pub chain_ask_exit_region: Option<String>,
}

pub(crate) fn default_poll_interval() -> u64 { 30 }
pub(crate) fn default_respect_user_config() -> bool { true }
pub(crate) fn default_install_panic_hook() -> bool { true }
pub(crate) fn default_exit_on_memory_fatal() -> bool { true }
pub(crate) fn default_tun_args() -> Vec<String> {
    #[cfg(target_os = "windows")]
    { vec!["debug-run".to_owned()] }
    #[cfg(target_os = "macos")]
    { vec!["--config".to_owned(), "tun.json".to_owned()] }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    { vec!["debug-run".to_owned()] }
}

impl SentinelConfig {
    /// Pick the right tun_args for the current OS:
    /// `tun_args_windows` on Windows / `tun_args_macos` on macOS if set,
    /// otherwise falls back to `tun_args` (which itself defaults per-OS).
    pub fn effective_tun_args(&self) -> Vec<String> {
        #[cfg(target_os = "windows")]
        { if let Some(v) = &self.tun_args_windows { return v.clone(); } }
        #[cfg(target_os = "macos")]
        { if let Some(v) = &self.tun_args_macos { return v.clone(); } }
        self.tun_args.clone()
    }
}
pub(crate) fn default_telemetry_interval() -> u64 { 0 }
pub(crate) fn default_logs_upload_interval() -> u64 { 0 }
pub(crate) fn default_supervisor_args() -> Vec<String> {
    vec!["{client_config_path}".to_owned()]
}
pub(crate) fn default_subscriber_client_bind() -> String {
    entrance_artefact::DEFAULT_CONNECTOR_BIND.to_owned()
}
pub(crate) fn default_mesh_id() -> String { "v3-staging".into() }
pub(crate) fn default_role() -> String { "sentry-client".into() }
pub(crate) fn default_spawn_cli() -> bool { true }
pub(crate) fn default_chain_ask_enabled() -> bool { true }
pub(crate) fn default_chain_ask_hops() -> u8 { 2 }
pub(crate) fn default_chain_ask_wait_secs() -> u64 { 90 }
pub(crate) fn default_chain_ask_retry_floor_secs() -> u64 { 30 }
pub(crate) fn default_chain_ask_retry_cap_secs() -> u64 { 300 }

/// Extract the path following a `--config <path>` pair in tun args.
/// macOS tun uses `--config <tun.json>`; when `tun_config_path` is unset we
/// derive the write target from these args so a single `tun_args_macos`
/// setting is enough to enable tun.json generation.
pub(crate) fn extract_config_arg(args: &[String]) -> Option<std::path::PathBuf> {
    args.iter()
        .position(|a| a == "--config")
        .and_then(|i| args.get(i + 1))
        .map(std::path::PathBuf::from)
}

// ── Managed marker ────────────────────────────────────────────────────────────

pub(crate) const MANAGED_BY_VALUE: &str = "mirage-l0-sentinel";

pub(crate) fn is_managed(path: &PathBuf) -> bool {
    let Ok(raw) = std::fs::read_to_string(path) else { return false; };
    if raw.trim_start().starts_with("// mirage-l0-sentinel: managed") {
        return true;
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
        if v.get("_managed_by").and_then(|v| v.as_str()) == Some(MANAGED_BY_VALUE) {
            return true;
        }
    }
    false
}

// ── Config materialisation ────────────────────────────────────────────────────

pub(crate) fn materialise(
    template_raw: &str,
    chains: Vec<mirage_core::config::ChainConfig>,
    operator_pub_hex: Option<&str>,
    monitor_wire_addr: Option<&str>,
    monitor_wire_pubkey_hex: Option<&str>,
    enrol_token: Option<&str>,
    ring_member_sk_hex: Option<&str>,
    ring_path: Option<&[mirage_core::config::RingPathStep]>,
    ring_epoch_id: Option<u32>,
    credential_params: Option<&mirage_core::crypto::credential::CredentialParams>,
    capability_tokens_path: Option<&str>,
    tun_api_bind: Option<&str>,
    connect_generation: u64,
) -> Result<String> {
    let mut obj: serde_json::Value =
        serde_json::from_str(template_raw).context("failed to parse client-template.json")?;
    let map = obj.as_object_mut().context("client-template.json must be a JSON object")?;
    map.remove("chains");
    // cli2's from_client_config hard-errors on any chain whose socks5_bind is
    // None — the monitor pushes chains without socks5_bind set, so we inject a
    // deterministic default here. Port scheme: 127.0.0.1:{19480 + index}.
    let mut chains = chains;
    for (i, chain) in chains.iter_mut().enumerate() {
        if chain.socks5_bind.is_none() {
            chain.socks5_bind = Some(format!("127.0.0.1:{}", 19480u16 + i as u16));
        }
    }
    let chains_val = serde_json::to_value(&chains).context("failed to serialise chains")?;
    map.insert("chains".to_owned(), chains_val);
    map.insert("_managed_by".to_owned(), serde_json::Value::String(MANAGED_BY_VALUE.to_owned()));
    // Monotonic counter that changes on every connect/disconnect.  The cli2
    // supervisor hashes client-config.json content to detect changes; embedding
    // this counter ensures a connect/disconnect always produces a different
    // fingerprint and triggers a drain+respawn of the worker, which forces fresh
    // carrier establishment under the new routing (tun up or direct).
    // The field is present in EVERY materialise call with the SAME value unless
    // connect/disconnect bumps it — so routine monitor-push rewrites do NOT
    // change the fingerprint and cause spurious respawns.
    map.insert(
        "_connect_generation".to_owned(),
        serde_json::Value::Number(serde_json::Number::from(connect_generation)),
    );
    // Inject blind-token credentials so the spawned cli's own MonitorLink can
    // request a batch and spend one token per dial under a require=true fleet.
    // `enrol_token` is one-time and operator-delivered via sentinel.toml (it
    // cannot live statically in the template); `operator_pub_hex` verifies the
    // monitor-pushed CredentialParams. The wire transport (monitor_wire_addr /
    // monitor_wire_pubkey_hex) and monitor_url stay in client-template.json —
    // they are static per deployment and survive materialisation untouched.
    if let Some(op) = operator_pub_hex {
        map.insert("operator_pub_hex".to_owned(), serde_json::Value::String(op.to_owned()));
    }
    if let Some(addr) = monitor_wire_addr {
        map.insert("monitor_wire_addr".to_owned(), serde_json::Value::String(addr.to_owned()));
    }
    if let Some(pk) = monitor_wire_pubkey_hex {
        map.insert("monitor_wire_pubkey_hex".to_owned(), serde_json::Value::String(pk.to_owned()));
    }
    if let Some(tok) = enrol_token {
        map.insert("enrol_token".to_owned(), serde_json::Value::String(tok.to_owned()));
    }
    // Inject ring-membership credential fields so the spawned cli can derive a
    // 0x04 token at dial time without contacting the issuer.  All three fields
    // must be present; absent → no ring credential injected.
    if let Some(sk) = ring_member_sk_hex {
        map.insert("ring_member_sk_hex".to_owned(), serde_json::Value::String(sk.to_owned()));
    }
    if let Some(path) = ring_path {
        map.insert(
            "ring_path".to_owned(),
            serde_json::to_value(path).context("failed to serialise ring_path")?,
        );
    }
    if let Some(eid) = ring_epoch_id {
        map.insert(
            "ring_epoch_id".to_owned(),
            serde_json::Value::Number(serde_json::Number::from(eid)),
        );
    }
    if let Some(cp) = credential_params {
        match serde_json::to_value(cp) {
            Ok(v) => { map.insert("credential_params".to_owned(), v); }
            Err(e) => warn!("materialise: failed to serialise credential_params: {e}"),
        }
    }
    // K2: point the worker's pre-minted-0x02 source at the sentinel's own
    // state_dir file (see `access_packet::capability_tokens_path`) unless the
    // template already sets this explicitly (manual/legacy deployments keep
    // their own path). The target file need not exist yet — an absent file
    // is simply an empty source — so this is safe to set unconditionally
    // whether or not any access packet has ever delivered tokens.
    if !map.contains_key("capability_tokens_path") {
        if let Some(p) = capability_tokens_path {
            map.insert("capability_tokens_path".to_owned(), serde_json::Value::String(p.to_owned()));
        }
    }
    // Derive tun_api_url from the TUN's api_bind so cli2's split-policy pusher
    // can reach the TUN control API without operator hand-setting it in the
    // client-template. Template's explicit tun_api_url still takes precedence.
    if !map.contains_key("tun_api_url") {
        if let Some(bind) = tun_api_bind {
            map.insert(
                "tun_api_url".to_owned(),
                serde_json::Value::String(format!("http://{bind}")),
            );
        }
    }
    serde_json::to_string_pretty(&obj).context("failed to serialise materialised config")
}

pub(crate) fn write_atomic(path: &PathBuf, content: &str) -> Result<()> {
    // Unique tmp name per call: the sentinel fires several config-rewrite tasks
    // (Config snapshot / ChainAvailable / CredentialParams) that can run nearly
    // concurrently. A fixed `<name>.tmp` made them race — two writers truncating
    // the same tmp, one rename hitting a half-written/already-renamed file →
    // "rename failed" and a stale config left on disk. A per-call suffix removes
    // the collision so every rewrite lands atomically.
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp.{}.{}", std::process::id(), seq));
    std::fs::write(&tmp, content).with_context(|| format!("write tmp file {:?}", tmp))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename {:?} -> {:?}", tmp, path))?;
    Ok(())
}

/// Prefer the LIVE hot-swapped ring credential (from MonitorLink's cell) over
/// the static seed from `sentinel.toml`. EVERY config-materialisation path must
/// route ring fields through this — `MaterialiseCtx::rewrite` (the credential /
/// ChainAvailable fast-paths) AND the main config-snapshot handler. If the
/// snapshot handler materialises from the static seed instead, each (frequent)
/// snapshot push clobbers the fresh epoch the credential path just wrote,
/// pinning the data-plane worker to the stale seed epoch — which, once it ages
/// out or its exit SpentSet is spent, the exit rejects forever (CREDENTIAL_REJECTED).
pub(crate) fn resolve_ring_fields(
    ring_cell: &mirage_client_core::token_pool::RingCredentialCell,
    static_sk_hex: Option<String>,
    static_path: Option<Vec<mirage_core::config::RingPathStep>>,
    static_epoch: Option<u32>,
) -> (
    Option<String>,
    Option<Vec<mirage_core::config::RingPathStep>>,
    Option<u32>,
) {
    let live = ring_cell.load();
    match live.as_ref().as_ref() {
        Some(rc) => (
            Some(hex::encode(rc.member_sk.to_bytes())),
            Some(
                rc.path
                    .iter()
                    .map(|(sib, is_right)| mirage_core::config::RingPathStep {
                        sibling_hex: hex::encode(sib),
                        is_right: *is_right,
                    })
                    .collect(),
            ),
            Some(rc.epoch_id),
        ),
        None => (static_sk_hex, static_path, static_epoch),
    }
}

/// Captures the inputs required to re-materialise and atomically write the
/// supervisor config on demand (e.g. from `ChainAvailable` fast-path nudge).
/// All fields are immutable after construction; shared via `Arc`.
pub struct MaterialiseCtx {
    pub template_raw: String,
    pub client_config_path: PathBuf,
    pub operator_pub_hex: Option<String>,
    pub monitor_wire_addr: Option<String>,
    pub monitor_wire_pubkey_hex: Option<String>,
    pub enrol_token: Option<String>,
    /// Ring credential seeded from `sentinel.toml` (the first/registration
    /// artifact). Used as the FALLBACK when the live cell below is still empty
    /// (cold boot before the first re-enrol completes).
    pub ring_member_sk_hex: Option<String>,
    pub ring_path: Option<Vec<mirage_core::config::RingPathStep>>,
    pub ring_epoch_id: Option<u32>,
    /// Live arc from MonitorLink; snapshot at each rewrite() call so the config
    /// always carries the latest accepted CredentialParams without requiring a
    /// struct mutation.
    pub credential_params_arc: Arc<ArcSwap<Option<mirage_core::crypto::credential::CredentialParams>>>,
    /// K2: worker-facing path for pre-minted 0x02 tokens — always
    /// `access_packet::capability_tokens_path(state_dir)`; see `materialise`'s
    /// `capability_tokens_path` parameter for why it is safe to set
    /// unconditionally.
    pub capability_tokens_path: String,
    /// Live ring-membership credential cell from MonitorLink. When the sentinel
    /// re-enrols on epoch rotation, MonitorLink hot-swaps the fresh credential
    /// here; rewrite() prefers it over the static `ring_*` fields above so the
    /// data-plane worker (a pure config consumer) picks up the new credential on
    /// its next config reload — without this, the worker would keep dialing a
    /// stale epoch after its seeded one ages out of the declared set and the exit
    /// would reject it as a "no-credential dial".
    pub ring_cell: mirage_client_core::token_pool::RingCredentialCell,
    /// Path to the tun config template (tun-template.json). Used to read the
    /// effective `api_bind` so that `tun_api_url` can be derived and injected
    /// into the materialised client-config without operator hand-configuration.
    pub tun_template_path: Option<PathBuf>,
    /// Monotonic counter shared with the status server.  Bumped on connect and
    /// disconnect; embedded in every materialised config as `_connect_generation`
    /// so that a bump always changes the file's content fingerprint and triggers
    /// a cli2 supervisor drain+respawn.  Routine monitor-push rewrites read the
    /// current value without incrementing it — no spurious respawns.
    pub connect_generation: Arc<AtomicU64>,
}

impl MaterialiseCtx {
    /// Materialise using the supplied chain list and atomically write
    /// `client_config_path`.  Reads the current `connect_generation` without
    /// incrementing it — routine monitor-push rewrites preserve the same
    /// generation value and therefore do not change the content fingerprint,
    /// preventing spurious cli2 worker respawns.
    pub(crate) fn rewrite(
        &self,
        chains: Vec<mirage_core::config::ChainConfig>,
    ) -> Result<()> {
        let gen = self.connect_generation.load(Ordering::Relaxed);
        self.rewrite_with_generation(chains, gen)
    }

    /// Materialise with an explicitly supplied `generation` value.  Used by
    /// the connect/disconnect handlers to inject a freshly incremented counter
    /// without racing against a concurrent `rewrite()` that reads the atomic.
    pub(crate) fn rewrite_with_generation(
        &self,
        chains: Vec<mirage_core::config::ChainConfig>,
        generation: u64,
    ) -> Result<()> {
        let cp_guard = self.credential_params_arc.load();
        let cp_ref = cp_guard.as_ref().as_ref();

        // Prefer the LIVE ring credential (refreshed by MonitorLink's ring
        // re-enrol on epoch rotation) over the static one seeded from
        // sentinel.toml. Shared with the main config-snapshot handler via
        // `resolve_ring_fields` so neither path can clobber the other.
        let (sk_owned, path_owned, ring_epoch) = resolve_ring_fields(
            &self.ring_cell,
            self.ring_member_sk_hex.clone(),
            self.ring_path.clone(),
            self.ring_epoch_id,
        );

        let tun_api_bind =
            crate::tun_config::effective_api_bind(self.tun_template_path.as_deref());
        let json = materialise(
            &self.template_raw,
            chains,
            self.operator_pub_hex.as_deref(),
            self.monitor_wire_addr.as_deref(),
            self.monitor_wire_pubkey_hex.as_deref(),
            self.enrol_token.as_deref(),
            sk_owned.as_deref(),
            path_owned.as_deref(),
            ring_epoch,
            cp_ref,
            Some(self.capability_tokens_path.as_str()),
            Some(tun_api_bind.as_str()),
            generation,
        )?;
        write_atomic(&self.client_config_path, &json)
    }
}

pub(crate) fn build_child_args(template_args: &[String], config_path: &PathBuf) -> Vec<String> {
    let path_str = config_path.to_string_lossy();
    template_args.iter().map(|a| a.replace("{client_config_path}", &path_str)).collect()
}

/// Resolve the binary path and expanded args for the cli2 supervisor.
pub(crate) fn resolve_cli_binary_and_args(cfg: &SentinelConfig) -> (PathBuf, Vec<String>) {
    let binary = resolve_supervisor_binary(cfg.supervisor_binary.as_ref())
        .unwrap_or_else(|| {
            warn!("[sentinel] supervisor binary not found via auto-detect; using fallback name");
            let exe = std::env::consts::EXE_SUFFIX;
            PathBuf::from(format!("mirage-chain-supervisor{exe}"))
        });
    let args = build_child_args(&cfg.supervisor_args, &cfg.client_config_path);
    (binary, args)
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run(cfg: SentinelConfig) -> Result<()> {
    // Derive the effective tun config write path: explicit `tun_config_path`
    // wins; otherwise auto-extract from `--config <path>` in the macOS tun
    // args. This lets a single `tun_args_macos = ["--config", ".../tun.json"]`
    // setting enable tun.json generation without also setting `tun_config_path`.
    let effective_tun_config_path: Option<std::path::PathBuf> = cfg
        .tun_config_path
        .clone()
        .or_else(|| extract_config_arg(&cfg.effective_tun_args()));

    let state_dir = debug_shell::resolve_state_dir(
        cfg.state_dir.as_deref(),
        &cfg.client_config_path,
    );

    // K2: worker-facing path for pre-minted 0x02 tokens — always this file
    // under state_dir (see `access_packet::capability_tokens_path`), whether
    // or not an access packet has ever delivered any (an absent file is
    // simply an empty source).
    let capability_tokens_path_str: String =
        access_packet::capability_tokens_path(&state_dir).to_string_lossy().into_owned();

    // K2 / T1.1: consume whatever is at `access_path` — the config/QR
    // delivery unit — before anything below reads persisted state or builds
    // a MonitorLink. `cfg.access_packet_path` (set from the binary's
    // `--access <path>` flag) wins when present; otherwise the default
    // unconsumed `${state_dir}/access.json` is tried.
    //
    // T1.1: the file is no longer assumed to be a legacy `AccessPacket` by
    // virtue of living at this path — `entrance_artefact::detect_kind`
    // sniffs its JSON SHAPE (which discriminating field it carries, never
    // its filename) to tell a commercial `EntranceArtefact` from the
    // mesh-participant `AccessPacket`. An artefact is parsed and validated
    // with the SAME parser `mirage-subscriber-client` itself uses
    // (`mirage_subscriber_client::EntranceArtefact`, shared as a dependency,
    // not copied); a malformed one is refused by name (which field) and
    // sentinel falls through to the ordinary boot below exactly as it does
    // today for a malformed `AccessPacket`. A validated artefact instead
    // short-circuits `run()` entirely — see `entrance_artefact`'s module doc
    // for why the rest of this function (wire coords, register, MonitorLink)
    // must never run on this branch.
    let access_path = cfg
        .access_packet_path
        .clone()
        .unwrap_or_else(|| access_packet::default_path(&state_dir));
    let mut ready_artefact: Option<PathBuf> = None;
    if access_path.exists() {
        match std::fs::read_to_string(&access_path) {
            Ok(raw) => match entrance_artefact::detect_kind(&raw) {
                Ok(entrance_artefact::DeliveryKind::Artefact) => {
                    match mirage_subscriber_client::EntranceArtefact::from_json(&raw) {
                        Ok(artefact) => match artefact.validate() {
                            Ok(()) => match entrance_artefact::materialise(&state_dir, &artefact) {
                                Ok(materialised) => {
                                    info!(
                                        "[sentinel] entrance artefact at {:?} validated — materialised {:?}",
                                        access_path, materialised
                                    );
                                    ready_artefact = Some(materialised);
                                }
                                Err(e) => error!(
                                    "[sentinel] entrance artefact at {:?} materialise failed: {:#}",
                                    access_path, e
                                ),
                            },
                            Err(e) => error!(
                                "[sentinel] entrance artefact at {:?} refused: {}",
                                access_path, e
                            ),
                        },
                        Err(e) => error!(
                            "[sentinel] entrance artefact at {:?} did not parse: {}",
                            access_path, e
                        ),
                    }
                }
                Ok(entrance_artefact::DeliveryKind::Packet) => {
                    match access_packet::AccessPacket::from_json(&raw) {
                        Ok(packet) => {
                            if let Err(e) = packet.apply(&state_dir) {
                                error!("[sentinel] access packet at {:?} refused: {}", access_path, e);
                            }
                        }
                        Err(e) => error!("[sentinel] access packet at {:?} invalid: {}", access_path, e),
                    }
                }
                Err(e) => error!("[sentinel] delivery file at {:?} refused: {}", access_path, e),
            },
            Err(e) => warn!("[sentinel] access packet at {:?} unreadable: {}", access_path, e),
        }
    }

    // T1.1 spawn branch: a validated, materialised artefact takes over here
    // — no wire coords, no register, no MonitorLink. Spawns
    // `mirage-subscriber-client` via `supervisor::spawn_child` (the exact
    // primitive the cli2 spawn below also uses) with a restart policy.
    if let Some(materialised) = ready_artefact {
        let binary = entrance_artefact::resolve_connector_binary(cfg.subscriber_client_binary.as_ref())
            .unwrap_or_else(|| {
                warn!("[sentinel] subscriber-client binary not found via auto-detect; using fallback name");
                let exe = std::env::consts::EXE_SUFFIX;
                PathBuf::from(format!("mirage-subscriber-client{exe}"))
            });
        let args = entrance_artefact::build_connector_args(&materialised, &cfg.subscriber_client_bind);
        info!(
            "[sentinel] EntranceArtefact detected — skipping MonitorLink, spawning the commercial connector"
        );
        return entrance_artefact::run(binary, args).await;
    }

    // Wire transport coordinates: explicit sentinel.toml fields win; otherwise
    // fall back to the last-run persisted coords (auto-seeded from peer_table
    // by `spawn_frame_watcher`). This is the ONLY bootstrap path — the legacy
    // HTTPS register/monitor_urls list was retired with the HTTPS control
    // plane (see `chain_request::register_with_failover`).
    let wire_coords: Option<monitor_wire::WireCoords> =
        if cfg.monitor_wire_addr.is_some() && cfg.monitor_wire_pubkey_hex.is_some() {
            Some(monitor_wire::WireCoords {
                monitor_addr: cfg.monitor_wire_addr.clone().unwrap(),
                monitor_pubkey_hex: cfg.monitor_wire_pubkey_hex.clone().unwrap(),
                mesh_id: cfg.mesh_id.clone(),
            })
        } else {
            monitor_wire::load(&state_dir)
        };

    let children = Arc::new(Mutex::new(ChildState::new()));
    let monitor_info = Arc::new(std::sync::Mutex::new(MonitorInfo {
        monitor_url: wire_coords
            .as_ref()
            .map(|wc| format!("wire://{}", wc.monitor_addr))
            .unwrap_or_default(),
        monitor_reachable: false,
    }));

    // Shared monotonic counter for connect/disconnect re-establish.  Bumped by
    // the status-server /connect and /disconnect handlers; read (not bumped) by
    // every routine materialise() call.  Embedded in client-config.json as
    // `_connect_generation` so a bump always changes the file fingerprint and
    // triggers a cli2 supervisor drain+respawn.
    let connect_generation = Arc::new(AtomicU64::new(0));

    let (monitor_reachable_tx, monitor_reachable_rx) = watch::channel(false);
    let monitor_reachable_tx = Arc::new(monitor_reachable_tx);
    let (tun_alive_tx, tun_alive_rx) = watch::channel(false);
    let tun_alive_tx = Arc::new(tun_alive_tx);

    // Resolve cli binary + args up front so /connect (status server route)
    // can spawn them on demand.
    let (cli_binary_for_status, cli_args_for_status) = resolve_cli_binary_and_args(&cfg);

    let (phase_tx, phase_rx) = status_server::phase_channel();
    let (reboot_tx, mut reboot_rx) = reboot_channel();
    // Proxy watch for the MonitorConfig chain snapshot.  MonitorLink is not
    // built yet at this point, so we create a standalone proxy channel here and
    // forward link updates into it once the link exists (see forwarder task below
    // after MonitorLink::new_with_operator_pub).  The status server reads from
    // this proxy receiver to get the current chain list on /connect and /disconnect.
    let (config_proxy_tx, config_proxy_rx) =
        tokio::sync::watch::channel::<Option<mirage_client_core::monitor_link::MonitorConfig>>(None);
    let config_proxy_tx = Arc::new(config_proxy_tx);
    // Shared latch for the materialise context — populated after MonitorLink is
    // built.  The status server spawns before the link, so this cell starts
    // empty and is filled in below.
    let mat_ctx_latch: Arc<std::sync::Mutex<Option<Arc<MaterialiseCtx>>>> =
        Arc::new(std::sync::Mutex::new(None));

    let publisher = status_server::spawn(
        phase_rx,
        phase_tx,
        Arc::clone(&children),
        TunSpawnCfg {
            tun_binary: cfg.tun_binary.clone(),
            tun_args: cfg.effective_tun_args(),
            cli_binary: cli_binary_for_status,
            cli_args: cli_args_for_status,
        },
        Arc::clone(&monitor_info),
        Arc::clone(&reboot_tx),
        Arc::clone(&connect_generation),
        config_proxy_rx,
        Arc::clone(&mat_ctx_latch),
    );
    publisher.set(SentinelPhase::Starting);

    let want_hook = cfg.install_panic_hook
        || std::env::var("MIRAGE_SENTINEL_PANIC_EXIT").as_deref() == Ok("1");
    if want_hook {
        std::panic::set_hook(Box::new(|info| {
            let location = info
                .location()
                .map(|l| format!("{}:{}", l.file(), l.line()))
                .unwrap_or_else(|| "<unknown>".to_string());
            let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = info.payload().downcast_ref::<String>() {
                s.clone()
            } else {
                "<non-string panic payload>".to_string()
            };
            tracing::error!(location=%location, message=%msg, "panic — process exiting");
            std::thread::sleep(std::time::Duration::from_millis(100));
            std::process::exit(1);
        }));
    }

    // WaitingOnboard: block if wire coords are missing. Unlike pre-Stage-3
    // builds we DO NOT require a static `client_id` — the stable sentinel_id
    // flow generates one on first boot under ${state_dir}.
    //
    // K2 (2026-09-17): this used to ALSO require a non-empty
    // `monitor_api_key`, a field nothing on the wire reads —
    // `monitor_link.rs` documents it as "ignored; kept in the signature for
    // backward compat" and the Wire transport sends no `Authorization`
    // header at all (the Mirage V9 wire handshake is the authentication). A
    // gate CAN require a field nobody reads — that combination held a box
    // with valid wire_coords (including one freshly seeded by an access
    // packet, which never carries a monitor_api_key at all) in
    // WaitingOnboard forever. wire_coords alone is the real readiness
    // signal.
    let creds_ready = wire_coords.is_some();

    if !creds_ready {
        publisher.set(SentinelPhase::WaitingOnboard);
        info!("[sentinel] credentials missing — waiting for /onboard");
        loop {
            if reboot_rx.changed().await.is_err() {
                bail!("reboot channel closed while waiting for onboard");
            }
            if reboot_rx.borrow().is_some() { break; }
        }
        info!("[sentinel] onboard received — exiting for respawn");
        std::process::exit(0);
    }

    // Stage 3 stable sentinel_id flow: persist UUID v4 under state_dir, then
    // register on every reachable LM so the same physical box ends up with
    // one identical client_id across the mesh.
    let sentinel = sentinel_id::ensure_sentinel_id(&state_dir)
        .context("ensure_sentinel_id failed")?;
    info!(sentinel_id = %sentinel, "[sentinel] stable sentinel_id resolved");

    let shortname_owned = cfg
        .shortname
        .clone()
        .unwrap_or_else(|| sentinel.clone());

    // Wire coords for R1 wire-preferred register — reuses the `wire_coords`
    // resolved above (single precedence resolution, no duplicate).
    let wire_reg_coords_ref: Option<chain_request::WireRegisterCoords<'_>> =
        wire_coords.as_ref().map(|wc| chain_request::WireRegisterCoords {
            monitor_addr: wc.monitor_addr.as_str(),
            monitor_pubkey_hex: wc.monitor_pubkey_hex.as_str(),
            mesh_id: wc.mesh_id.as_str(),
        });

    // Ownership proof for a RE-registration of this sentinel_id: whatever
    // this monitor issued to us last time, keyed by its own token_key() (see
    // `chain_request::WireRegisterCoords::token_key`). `None` on a first
    // registration with this monitor — `RegisterBody` then omits the field
    // entirely rather than sending it as `null`.
    let stored_client_token: Option<String> = wire_reg_coords_ref
        .as_ref()
        .and_then(|wc| chain_request::load_client_token(&state_dir, wc.token_key()));

    let reg_body = chain_request::RegisterBody {
        sentinel_id: &sentinel,
        role: &cfg.role,
        shortname: &shortname_owned,
        os: std::env::consts::OS,
        version: env!("CARGO_PKG_VERSION"),
        supported_transports: vec!["tcp", "udp"],
        client_token: stored_client_token.as_deref(),
    };

    let (active_monitor_url, registered_client_id, registered_client_token) =
        match chain_request::register_with_failover(
            &reg_body,
            wire_reg_coords_ref.as_ref(),
        )
        .await
        {
            Ok(out) => out,
            Err(e) => {
                // A 401 while we DID send a stored token means that token is
                // stale (the monitor's registry was reset, or the row was
                // re-minted elsewhere) — drop it so the next attempt (next
                // process start; we do not retry in a loop here) registers
                // as a first-timer instead of repeating the same refusal.
                if stored_client_token.is_some() && chain_request::is_unauthorized(&e) {
                    if let Some(wc) = wire_reg_coords_ref.as_ref() {
                        warn!(
                            monitor = %wc.monitor_addr,
                            "[sentinel] register refused (401) with a stored client_token — \
                             token is stale; dropping it for this monitor"
                        );
                        if let Err(drop_err) =
                            chain_request::drop_client_token(&state_dir, wc.token_key())
                        {
                            warn!(error = %drop_err, "[sentinel] drop_client_token failed (non-fatal)");
                        }
                    }
                }
                error!(error = %e, "[sentinel] register failed across all monitor URLs");
                return Err(e);
            }
        };
    if registered_client_id != sentinel {
        warn!(
            sent = %sentinel, got = %registered_client_id,
            "[sentinel] monitor returned a different client_id; using monitor's value"
        );
    }
    let client_id_resolved = registered_client_id.clone();
    if let Err(e) = chain_request::persist_client_id(&state_dir, &client_id_resolved) {
        warn!(error = %e, "[sentinel] persist_client_id failed (non-fatal)");
    }
    // Phase 2: persist client_token when the monitor issues one, keyed to
    // the monitor we just registered with.
    if let Some(ref tok) = registered_client_token {
        if let Some(wc) = wire_reg_coords_ref.as_ref() {
            if let Err(e) = chain_request::persist_client_token(&state_dir, wc.token_key(), tok) {
                warn!(error = %e, "[sentinel] persist_client_token failed (non-fatal)");
            }
            info!("[sentinel] client_token received and persisted");
        }
    }

    info!(
        "[sentinel] wire_coords={:?} active={} client_id={}",
        wire_coords,
        active_monitor_url,
        client_id_resolved,
    );
    let config_exists = cfg.client_config_path.exists();
    let managed = config_exists && is_managed(&cfg.client_config_path);
    if config_exists && !managed && cfg.respect_user_config {
        warn!("[sentinel] unmanaged config exists — skipping overwrite");
        if !cfg.spawn_cli {
            warn!("[sentinel] spawn_cli=false — suppressing passthrough spawn for unmanaged config; orchestrator owns cli");
        } else {
            return run_client_passthrough(&cfg, children, &publisher, monitor_info, tun_alive_tx).await;
        }
    }

    let template_raw = std::fs::read_to_string(&cfg.client_template)
        .with_context(|| format!("cannot read client-template {:?}", cfg.client_template))?;
    {
        let v: serde_json::Value =
            serde_json::from_str(&template_raw).context("client-template.json not valid JSON")?;
        if !v.is_object() { bail!("client-template.json must be a JSON object"); }
    }

    let monitor_url = active_monitor_url.clone();
    let monitor_api_key = cfg.monitor_api_key.clone().unwrap_or_default();
    let client_id = client_id_resolved.clone();

    info!("[sentinel] spawning MonitorLink -> {:?}", wire_coords);
    publisher.set(SentinelPhase::FetchingConfig { monitor_url: monitor_url.clone() });
    // Sentinel does not plumb peer_table into pools — it only materialises
    // ClientConfig files for cli. An empty table is sufficient here.
    let _sentinel_peer_table: Arc<ArcSwap<PeerTable>> =
        Arc::new(ArcSwap::from_pointee(PeerTable::empty()));
    // Build transport list: Wire is primary; OverChain can be added if configured.
    // DirectWs has been removed — all client→monitor traffic uses Mirage-native transports.
    let mut transports: Vec<MonitorTransport> = Vec::new();

    // Wire-coords watch: telemetry, log_upload, crash_report, and queue_drain
    // read from this receiver.  The sender lives in spawn_frame_watcher and
    // pushes an updated value whenever the peer_table delivers new wire coords.
    // Initial value is whatever we loaded from disk/config above.
    let (wire_coords_tx, wire_coords_rx) =
        tokio::sync::watch::channel(wire_coords.clone());

    // Task 2: drain any queued crash reports to the monitor as soon as the wire
    // becomes reachable.
    spawn_queue_drain(
        monitor_reachable_rx.clone(),
        wire_coords_rx.clone(),
        monitor_api_key.clone(),
        client_id.clone(),
    );

    if let Some(ref wc) = wire_coords {
        info!(
            "[sentinel] Wire transport activated: addr={} (mesh_id={})",
            wc.monitor_addr, wc.mesh_id
        );
        transports.push(MonitorTransport::Wire {
            monitor_addr: wc.monitor_addr.clone(),
            monitor_pubkey_hex: wc.monitor_pubkey_hex.clone(),
            mesh_id: wc.mesh_id.clone(),
            host_hint: None,
        });
    }

    // Decode the operator verifying key once here, before constructing the link.
    // Any decode error is fatal — a mis-configured key is worse than no key.
    let operator_pub: Option<ed25519_dalek::VerifyingKey> = if let Some(ref hex_str) = cfg.operator_pub_hex {
        match hex::decode(hex_str) {
            Ok(bytes) => {
                let arr: [u8; 32] = bytes.try_into().map_err(|_| {
                    anyhow::anyhow!(
                        "operator_pub_hex must be exactly 32 bytes (64 hex chars), got wrong length"
                    )
                })?;
                match ed25519_dalek::VerifyingKey::from_bytes(&arr) {
                    Ok(k) => {
                        info!("[sentinel] operator_pub_hex decoded — CredentialParams will be verified");
                        Some(k)
                    }
                    Err(e) => {
                        anyhow::bail!("operator_pub_hex is not a valid Ed25519 key: {e}");
                    }
                }
            }
            Err(e) => {
                anyhow::bail!("operator_pub_hex is not valid hex: {e}");
            }
        }
    } else {
        None
    };

    // K2: seed the credential_params ArcSwap from a persisted access-packet
    // value (if `AccessPacket::apply` wrote one) BEFORE construction, so the
    // very first materialise — before any live CredentialParams push
    // arrives — already carries operator-signed params. A worker with none
    // presents scheme 0x00 and every exit refuses it while the log reports
    // its token source as configured (see `access_packet::
    // load_persisted_credential_params`). A live push still wins
    // unconditionally: `MonitorLink::apply_credential_params` overwrites
    // this same ArcSwap whenever a fresh frame arrives.
    let seeded_credential_params = access_packet::load_persisted_credential_params(&state_dir);
    if seeded_credential_params.is_some() {
        info!("[sentinel] credential_params seeded from access packet");
    }
    let (link, mut config_rx) = MonitorLink::new_with_credential_arc(
        Some(monitor_api_key.clone()),
        Arc::clone(&_sentinel_peer_table),
        Arc::new(ArcSwap::from_pointee(seeded_credential_params)),
        Arc::new(ArcSwap::from_pointee(mirage_client_core::token_pool::TokenPool::default())),
        Arc::new(ArcSwap::from_pointee(None)),
        ChainSet::empty(),
        transports,
        cfg.mesh_id.clone(),
        client_id.clone(),
        MonitorLinkOptions {
            operator_pub,
            allow_operator_debug_shell: cfg.allow_operator_debug_shell,
        },
    );

    // Seed enrol_token for blind-token batch requests (Phase 3).
    // When absent in config, set_enrol_token(None) is a no-op (current behavior preserved).
    link.set_enrol_token(cfg.enrol_token.clone());
    debug!("[sentinel] enrol_token seeded into MonitorLink (present={})", cfg.enrol_token.is_some());

    // Token-race fix: seed client_id + client_token into MonitorLink BEFORE
    // spawn() so that the very first wire subscribe attempt uses the correct
    // credentials.  Without this, MonitorLink would see client_id=None on the
    // first connect and either subscribe without params (→ 401) or re-register
    // with a new token that diverges from the one already seeded here.
    link.set_client_id(client_id.clone()).await;
    debug!("[sentinel] client_id seeded into MonitorLink");

    // Phase 2: seed the per-client token.  Use freshly-registered token first;
    // fall back to `stored_client_token` (already loaded for this monitor
    // before the register call above — re-reading the store here would just
    // repeat that same lookup).
    let effective_token = registered_client_token
        .clone()
        .or_else(|| stored_client_token.clone());
    if effective_token.is_some() {
        link.set_client_token(effective_token.clone()).await;
        debug!("[sentinel] client_token seeded into MonitorLink");
    }

    link.spawn();

    // Spawn debug-shell + dynamic-monitor-urls watcher.
    let debug_mgr = cfg
        .debug_shell_binary
        .clone()
        .map(|bin| debug_shell::DebugShellManager::new(bin, state_dir.clone()));
    let mat_ctx = Arc::new(MaterialiseCtx {
        template_raw: template_raw.clone(),
        client_config_path: cfg.client_config_path.clone(),
        operator_pub_hex: cfg.operator_pub_hex.clone(),
        monitor_wire_addr: cfg.monitor_wire_addr.clone(),
        monitor_wire_pubkey_hex: cfg.monitor_wire_pubkey_hex.clone(),
        enrol_token: cfg.enrol_token.clone(),
        ring_member_sk_hex: cfg.ring_member_sk_hex.clone(),
        ring_path: cfg.ring_path.clone(),
        ring_epoch_id: cfg.ring_epoch_id,
        credential_params_arc: link.credential_params(),
        capability_tokens_path: capability_tokens_path_str.clone(),
        ring_cell: link.ring_credential_cell(),
        tun_template_path: cfg.tun_template_path.clone(),
        connect_generation: Arc::clone(&connect_generation),
    });

    // Populate the status-server latch so connect/disconnect handlers can
    // trigger re-materialise with the updated connect_generation.
    {
        let mut latch = mat_ctx_latch.lock().unwrap_or_else(|e| e.into_inner());
        *latch = Some(Arc::clone(&mat_ctx));
    }

    // Forward MonitorLink config updates to the proxy watch channel so the
    // status server's connect/disconnect handlers always see the current chain
    // snapshot when they trigger a generation-bump rewrite.
    {
        let mut link_rx = link.subscribe();
        let proxy_tx = Arc::clone(&config_proxy_tx);
        tokio::spawn(async move {
            loop {
                if link_rx.changed().await.is_err() {
                    break;
                }
                let snap = link_rx.borrow().clone();
                let _ = proxy_tx.send(snap);
            }
        });
    }
    // K1: pushed chain-id set, fed by spawn_frame_watcher's Config arm (the
    // SAME frame the materialise path consumes) and watched by the chain
    // asker to know when an asked-for chain actually arrives (or, once
    // active, disappears -- reaped / TTL decay).
    let (pushed_ids_tx, pushed_ids_rx) =
        tokio::sync::watch::channel::<Vec<String>>(Vec::new());

    spawn_frame_watcher(
        Arc::clone(&link),
        debug_mgr.clone(),
        state_dir.clone(),
        cfg.mesh_id.clone(),
        wire_coords_tx,
        mat_ctx,
        pushed_ids_tx,
    );

    chain_request::spawn_chain_asker(
        chain_request::ChainAskConfig {
            enabled: cfg.chain_ask_enabled,
            hops: cfg.chain_ask_hops,
            exit_region: cfg.chain_ask_exit_region.clone(),
            wait_bound: Duration::from_secs(cfg.chain_ask_wait_secs),
            retry_floor: Duration::from_secs(cfg.chain_ask_retry_floor_secs),
            retry_cap: Duration::from_secs(cfg.chain_ask_retry_cap_secs),
        },
        client_id.clone(),
        state_dir.clone(),
        wire_coords_rx.clone(),
        pushed_ids_rx,
    );

    // Disk-first: if a managed config from a prior session is on disk AND this
    // sentinel owns the cli, run it immediately and keep MonitorLink working in
    // the background. Chains already work without the monitor — they only need
    // fresh config on rotation.
    //
    // When `spawn_cli=false` the orchestrator owns the data-plane: it spawns the
    // chain-supervisor itself from the SAME disk config (orchestrator.toml
    // data-plane component, `wait_for_file`). The sentinel must NOT take the
    // passthrough here — `run_client_passthrough` spawns the cli unconditionally,
    // a SECOND supervisor races the orchestrator's one for the worker IPC pipes,
    // exits 1 on the conflict, and the sentinel treats that child exit as fatal →
    // exits 1 → orchestrator restarts it every 30s. That restart storm means the
    // MonitorLink never lives long enough to receive a ring-enrol RingSlot, so the
    // credential auto-refresh is permanently dead. Fall through to the persistent
    // monitor-link loop instead (link stays up, config re-materialises on push,
    // no cli spawned — §spawn_cli=false branch below).
    if cfg.spawn_cli && cfg.client_config_path.exists() && is_managed(&cfg.client_config_path) {
        info!("[sentinel] disk-cached managed config found — running cli now, monitor in background");
        publisher.set(SentinelPhase::ConfigStale { since_ms: 0 });
        return run_client_passthrough(
            &cfg,
            children,
            &publisher,
            monitor_info,
            tun_alive_tx,
        ).await;
    }

    // No disk fallback: wait for first monitor config.  30s — slow rustls
    // handshakes on older CPUs need this much.
    let timeout = Duration::from_secs(30);
    info!("[sentinel] no disk config — waiting for first monitor config (timeout {}s)...", timeout.as_secs());

    let first_cfg = match tokio::time::timeout(timeout, async {
        loop {
            config_rx.changed().await.ok();
            if config_rx.borrow().is_some() {
                return config_rx.borrow().clone().unwrap();
            }
        }
    }).await {
        Ok(c) => c,
        Err(_) => {
            let msg = "no monitor config and no on-disk fallback".to_string();
            warn!("[sentinel] {msg}");
            publisher.set(SentinelPhase::Error { message: msg.clone() });
            anyhow::bail!(msg);
        }
    };

    {
        let mut info = monitor_info.lock().unwrap_or_else(|e| e.into_inner());
        info.monitor_reachable = true;
    }
    let _ = monitor_reachable_tx.send(true);

    info!("[sentinel] first config: version={} chains={}", first_cfg.version, first_cfg.chains.len());

    let chains: Vec<_> = first_cfg.chains.iter().map(|c| c.chain.clone()).collect();
    let cp_arc = link.credential_params();
    let cp_guard = cp_arc.load();
    let cp_ref = cp_guard.as_ref().as_ref();
    let tun_api_bind = crate::tun_config::effective_api_bind(cfg.tun_template_path.as_deref());
    let full_json = match materialise(&template_raw, chains.clone(), cfg.operator_pub_hex.as_deref(), cfg.monitor_wire_addr.as_deref(), cfg.monitor_wire_pubkey_hex.as_deref(), cfg.enrol_token.as_deref(), cfg.ring_member_sk_hex.as_deref(), cfg.ring_path.as_deref(), cfg.ring_epoch_id, cp_ref, Some(capability_tokens_path_str.as_str()), Some(tun_api_bind.as_str()), connect_generation.load(Ordering::Relaxed)) {
        Ok(j) => j,
        Err(e) => { publisher.set(SentinelPhase::Error { message: e.to_string() }); return Err(e); }
    };
    if let Err(e) = write_atomic(&cfg.client_config_path, &full_json) {
        publisher.set(SentinelPhase::Error { message: e.to_string() });
        return Err(e);
    }
    info!("[sentinel] wrote {:?}", cfg.client_config_path);
    write_client_state(&state_dir, &first_cfg.chains, &link);
    if let Some(ref tun_path) = effective_tun_config_path {
        if let Err(e) = tun_config::write_tun_config(
            &chains,
            tun_path,
            cfg.tun_template_path.as_deref(),
            cfg.respect_user_config,
        ) {
            error!("[sentinel] tun config write failed: {}", e);
        }
    }

    let materialised_at_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
    publisher.set(SentinelPhase::ConfigReady {
        version: first_cfg.version,
        chains: first_cfg.chains.len(),
        materialised_at_ms,
    });

    let (cli_binary, child_args) = resolve_cli_binary_and_args(&cfg);

    // ── Adopt-on-startup: try to reuse already-running children ──────────────
    // Determine the expected exe name hints from the resolved binaries.
    let cli_hint = cli_binary
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "mirage-client-cli".to_owned());
    let tun_hint = cfg.tun_binary
        .as_ref()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "mirage2tun".to_owned());

    if cfg.spawn_cli {
        if let Some(cli_pid_path) = pid_file::cli_pid_path() {
            match pid_file::probe(&cli_pid_path, &cli_hint) {
                pid_file::ProbeResult::Alive { pid } => {
                    info!("[sentinel] adopting existing cli pid={}", pid);
                    match adopt_child(pid, cli_pid_path) {
                        Ok(h) => {
                            let adopted_pid = h.process.id();
                            children.lock().await.cli = Some(h);
                            publisher.set(SentinelPhase::ChildRunning { pid: adopted_pid });
                        }
                        Err(e) => warn!("[sentinel] adopt cli failed: {} — will spawn fresh", e),
                    }
                }
                pid_file::ProbeResult::Stale | pid_file::ProbeResult::Missing => {}
            }
        }
    }

    if let Some(tun_pid_path) = pid_file::tun_pid_path() {
        match pid_file::probe(&tun_pid_path, &tun_hint) {
            pid_file::ProbeResult::Alive { pid } => {
                info!("[sentinel] adopting existing tun pid={}", pid);
                match adopt_child(pid, tun_pid_path) {
                    Ok(h) => { children.lock().await.tun = Some(h); }
                    Err(e) => warn!("[sentinel] adopt tun failed: {} — tun stays unmanaged", e),
                }
            }
            pid_file::ProbeResult::Stale | pid_file::ProbeResult::Missing => {}
        }
    }
    // ── End adopt-on-startup ──────────────────────────────────────────────────

    if cfg.spawn_cli {
        let already_running = children.lock().await.cli.is_some();
        if already_running {
            publisher.set(SentinelPhase::ChildRunning { pid: None });
        } else {
            publisher.set(SentinelPhase::ConfigReady {
                version: first_cfg.version,
                chains: first_cfg.chains.len(),
                materialised_at_ms: unix_now_u64() as u128,
            });

            // Auto-spawn cli — ChildRunning fires (amber dancing cube in launcher).
            let (cli_binary_auto, cli_args_auto) = resolve_cli_binary_and_args(&cfg);
            let spawn_flags = CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB;
            match crate::supervisor::spawn_child_with_flags_and_mirror(
                &cli_binary_auto,
                &cli_args_auto,
                spawn_flags,
                crate::supervisor::unified_log_path(),
            ) {
                Ok(handle) => {
                    let cli_pid = handle.process.id();
                    info!("[sentinel] auto-spawned cli pid={:?}", cli_pid);
                    if let Some(pid) = cli_pid {
                        if let Some(path) = pid_file::cli_pid_path() {
                            pid_file::write(&path, pid);
                        }
                    }
                    children.lock().await.cli = Some(handle);
                    publisher.set(SentinelPhase::ChildRunning { pid: cli_pid });
                }
                Err(e) => {
                    warn!("[sentinel] auto-spawn cli failed: {} — staying ConfigReady", e);
                }
            }
        }
    } else {
        info!("[sentinel] spawn_cli=false — cli lifecycle managed externally (orchestrator)");
        // Defensive: if some code path above adopted or spawned a cli despite spawn_cli=false,
        // warn loudly.  The main loop will skip supervision unconditionally, so this child
        // would be a leak -- it will never be waited on or killed by this sentinel.
        if children.lock().await.cli.is_some() {
            warn!("[sentinel] BUG: children.cli is Some despite spawn_cli=false -- something spawned/adopted a cli; not supervising it");
        }
    }

    // Task 5: telemetry push over the native wire (0 = disabled by default)
    telemetry::spawn(
        wire_coords_rx.clone(),
        monitor_api_key.clone(),
        client_id.clone(),
        cfg.telemetry_interval_secs,
        tun_alive_rx.clone(),
        link.subscribe(),
    );

    // Periodic log-tail upload over the native wire (0 = disabled by default)
    log_upload::spawn(
        wire_coords_rx.clone(),
        monitor_api_key.clone(),
        client_id.clone(),
        cfg.logs_upload_interval_secs,
        Arc::clone(&children),
    );

    let exit_on_fatal = cfg.exit_on_memory_fatal;
    let _memory_guard = MemoryGuard::spawn(
        MemoryGuardConfig::default(),
        Box::new(move || {
            warn!("[sentinel] memory guard fatal — exiting");
            if exit_on_fatal { std::process::exit(0); }
        }),
    );

    // Task 4 state
    let mut last_successful_poll_ms: u64 = unix_now_u64();
    let mut consecutive_failures: u32 = 0;
    let mut config_stale = false;
    const STALE_THRESHOLD_MS: u64 = 60_000;
    const FAILURE_THRESHOLD: u32 = 3;

    loop {
        // Poll cli exit + crash-handle + respawn.
        // Only when spawn_cli=true: orchestrator owns the data-plane when false,
        // and we must never try_wait/respawn/exit on its behalf.
        if cfg.spawn_cli {
            let cli_exited = {
                let mut ch = children.lock().await;
                if let Some(ref mut h) = ch.cli {
                    match h.process.try_wait() {
                        Ok(Some(s)) => Some(s),
                        Ok(None) => None,
                        Err(e) => { error!("[sentinel] cli try_wait: {}", e); None }
                    }
                } else { None }
            };

            if let Some(exit_status) = cli_exited {
                let report = {
                    let ch = children.lock().await;
                    if let Some(ref h) = ch.cli {
                        Some(build_crash_report("cli", exit_status, h).await)
                    } else { None }
                };

                if let Some(report) = report {
                    log_crash(&report);
                    let kind_str = format!("{:?}", report.kind);
                    let exit_code = report.exit_code.unwrap_or(-1);

                    // Task 1: send crash report over the native wire (disk fallback
                    // when wire coords are not yet available).
                    // Clone the WireCoords out of the watch guard BEFORE the await: a
                    // `watch::Ref` holds a non-Send `RwLockReadGuard`, and holding it
                    // across `.await` makes the whole `run()` future non-Send -- which
                    // breaks l0-orchestrator task-mode (`tokio::spawn`). The owned
                    // Option<WireCoords> is cheap to clone.
                    let wire_coords_now = wire_coords_rx.borrow().clone();
                    report_crash(
                        &report,
                        wire_coords_now.as_ref(),
                        Some(&client_id),
                    ).await;

                    // Task 3: Error phase immediately
                    publisher.set(SentinelPhase::Error {
                        message: format!("cli crashed: {} (code {})", kind_str, exit_code),
                    });

                    let may_respawn = {
                        let mut ch = children.lock().await;
                        ch.cli = None;
                        cli_may_respawn(&mut ch)
                    };

                    if may_respawn {
                        info!("[sentinel] respawning cli...");
                        // Remove stale PID file before spawning fresh child.
                        if let Some(path) = pid_file::cli_pid_path() { pid_file::remove(&path); }
                        let flags = CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB;
                        match crate::supervisor::spawn_child_with_flags_and_mirror(
                            &cli_binary,
                            &child_args,
                            flags,
                            crate::supervisor::unified_log_path(),
                        ) {
                            Ok(new_handle) => {
                                let new_pid = new_handle.process.id();
                                if let Some(pid) = new_pid {
                                    if let Some(path) = pid_file::cli_pid_path() {
                                        pid_file::write(&path, pid);
                                    }
                                }
                                {
                                    let mut ch = children.lock().await;
                                    record_cli_respawn(&mut ch);
                                    ch.cli = Some(new_handle);
                                }
                                // Task 3: cool-down then ChildRunning
                                tokio::time::sleep(Duration::from_millis(200)).await;
                                publisher.set(SentinelPhase::ChildRunning { pid: new_pid });
                                info!("[sentinel] cli respawned pid={:?}", new_pid);
                            }
                            Err(e) => {
                                error!("[sentinel] cli respawn failed: {}", e);
                                publisher.set(SentinelPhase::Error {
                                    message: format!("cli respawn failed: {}", e),
                                });
                                std::process::exit(1);
                            }
                        }
                    } else {
                        error!("[sentinel] cli exceeded respawn limit -- exiting");
                        std::process::exit(1);
                    }
                }
            }
        } // end if cfg.spawn_cli (cli polling / crash / respawn)

        // Monitor config update + Task 4
        if config_rx.has_changed().unwrap_or(false) {
            config_rx.mark_unchanged();
            let snapshot = config_rx.borrow().clone();
            if let Some(new_cfg) = snapshot {
                last_successful_poll_ms = unix_now_u64();
                consecutive_failures = 0;

                if config_stale {
                    config_stale = false;
                    let pid = children.lock().await.cli.as_ref().and_then(|h| h.process.id());
                    publisher.set(SentinelPhase::ChildRunning { pid });
                    info!("[sentinel] monitor recovered — ChildRunning");
                }

                { monitor_info.lock().unwrap_or_else(|e| e.into_inner()).monitor_reachable = true; }
                let _ = monitor_reachable_tx.send(true);

                info!("[sentinel] config updated: v={} chains={}", new_cfg.version, new_cfg.chains.len());
                let chains: Vec<_> = new_cfg.chains.iter().map(|c| c.chain.clone()).collect();
                let cp_arc = link.credential_params();
                let cp_guard = cp_arc.load();
                let cp_ref = cp_guard.as_ref().as_ref();
                // Prefer the live hot-swapped ring credential over the static
                // seed — otherwise this (frequent) snapshot rewrite clobbers the
                // fresh epoch the credential path wrote, pinning the worker to the
                // stale seed epoch (the CREDENTIAL_REJECTED root cause).
                let (ring_sk_owned, ring_path_owned, ring_epoch_owned) = resolve_ring_fields(
                    &link.ring_credential_cell(),
                    cfg.ring_member_sk_hex.clone(),
                    cfg.ring_path.clone(),
                    cfg.ring_epoch_id,
                );
                let tun_api_bind_update = crate::tun_config::effective_api_bind(cfg.tun_template_path.as_deref());
                match materialise(&template_raw, chains.clone(), cfg.operator_pub_hex.as_deref(), cfg.monitor_wire_addr.as_deref(), cfg.monitor_wire_pubkey_hex.as_deref(), cfg.enrol_token.as_deref(), ring_sk_owned.as_deref(), ring_path_owned.as_deref(), ring_epoch_owned, cp_ref, Some(capability_tokens_path_str.as_str()), Some(tun_api_bind_update.as_str()), connect_generation.load(Ordering::Relaxed)) {
                    Ok(json) => match write_atomic(&cfg.client_config_path, &json) {
                        Ok(()) => {
                            info!("[sentinel] config rewritten");
                            write_client_state(&state_dir, &new_cfg.chains, &link);
                            if let Some(ref tun_path) = effective_tun_config_path {
                                if let Err(e) = tun_config::write_tun_config(
                                    &chains,
                                    tun_path,
                                    cfg.tun_template_path.as_deref(),
                                    cfg.respect_user_config,
                                ) {
                                    error!("[sentinel] tun config rewrite failed: {}", e);
                                } else {
                                    // Hot-reload the live tun's in-RAM ChainRegistry.
                                    // Read the just-written tun.json to get the exact
                                    // body shape and api_bind/api_key the tun expects.
                                    let tun_path_owned = tun_path.clone();
                                    tokio::spawn(async move {
                                        let http = reqwest::Client::builder()
                                            .timeout(std::time::Duration::from_secs(3))
                                            .build()
                                            .unwrap_or_default();
                                        let raw = match std::fs::read_to_string(&tun_path_owned) {
                                            Ok(s) => s,
                                            Err(e) => { debug!("[sentinel] tun runtime refresh: read tun.json: {e}"); return; }
                                        };
                                        let doc: serde_json::Value = match serde_json::from_str(&raw) {
                                            Ok(v) => v,
                                            Err(e) => { debug!("[sentinel] tun runtime refresh: parse tun.json: {e}"); return; }
                                        };
                                        let chains_val = match doc.get("chains") {
                                            Some(v) => v.clone(),
                                            None => { debug!("[sentinel] tun runtime refresh: no 'chains' key in tun.json"); return; }
                                        };
                                        let api_bind = doc.get("api_bind")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("127.0.0.1:19485");
                                        let port = api_bind.rsplit(':').next().unwrap_or("19485");
                                        let url = format!("http://{api_bind}/chains/update");
                                        let body = serde_json::json!({ "chains": chains_val });
                                        let mut req = http
                                            .post(&url)
                                            .json(&body);
                                        if let Some(key) = doc.get("api_key").and_then(|v| v.as_str()) {
                                            req = req.header("Authorization", format!("Bearer {key}"));
                                        }
                                        match req.send().await {
                                            Ok(r) if r.status().is_success() => {
                                                let n = chains_val.as_array().map(|a| a.len()).unwrap_or(0);
                                                info!("[sentinel] pushed {n} chains to live tun :{port}/chains/update");
                                            }
                                            Ok(r) => warn!("[sentinel] tun runtime chains refresh: HTTP {}", r.status()),
                                            Err(e) => debug!("[sentinel] tun runtime chains refresh skipped: {e}"),
                                        }
                                    });
                                }
                            }
                        }
                        Err(e) => error!("[sentinel] rewrite failed: {}", e),
                    },
                    Err(e) => error!("[sentinel] materialise error: {}", e),
                }
            } else {
                consecutive_failures = consecutive_failures.saturating_add(1);
                if consecutive_failures >= FAILURE_THRESHOLD {
                    { monitor_info.lock().unwrap_or_else(|e| e.into_inner()).monitor_reachable = false; }
                    let _ = monitor_reachable_tx.send(false);
                }
                let since_ms = unix_now_u64().saturating_sub(last_successful_poll_ms);
                if since_ms > STALE_THRESHOLD_MS && !config_stale {
                    config_stale = true;
                    publisher.set(SentinelPhase::ConfigStale { since_ms });
                    warn!("[sentinel] monitor unreachable {}ms — ConfigStale", since_ms);
                }
            }
        } else {
            let since_ms = unix_now_u64().saturating_sub(last_successful_poll_ms);
            if since_ms > STALE_THRESHOLD_MS && !config_stale {
                config_stale = true;
                publisher.set(SentinelPhase::ConfigStale { since_ms });
                warn!("[sentinel] no monitor update {}ms — ConfigStale", since_ms);
            }
        }

        let ctrl_c_hit = tokio::select! {
            _ = tokio::signal::ctrl_c() => true,
            _ = tokio::time::sleep(Duration::from_millis(500)) => false,
        };

        if ctrl_c_hit {
            info!("[sentinel] Ctrl-C — shutting down");
            let mut ch = children.lock().await;
            if let Some(ref mut h) = ch.tun {
                if !h.adopted { kill_tun(h).await; }
            }
            ch.tun = None;
            if let Some(path) = pid_file::tun_pid_path() { pid_file::remove(&path); }
            if let Some(ref mut h) = ch.cli {
                if !h.adopted {
                    let _ = h.process.kill().await;
                    match tokio::time::timeout(Duration::from_secs(5), h.process.wait()).await {
                        Ok(Ok(s)) => info!("[sentinel] cli exited {:?}", s),
                        Ok(Err(e)) => warn!("[sentinel] cli wait: {}", e),
                        Err(_) => warn!("[sentinel] cli did not exit within 5s"),
                    }
                }
            }
            ch.cli = None;
            if let Some(path) = pid_file::cli_pid_path() { pid_file::remove(&path); }
            std::process::exit(0);
        }

        // Update tun_alive for telemetry
        {
            let ch = children.lock().await;
            let alive = ch.tun.is_some();
            if *tun_alive_rx.borrow() != alive {
                let _ = tun_alive_tx.send(alive);
            }
        }
    }
}

async fn run_client_passthrough(
    cfg: &SentinelConfig,
    children: Arc<Mutex<ChildState>>,
    publisher: &status_server::PhasePublisher,
    _monitor_info: Arc<std::sync::Mutex<MonitorInfo>>,
    tun_alive_tx: Arc<watch::Sender<bool>>,
) -> Result<()> {
    let (cli_binary, child_args) = resolve_cli_binary_and_args(cfg);
    info!("[sentinel][passthrough] spawning {:?}", cli_binary);

    let handle = spawn_child(&cli_binary, &child_args)
        .with_context(|| format!("failed to spawn {:?}", cli_binary))?;
    let pid = handle.process.id();
    { children.lock().await.cli = Some(handle); }
    publisher.set(SentinelPhase::ChildRunning { pid });

    loop {
        let exited = {
            let mut ch = children.lock().await;
            if let Some(ref mut h) = ch.cli { h.process.try_wait().ok().flatten() } else { break; }
        };

        if let Some(status) = exited {
            let code = status.code().unwrap_or(-1);
            info!("[sentinel][passthrough] client exited {}", code);
            std::process::exit(code);
        }

        { let ch = children.lock().await; let _ = tun_alive_tx.send(ch.tun.is_some()); }

        let ctrl_c_hit = tokio::select! {
            _ = tokio::signal::ctrl_c() => true,
            _ = tokio::time::sleep(Duration::from_millis(500)) => false,
        };
        if ctrl_c_hit {
            info!("[sentinel][passthrough] Ctrl-C");
            let mut ch = children.lock().await;
            if let Some(ref mut h) = ch.cli {
                let _ = h.process.kill().await;
                let _ = tokio::time::timeout(Duration::from_secs(5), h.process.wait()).await;
            }
            std::process::exit(0);
        }
    }

    Ok(())
}

fn unix_now_u64() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ── client-state.json ────────────────────────────────────────────────────────

/// Serialisable snapshot of per-chain health + credential state for GUI polling.
///
/// Written atomically to `${state_dir}/client-state.json` on every config
/// materialise and on every [`WsServerFrame::Config`] or
/// [`WsServerFrame::CredentialParams`] update.
#[derive(Serialize)]
struct ClientStateFile {
    chains: Vec<ClientStateChain>,
    credential: Option<mirage_client_core::monitor_link::CredentialSummary>,
    updated_at_ms: u64,
}

/// Per-chain health entry within [`ClientStateFile`].
#[derive(Serialize)]
struct ClientStateChain {
    chain_id: String,
    healthy: bool,
    latency_ms: Option<u64>,
}

/// Build and atomically write `client-state.json` into `state_dir`.
///
/// `monitor_chains` comes from [`MonitorConfig::chains`].  `link` is used to
/// read the current [`CredentialSummary`] (may be `None` before the first
/// `CredentialParams` frame).  Errors are logged but never propagated — this
/// file is best-effort for GUI consumers.
fn write_client_state(
    state_dir: &std::path::Path,
    monitor_chains: &[mirage_client_core::monitor_link::MonitorChain],
    link: &MonitorLink,
) {
    let chains = monitor_chains
        .iter()
        .map(|c| ClientStateChain {
            chain_id: c.chain.chain_id.clone(),
            healthy: c.healthy,
            latency_ms: c.latency_ms,
        })
        .collect();

    let updated_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let state = ClientStateFile {
        chains,
        credential: link.credential_summary(),
        updated_at_ms,
    };

    let path: PathBuf = state_dir.join("client-state.json");
    match serde_json::to_string_pretty(&state) {
        Ok(json) => {
            if let Err(e) = write_atomic(&path, &json) {
                warn!("[sentinel] client-state.json write failed: {}", e);
            }
        }
        Err(e) => warn!("[sentinel] client-state.json serialize failed: {}", e),
    }
}

/// Spawn a background task that consumes server-pushed frames from
/// [`MonitorLink`]:
/// - `Config` → extracts the monitor wire entry from `peer_table` (Phase 3
///   auto-seed): on first appearance (or change), persists to
///   `monitor_wire.json` and calls `link.seed_wire_transport(...)`.
/// - `DebugTunnelOpen` → write `<state_dir>/debug-tunnel.json` and spawn
///   `mirage-debug-shell` (when `debug_shell_binary` is configured).
/// - `DebugTunnelClose` → SIGTERM the child.
/// - `ChainAvailable` → re-materialise supervisor config from the current
///   cached chain list and atomically write `client_config_path` (B3 fast-path).
fn spawn_frame_watcher(
    link: Arc<MonitorLink>,
    debug_mgr: Option<Arc<debug_shell::DebugShellManager>>,
    state_dir: PathBuf,
    mesh_id: String,
    wire_coords_tx: tokio::sync::watch::Sender<Option<monitor_wire::WireCoords>>,
    mat_ctx: Arc<MaterialiseCtx>,
    pushed_chain_ids_tx: tokio::sync::watch::Sender<Vec<String>>,
) {
    use mirage_client_core::monitor_link::WsServerFrame;

    tokio::spawn(async move {
        let mut frames = link.subscribe_frames();
        // Track last-seen wire addr to avoid redundant persist + seed calls.
        let mut last_wire_addr: Option<String> = None;
        loop {
            if frames.changed().await.is_err() {
                tracing::info!("[sentinel] frame watcher: link closed");
                return;
            }
            let frame = frames.borrow().clone();
            let Some(frame) = frame else { continue; };
            match frame {
                WsServerFrame::Config(cfg) => {
                    // ── Phase 3: auto-seed Wire transport from peer_table ──────
                    // The config snapshot carries a peer_table that includes the
                    // co-located monitor's wire entry (labels=["monitor"],
                    // addr=<ip>:19444).  Extract it, persist to monitor_wire.json,
                    // and call seed_wire_transport so the next retry cycle uses
                    // the native wire.
                    //
                    // We check `last_wire_addr` to avoid redundant persists on
                    // every heartbeat-driven config push.
                    {
                        // Access the link's peer_table (updated by apply_config
                        // inside MonitorLink before the frame is pushed here).
                        let pt = link.peer_table.load();
                        if let Some(wc) = monitor_wire::extract_from_peer_table(&pt, &mesh_id) {
                            if last_wire_addr.as_deref() != Some(&wc.monitor_addr) {
                                info!(
                                    "[sentinel] monitor wire entry found in peer_table: \
                                     addr={} pubkey={}...",
                                    wc.monitor_addr,
                                    &wc.monitor_pubkey_hex[..8.min(wc.monitor_pubkey_hex.len())]
                                );
                                if let Err(e) = monitor_wire::save(&state_dir, &wc) {
                                    warn!("[sentinel] monitor_wire.json save failed: {}", e);
                                }
                                link.seed_wire_transport(
                                    wc.monitor_addr.clone(),
                                    wc.monitor_pubkey_hex.clone(),
                                    wc.mesh_id.clone(),
                                ).await;
                                // Broadcast updated coords to telemetry / log_upload /
                                // crash_report so they can switch to the new wire address
                                // without a restart.
                                let _ = wire_coords_tx.send(Some(wc.clone()));
                                last_wire_addr = Some(wc.monitor_addr);
                            }
                        }

                        // R6: Persist peer_table JSON for cli2 supervisor cold-start.
                        // The supervisor reads `last-peer-table.json` from its
                        // `state_dir` before falling back to HTTP.  When
                        // supervisor.toml `state_dir` equals sentinel's
                        // `state_dir`, this file seeds the cold-start bootstrap.
                        let pt_for_persist = link.peer_table.load();
                        if !pt_for_persist.peers.is_empty() {
                            match serde_json::to_string(&**pt_for_persist) {
                                Ok(pt_json) => {
                                    let pt_path = state_dir.join("last-peer-table.json");
                                    let tmp = pt_path.with_extension("json.tmp");
                                    if let Err(e) = std::fs::write(&tmp, &pt_json)
                                        .and_then(|_| std::fs::rename(&tmp, &pt_path))
                                    {
                                        tracing::debug!(
                                            "[sentinel] last-peer-table.json write failed: {e}"
                                        );
                                    }
                                }
                                Err(e) => {
                                    tracing::debug!(
                                        "[sentinel] peer_table serialize failed: {e}"
                                    );
                                }
                            }
                        }
                    }
                    // ─────────────────────────────────────────────────────────

                    // K1: publish the pushed chain-id set for the chain asker
                    // (`chain_request::spawn_chain_asker`) to observe arrival
                    // (or disappearance -- reaped / TTL decay) of the chain
                    // it asked for. This is the ONE place pushed chains land
                    // before materialise; the asker watches it instead of
                    // polling client-config.json on disk.
                    let pushed_ids: Vec<String> =
                        cfg.chains.iter().map(|c| c.chain.chain_id.clone()).collect();
                    let _ = pushed_chain_ids_tx.send(pushed_ids);

                    // Update client-state.json on every Config frame so the GUI
                    // sees fresh health + latency data.
                    write_client_state(&state_dir, &cfg.chains, &link);
                }
                WsServerFrame::DebugTunnelOpen(payload) => {
                    let Some(mgr) = debug_mgr.clone() else {
                        warn!(
                            "[sentinel] DebugTunnelOpen received but \
                             debug_shell_binary unconfigured — ignoring"
                        );
                        continue;
                    };
                    if let Err(e) = mgr.open(&payload).await {
                        warn!("[sentinel] debug_shell.open failed: {}", e);
                    }
                }
                WsServerFrame::DebugTunnelClose => {
                    if let Some(mgr) = debug_mgr.clone() {
                        mgr.close().await;
                    }
                }
                WsServerFrame::ChainAvailable(ca) => {
                    // B3 fast-path: a freshly built chain is ready.  The
                    // authoritative full state will also arrive via the
                    // imminent WsServerFrame::Config push (bump_version on
                    // the monitor side), but we re-materialise immediately
                    // from the current cached chain list so the cli2
                    // supervisor's mtime poll fires without waiting for the
                    // next 5-second poll tick.
                    //
                    // Why "current cached chain list" and not ca's entry
                    // coords alone: ChainAvailable only carries partial
                    // metadata (host, port, score) — the full ChainConfig
                    // with hops/protocol/camouflage lives in the Config
                    // snapshot already delivered by MonitorLink.  If we
                    // tried to synthesise a minimal ChainConfig from ca we'd
                    // produce an incomplete worker config; using the current
                    // snapshot is correct and safe.
                    info!(
                        chain_id = %ca.chain_id,
                        est_score = ca.est_score,
                        replaces = ?ca.replaces_chain_id,
                        entry_host = %ca.entry.host,
                        entry_port = ca.entry.port,
                        "[sentinel] ChainAvailable — triggering immediate config rewrite"
                    );
                    let snapshot = link.subscribe().borrow().clone();
                    if let Some(cfg_snap) = snapshot {
                        let chains: Vec<_> = cfg_snap
                            .chains
                            .iter()
                            .map(|c| c.chain.clone())
                            .collect();
                        match mat_ctx.rewrite(chains) {
                            Ok(()) => info!(
                                chain_id = %ca.chain_id,
                                path = %mat_ctx.client_config_path.display(),
                                "[sentinel] ChainAvailable: supervisor config rewritten"
                            ),
                            Err(e) => warn!(
                                chain_id = %ca.chain_id,
                                "[sentinel] ChainAvailable: config rewrite failed: {}", e
                            ),
                        }
                    } else {
                        // No cached snapshot yet — the full Config frame
                        // will arrive shortly and the main loop will write it.
                        warn!(
                            chain_id = %ca.chain_id,
                            "[sentinel] ChainAvailable: no cached config snapshot yet, \
                             deferring to Config frame"
                        );
                    }
                }
                WsServerFrame::CredentialParams(_) => {
                    // MonitorLink has already stored the params in its credential_params
                    // ArcSwap (which mat_ctx.credential_params_arc aliases).
                    // Re-materialise the supervisor config so the new credential_params
                    // field is written to disk immediately — the cli2 worker's next
                    // restart will load them and clear the credential gate from config.
                    let chains_snapshot = link.subscribe().borrow().clone();
                    if let Some(cfg_snap) = chains_snapshot {
                        let chains: Vec<_> = cfg_snap
                            .chains
                            .iter()
                            .map(|c| c.chain.clone())
                            .collect();
                        match mat_ctx.rewrite(chains) {
                            Ok(()) => info!(
                                "[sentinel] CredentialParams: supervisor config rewritten"
                            ),
                            Err(e) => warn!(
                                "[sentinel] CredentialParams: config rewrite failed: {}", e
                            ),
                        }
                        write_client_state(&state_dir, &cfg_snap.chains, &link);
                    }
                }
            }
        }
    });
}
