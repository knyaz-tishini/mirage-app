//! Register + chain-ask helpers used by sentinel boot.
//!
//! This module owns the failover register + client_id persistence path, the
//! per-monitor client_token store, and (K1) the chain-ask cycle: the
//! sentinel asks the monitor to BUILD a chain for this client
//! (`POST /relay/clients/{id}/chains/request`) instead of only waiting to be
//! handed one. Asking does not replace the push — the materialised chain
//! still arrives the ordinary way, through `WsServerFrame::Config`; asking
//! only gives the monitor something to push. See
//! `docs/mirage/plans/client-track-checklist-2026-09-17.md` K1.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

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

// ── K2 v2: identity-bearing sentinel registration ───────────────────────────
//
// A v2 (`access_packet::AccessPacketV2`) device registers via its
// operator-declared identity — never anonymously, and there is no fallback
// to the ordinary `register_with_failover` path above. `POST
// /relay/clients/sentinel` mirrors `mirage2monitor::clients_api`'s existing
// `/relay/clients/edge` route: the operator signature on `identity` proves
// the OPERATOR vouched for this self_id/role/key set, and `possession_sig`
// proves THIS CALLER holds the identity's own signing key, over a
// nonce+timestamp so a captured proof cannot be replayed later.

/// Body for `POST /relay/clients/sentinel`. Field-for-field the same shape as
/// `mirage2monitor::clients_api::EdgeClientRegisterBody`, with `shortname`
/// carried over from the ordinary (v1) `RegisterBody`.
#[derive(Debug, Serialize)]
struct SentinelRegisterBody<'a> {
    identity: &'a mirage_core::identity::DeclaredIdentity,
    nonce_hex: String,
    ts_ms: u64,
    possession_sig: mirage_core::identity::SignatureHex,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    shortname: Option<&'a str>,
}

/// Domain-separated preimage bytes for [`SentinelRegisterBody::possession_sig`]
/// — the SAME layout as `mirage2monitor::clients_api::
/// edge_register_signing_bytes`, with the label swapped for this route's own
/// domain separation. `monitor_self_id` is `AccessPacketV2Monitor::self_id`
/// (the target monitor's own operator-declared identifier, carried in the
/// packet alongside `wire_addr`/`wire_pubkey_hex`) — binding it into the
/// preimage is what stops a proof minted for one monitor from verifying
/// against a different monitor in the same mesh, mirroring the reasoning
/// `edge_register_signing_bytes`'s own doc gives for its identical parameter.
pub fn sentinel_register_signing_bytes(
    self_id: uuid::Uuid,
    mesh_id: &str,
    monitor_self_id: &str,
    nonce_hex: &str,
    ts_ms: u64,
) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"MIRAGE-SENTINEL-CLIENT-REGISTER-V1\0");
    buf.extend_from_slice(self_id.to_string().as_bytes());
    buf.push(0);
    buf.extend_from_slice(mesh_id.as_bytes());
    buf.push(0);
    buf.extend_from_slice(monitor_self_id.as_bytes());
    buf.push(0);
    buf.extend_from_slice(nonce_hex.as_bytes());
    buf.push(0);
    buf.extend_from_slice(ts_ms.to_string().as_bytes());
    buf
}

/// Success body from `POST /relay/clients/sentinel` — `client_id` is always
/// `identity.self_id` (never a freshly minted or caller-chosen id, mirroring
/// the edge route's own rule).
#[derive(Debug, Deserialize)]
struct SentinelRegisterResponseBody {
    client_id: String,
    #[serde(default)]
    client_token: Option<String>,
}

/// A refusal's `error`/`detail` JSON fields, when the monitor sends them
/// (mirroring `EdgeClientRegisterError::label()`'s wire shape). Best-effort:
/// a monitor that answers with a non-JSON or differently-shaped body still
/// gets a reason string — the raw response body, lossily decoded.
#[derive(Debug, Deserialize)]
struct SentinelRegisterErrorBody {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    detail: Option<String>,
}

/// Honest classification of a `POST /relay/clients/sentinel` answer —
/// mirrors [`ChainAskOutcome`]'s own split between a granted 2xx and a named
/// refusal.
#[derive(Debug, Clone)]
enum SentinelRegisterOutcome {
    Registered { client_id: String, client_token: Option<String> },
    Refused { reason: String, status: u16 },
}

/// Classify a `/relay/clients/sentinel` HTTP response. Pure — no I/O — so
/// refusal-reason extraction is unit-testable without a wire dial, mirroring
/// [`classify_chain_ask_response`]'s own shape.
fn classify_sentinel_register_response(status: u16, body: &[u8]) -> Result<SentinelRegisterOutcome> {
    if (200..300).contains(&status) {
        let parsed: SentinelRegisterResponseBody =
            serde_json::from_slice(body).context("parse sentinel register response JSON")?;
        return Ok(SentinelRegisterOutcome::Registered {
            client_id: parsed.client_id,
            client_token: parsed.client_token,
        });
    }
    let reason = serde_json::from_slice::<SentinelRegisterErrorBody>(body)
        .ok()
        .and_then(|b| b.error.or(b.detail))
        .unwrap_or_else(|| String::from_utf8_lossy(body).into_owned());
    Ok(SentinelRegisterOutcome::Refused { reason, status })
}

/// One registration attempt: builds a fresh nonce + timestamp, signs
/// [`sentinel_register_signing_bytes`] with the device's own Ed25519 signing
/// key, and dials the monitor over the wire — pinning `monitor.wire_pubkey_hex`
/// and presenting the device's X25519 identity key as the wire hello's
/// static key (never an ephemeral one; see
/// `mirage_client_core::monitor_link::wire_post_no_bearer_with_static_key`'s
/// own doc for why).
async fn try_register_sentinel_v2(
    device: &crate::access_packet::AccessPacketV2,
    shortname: Option<&str>,
) -> Result<SentinelRegisterOutcome> {
    use ed25519_dalek::Signer;

    let self_id = uuid::Uuid::parse_str(&device.self_id)
        .context("device self_id is not a valid UUID (checked at apply-time; should be unreachable)")?;

    let signing_priv_bytes = hex::decode(&device.client_signing_private_key_hex)
        .context("decode client_signing_private_key_hex")?;
    let signing_priv_arr: [u8; 32] = signing_priv_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("client_signing_private_key_hex must be 32 bytes"))?;
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&signing_priv_arr);

    // 16 random bytes via a fresh UUID v4's own bytes — reuses the `uuid`
    // dependency already in this crate rather than adding a general-purpose
    // RNG crate for one nonce; a v4 UUID's randomness (122 bits) is ample
    // freshness salt for an anti-replay nonce that already sits alongside a
    // bounded `ts_ms` window.
    let nonce_hex = hex::encode(*uuid::Uuid::new_v4().as_bytes());
    let ts_ms: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let preimage = sentinel_register_signing_bytes(
        self_id,
        &device.mesh_id,
        &device.monitor.self_id,
        &nonce_hex,
        ts_ms,
    );
    let sig = signing_key.sign(&preimage);
    let possession_sig = mirage_core::identity::SignatureHex::from_signature(&sig);

    let body = SentinelRegisterBody {
        identity: &device.declared_identity,
        nonce_hex,
        ts_ms,
        possession_sig,
        shortname,
    };
    let body_json = serde_json::to_vec(&body).context("serialize sentinel register body")?;

    let client_priv_bytes =
        hex::decode(&device.client_private_key_hex).context("decode client_private_key_hex")?;
    let client_priv_arr: [u8; 32] = client_priv_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("client_private_key_hex must be 32 bytes"))?;

    let (status, resp_body) = mirage_client_core::monitor_link::wire_post_no_bearer_with_static_key(
        &device.monitor.wire_addr,
        &device.monitor.wire_pubkey_hex,
        &device.mesh_id,
        &client_priv_arr,
        "/relay/clients/sentinel",
        &body_json,
    )
    .await
    .map_err(|e| anyhow::anyhow!("sentinel register wire_post: {e}"))?;

    classify_sentinel_register_response(status, &resp_body)
}

/// Register a v2 (identity-bearing) sentinel with the monitor named in
/// `device.monitor`. Loops with the SAME floor/cap backoff schedule
/// [`Backoff`] gives the K1 chain-ask cycle (30s floor / 5min cap) on every
/// refusal — named, logged, never a tight spin — because a v2 device has NO
/// anonymous fallback: [`register_with_failover`]'s ordinary path is simply
/// not available once identity-bearing registration is in play (see this
/// module's own doc, "Register via identity, never anonymously"). Returns
/// only on success; mirrors [`register_with_failover`]'s own return shape
/// `(active_url, client_id, client_token)` so `lib.rs::run`'s subsequent
/// subscribe/config/chain flow needs no v2-specific branching past this
/// call.
pub async fn register_sentinel_v2(
    device: &crate::access_packet::AccessPacketV2,
    shortname: Option<&str>,
) -> Result<(String, String, Option<String>)> {
    let mut backoff = Backoff::new(Duration::from_secs(30), Duration::from_secs(300));
    loop {
        match try_register_sentinel_v2(device, shortname).await {
            Ok(SentinelRegisterOutcome::Registered { client_id, client_token }) => {
                tracing::info!(
                    client_id = %client_id,
                    "[sentinel] v2 register: monitor accepted this device's identity"
                );
                return Ok((format!("wire://{}", device.monitor.wire_addr), client_id, client_token));
            }
            Ok(SentinelRegisterOutcome::Refused { reason, status }) => {
                tracing::warn!(
                    reason = %reason,
                    status,
                    "[sentinel] v2 register refused — backing off, never spinning"
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "[sentinel] v2 register failed (transport) — backing off, never spinning"
                );
            }
        }
        tokio::time::sleep(backoff.next()).await;
    }
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

// ── K1: chain ask (sentinel asks for its own chain) ─────────────────────────
//
// The sentinel asks the monitor to BUILD a chain for this client instead of
// only waiting for the monitor's own push schedule to hand one over. This
// does NOT replace the push: the materialised `ChainConfig` still arrives
// through the ordinary `WsServerFrame::Config` frame — asking only gives the
// monitor something to build and push. See `lib.rs::spawn_frame_watcher`
// (the `WsServerFrame::Config` arm) for where the pushed chain-id set is
// published, and `lib.rs::run` for where the asker task is spawned.

/// Wire body for `POST /relay/clients/{client_id}/chains/request`.
///
/// Mirrors `mirage2monitor::auto_chain::AxisSpec` (JSON shape only — this
/// crate takes NO dependency on the monitor crate, per the client/monitor
/// boundary). Field order, names, and enum tag strings below reproduce
/// `AxisSpec`'s `#[derive(Serialize)]` output exactly (`rename_all =
/// "snake_case"`, `exit_mode` internally tagged on `"kind"`). Diff against
/// `mirage2monitor/crates/monitor/src/auto_chain.rs` (`AxisSpec`,
/// `default_auto_axis_spec`) if the wire shape ever drifts.
///
/// Every field below carries `default_auto_axis_spec()`'s literal value
/// except `hops`, which `SentinelConfig::chain_ask_hops` controls.
/// `exclude_nodes` is omitted entirely — the monitor's field is
/// `#[serde(skip_serializing_if = "Vec::is_empty")]` and we never populate it.
#[derive(Debug, Clone, Serialize)]
pub struct ChainAskSpec {
    pub transport: &'static str,
    pub camouflage: &'static str,
    pub hops: u8,
    pub relay_mode: &'static str,
    pub exit_mode: ChainAskExitMode,
    pub aead_suite: &'static str,
    pub byte_cipher: &'static str,
    pub reply_mode: &'static str,
    pub handshake: &'static str,
    pub anchor: ChainAskAnchor,
    pub ttl_secs: Option<u64>,
    pub camo_params: Option<serde_json::Value>,
}

/// Mirrors `mirage2monitor::auto_chain::ExitKind` (internally tagged on
/// `"kind"`). Only the `Direct` shape is ever sent.
#[derive(Debug, Clone, Serialize)]
pub struct ChainAskExitMode {
    pub kind: &'static str,
}

/// Mirrors `mirage2monitor::auto_chain::Anchor`. `exit_region` carries K4's
/// egress-country ask (`SentinelConfig::chain_ask_exit_region`) when the box
/// has one configured; every other field stays the empty default — the
/// sentinel does not pin a topology or an entry region.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ChainAskAnchor {
    pub entry_region: Option<String>,
    pub exit_region: Option<String>,
    pub entry_node_id: Option<String>,
    pub exit_node_id: Option<String>,
    pub hop_nodes: Vec<String>,
}

/// Build the chain-ask body: `default_auto_axis_spec()`'s values verbatim,
/// with `hops` overridden by the sentinel's own knob and `anchor.exit_region`
/// set from `exit_region` (K4 — the box's configured egress country, `None`
/// for full-auto).
pub fn default_chain_ask_spec(hops: u8, exit_region: Option<String>) -> ChainAskSpec {
    ChainAskSpec {
        transport: "tcp",
        camouflage: "none",
        hops,
        relay_mode: "onion_peel",
        exit_mode: ChainAskExitMode { kind: "direct" },
        aead_suite: "cha_cha20_poly1305",
        byte_cipher: "none",
        reply_mode: "onion",
        handshake: "telescopic",
        anchor: ChainAskAnchor {
            exit_region,
            ..ChainAskAnchor::default()
        },
        ttl_secs: None,
        camo_params: None,
    }
}

/// Success body from `POST /relay/clients/{client_id}/chains/request`
/// (`mirage2monitor::client_chains::RequestChainResponse`). `hops` is read
/// only for its length — the per-hop shape (`BuildHop`) is not needed here
/// and mirroring it would just be one more thing to keep in sync.
#[derive(Debug, Deserialize)]
struct ChainAskResponseBody {
    chain_id: String,
    expires_at_ms: u64,
    #[serde(default)]
    cached: bool,
    #[serde(default)]
    hops: Vec<serde_json::Value>,
}

/// Honest classification of a `chains/request` answer.
#[derive(Debug, Clone)]
pub enum ChainAskOutcome {
    /// 2xx — the monitor built (or returned a cached) chain.
    Granted {
        chain_id: String,
        expires_at_ms: u64,
        cached: bool,
        hop_count: usize,
    },
    /// `403` whose body is the policy gate's `format!("policy reject: {why}")`
    /// (`mirage2monitor::client_chains::handle_request_chain`) — a
    /// CONFIGURATION fact (this client's axis policy forbids the spec), not
    /// a transport error. Never retried on the fast path — see
    /// [`ChainAskCycle::on_outcome`].
    PolicyRejected { reason: String },
    /// Any other non-2xx status.
    Refused { status: u16, body: String },
}

/// Classify a `chains/request` HTTP response. Pure — no I/O — so the
/// 403-vs-other distinction is unit-testable without a wire dial.
/// [`ask_for_chain`] is the network-calling wrapper around this.
fn classify_chain_ask_response(status: u16, body: &[u8]) -> Result<ChainAskOutcome> {
    if (200..300).contains(&status) {
        let parsed: ChainAskResponseBody =
            serde_json::from_slice(body).context("parse chains/request response JSON")?;
        return Ok(ChainAskOutcome::Granted {
            chain_id: parsed.chain_id,
            expires_at_ms: parsed.expires_at_ms,
            cached: parsed.cached,
            hop_count: parsed.hops.len(),
        });
    }
    let body_text = String::from_utf8_lossy(body).into_owned();
    if status == 403 {
        let reason = body_text
            .strip_prefix("policy reject: ")
            .map(str::to_owned)
            .unwrap_or(body_text);
        return Ok(ChainAskOutcome::PolicyRejected { reason });
    }
    Ok(ChainAskOutcome::Refused {
        status,
        body: body_text,
    })
}

/// Ask the monitor (over the wire, bearer = the per-monitor client_token from
/// [`load_client_token`]) to build `spec` for `client_id`.
///
/// `wc` MUST be the coords of the monitor `bearer` was minted by — see
/// [`WireRegisterCoords::token_key`].
pub async fn ask_for_chain(
    wc: &WireRegisterCoords<'_>,
    bearer: &str,
    client_id: &str,
    spec: &ChainAskSpec,
) -> Result<ChainAskOutcome> {
    let body = serde_json::to_vec(spec).context("serialize chain-ask AxisSpec")?;
    let path = format!("/relay/clients/{client_id}/chains/request");
    let (status, resp_body) = mirage_client_core::monitor_link::wire_post(
        wc.monitor_addr,
        wc.monitor_pubkey_hex,
        wc.mesh_id,
        &path,
        bearer,
        &body,
    )
    .await
    .map_err(|e| anyhow::anyhow!("chain ask wire_post: {e}"))?;
    classify_chain_ask_response(status, &resp_body)
}

/// Exponential backoff with a floor and a cap. `next()` returns the CURRENT
/// wait and grows for the next call; `reset()` drops back to the floor.
/// Growth is doubling — matches the retry cadence described in K1 (floor 30s
/// → cap 5min).
#[derive(Debug, Clone, Copy)]
struct Backoff {
    floor: Duration,
    cap: Duration,
    current: Duration,
}

impl Backoff {
    fn new(floor: Duration, cap: Duration) -> Self {
        Self {
            floor,
            cap,
            current: floor,
        }
    }

    fn next(&mut self) -> Duration {
        let d = self.current;
        self.current = self.current.saturating_mul(2).min(self.cap);
        d
    }

    fn reset(&mut self) {
        self.current = self.floor;
    }
}

/// One outstanding ask, awaiting confirmation through the pushed chain-id set.
struct PendingAsk {
    chain_id: String,
    requested_at: Instant,
    /// Guards the wait-bound WARN so it fires once per pending ask, not once
    /// per poll tick — see [`ChainAskCycle::tick`].
    warned: bool,
}

/// Action [`ChainAskCycle::tick`] asks the caller to perform. The caller
/// (`issue_ask` / the async loop in `spawn_chain_asker`) owns all `tracing::`
/// calls and the actual wire dial — this type and [`ChainAskCycle`] stay
/// free of both so the decision logic is unit-testable without a runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ChainAskAction {
    /// The wait bound elapsed without the asked-for chain appearing in the
    /// pushed set. Fires exactly this one WARN — never per poll.
    WarnNeverPushed { chain_id: String, waited: Duration },
    /// The retry backoff elapsed (after a timeout, a refusal, or the active
    /// chain disappearing from the pushed set) — ask again.
    ReAsk,
}

/// Sans-IO state machine for the ask/wait/retry cycle. Driven by real time
/// and real wire calls from `spawn_chain_asker`'s async loop; kept separate
/// from both so the decision logic (when to warn, when to retry, when to
/// reset backoff) can be unit tested without a tokio runtime or timers.
struct ChainAskCycle {
    wait_bound: Duration,
    backoff: Backoff,
    pending: Option<PendingAsk>,
    active: Option<String>,
    next_retry_at: Option<Instant>,
}

impl ChainAskCycle {
    fn new(wait_bound: Duration, retry_floor: Duration, retry_cap: Duration) -> Self {
        Self {
            wait_bound,
            backoff: Backoff::new(retry_floor, retry_cap),
            pending: None,
            active: None,
            next_retry_at: None,
        }
    }

    fn schedule_retry(&mut self, now: Instant) {
        self.next_retry_at = Some(now + self.backoff.next());
    }

    /// Record the outcome of an ask attempt just performed by the caller.
    fn on_outcome(&mut self, outcome: &ChainAskOutcome, now: Instant) {
        match outcome {
            ChainAskOutcome::Granted { chain_id, .. } => {
                self.pending = Some(PendingAsk {
                    chain_id: chain_id.clone(),
                    requested_at: now,
                    warned: false,
                });
                self.next_retry_at = None;
            }
            // A policy rejection is a configuration fact, not a transient
            // failure — retrying it immediately gets the identical refusal.
            // It still eventually retries (the operator may loosen the
            // policy), through the SAME backoff as any other refusal —
            // never a tight fast-path loop.
            ChainAskOutcome::PolicyRejected { .. } | ChainAskOutcome::Refused { .. } => {
                self.pending = None;
                self.schedule_retry(now);
            }
        }
    }

    /// Called whenever the pushed chain-id set changes (see
    /// `spawn_frame_watcher`'s `WsServerFrame::Config` arm).
    fn observe_pushed(&mut self, pushed: &[String], now: Instant) {
        let arrived = self
            .pending
            .as_ref()
            .is_some_and(|p| pushed.iter().any(|id| id == &p.chain_id));
        if arrived {
            if let Some(p) = self.pending.take() {
                self.active = Some(p.chain_id);
            }
            self.next_retry_at = None;
            self.backoff.reset();
            return;
        }
        let disappeared = match &self.active {
            Some(active) => !pushed.iter().any(|id| id == active),
            None => false,
        };
        if disappeared {
            self.active = None;
            self.schedule_retry(now);
        }
    }

    /// Periodic tick — caller supplies `now`. Returns the actions to perform,
    /// if any. Each action fires exactly once per transition: the wait-bound
    /// WARN is guarded by [`PendingAsk::warned`], and `ReAsk` fires only when
    /// `now` first crosses `next_retry_at` (which is cleared immediately).
    fn tick(&mut self, now: Instant) -> Vec<ChainAskAction> {
        let mut actions = Vec::new();

        let mut warn_event: Option<(String, Duration)> = None;
        if let Some(p) = self.pending.as_mut() {
            if !p.warned {
                let waited = now.saturating_duration_since(p.requested_at);
                if waited >= self.wait_bound {
                    p.warned = true;
                    warn_event = Some((p.chain_id.clone(), waited));
                }
            }
        }
        if let Some((chain_id, waited)) = warn_event {
            actions.push(ChainAskAction::WarnNeverPushed { chain_id, waited });
            self.schedule_retry(now);
        }

        if let Some(t) = self.next_retry_at {
            if now >= t {
                self.next_retry_at = None;
                actions.push(ChainAskAction::ReAsk);
            }
        }

        actions
    }
}

/// [`SentinelConfig`](crate::SentinelConfig) knobs for the K1 chain-ask
/// cycle, resolved once in `run()` and handed to [`spawn_chain_asker`].
#[derive(Debug, Clone)]
pub struct ChainAskConfig {
    pub enabled: bool,
    pub hops: u8,
    /// K4: `SentinelConfig::chain_ask_exit_region` — the box's configured
    /// egress country, carried verbatim into `anchor.exit_region` on every
    /// ask. `None` asks for no region (full-auto).
    pub exit_region: Option<String>,
    pub wait_bound: Duration,
    pub retry_floor: Duration,
    pub retry_cap: Duration,
}

/// Granularity of the chain-ask loop's periodic re-check. Independent of
/// `wait_bound` / `retry_floor` — just how often the loop re-evaluates
/// whether either has elapsed. [`ChainAskCycle`] itself guards against
/// firing an action more than once per transition regardless of this value.
const CHAIN_ASK_TICK_INTERVAL: Duration = Duration::from_secs(5);

/// Spawn the K1 background task: ask the monitor for a chain, wait (bounded)
/// for it to arrive through the ordinary push path, and re-ask (bounded
/// backoff) on timeout or on the active chain disappearing from the pushed
/// set.
///
/// `pushed_ids_rx` is fed by `spawn_frame_watcher`'s `WsServerFrame::Config`
/// arm — the SAME frame the materialise path already consumes — so arrival
/// is observed at the one place pushed chains actually land, rather than by
/// polling `client-config.json` on disk.
///
/// No-op when `ask_cfg.enabled` is `false`.
pub fn spawn_chain_asker(
    ask_cfg: ChainAskConfig,
    client_id: String,
    state_dir: PathBuf,
    mut wire_coords_rx: watch::Receiver<Option<crate::monitor_wire::WireCoords>>,
    mut pushed_ids_rx: watch::Receiver<Vec<String>>,
) {
    if !ask_cfg.enabled {
        tracing::info!("[sentinel] chain_ask: disabled by config");
        return;
    }
    tokio::spawn(async move {
        let mut cycle =
            ChainAskCycle::new(ask_cfg.wait_bound, ask_cfg.retry_floor, ask_cfg.retry_cap);
        let mut ticker = tokio::time::interval(CHAIN_ASK_TICK_INTERVAL);

        // Wait for the first wire coords before asking for anything — an ask
        // needs somewhere to dial.
        while wire_coords_rx.borrow().is_none() {
            if wire_coords_rx.changed().await.is_err() {
                tracing::info!(
                    "[sentinel] chain_ask: wire_coords channel closed before first coords — exiting"
                );
                return;
            }
        }

        issue_ask(&ask_cfg, &client_id, &state_dir, &wire_coords_rx, &mut cycle).await;

        loop {
            tokio::select! {
                changed = pushed_ids_rx.changed() => {
                    if changed.is_err() {
                        tracing::info!("[sentinel] chain_ask: pushed-ids channel closed — exiting");
                        return;
                    }
                    let ids = pushed_ids_rx.borrow().clone();
                    cycle.observe_pushed(&ids, Instant::now());
                }
                _ = ticker.tick() => {
                    for action in cycle.tick(Instant::now()) {
                        match action {
                            ChainAskAction::WarnNeverPushed { chain_id, waited } => {
                                tracing::warn!(
                                    chain_id = %chain_id,
                                    waited_secs = waited.as_secs(),
                                    "[sentinel] chain_ask: monitor built this chain but it was \
                                     never pushed within the wait bound"
                                );
                            }
                            ChainAskAction::ReAsk => {
                                issue_ask(&ask_cfg, &client_id, &state_dir, &wire_coords_rx, &mut cycle).await;
                            }
                        }
                    }
                }
            }
        }
    });
}

/// Perform one ask attempt: resolve the current wire coords + this monitor's
/// stored client_token, call [`ask_for_chain`], log the honest outcome, and
/// feed it into `cycle`. The bearer token is never logged.
async fn issue_ask(
    ask_cfg: &ChainAskConfig,
    client_id: &str,
    state_dir: &Path,
    wire_coords_rx: &watch::Receiver<Option<crate::monitor_wire::WireCoords>>,
    cycle: &mut ChainAskCycle,
) {
    let Some(wc) = wire_coords_rx.borrow().clone() else {
        tracing::warn!("[sentinel] chain_ask: no wire coords yet — skipping ask");
        return;
    };
    let wrc = WireRegisterCoords {
        monitor_addr: &wc.monitor_addr,
        monitor_pubkey_hex: &wc.monitor_pubkey_hex,
        mesh_id: &wc.mesh_id,
    };
    let Some(bearer) = load_client_token(state_dir, wrc.token_key()) else {
        tracing::warn!(
            "[sentinel] chain_ask: no client_token stored for this monitor yet — skipping ask"
        );
        return;
    };
    let spec = default_chain_ask_spec(ask_cfg.hops, ask_cfg.exit_region.clone());
    let now = Instant::now();
    match ask_for_chain(&wrc, &bearer, client_id, &spec).await {
        Ok(outcome) => {
            match &outcome {
                ChainAskOutcome::Granted { chain_id, hop_count, cached, .. } => {
                    tracing::info!(
                        chain_id = %chain_id,
                        hops = *hop_count,
                        cached = *cached,
                        "[sentinel] chain_ask: monitor granted a chain"
                    );
                }
                ChainAskOutcome::PolicyRejected { reason } => {
                    tracing::warn!(
                        reason = %reason,
                        "[sentinel] chain_ask: monitor refused the axis spec by policy \
                         — not retrying on the fast path"
                    );
                }
                ChainAskOutcome::Refused { status, body } => {
                    tracing::warn!(
                        status = *status,
                        body = %body,
                        "[sentinel] chain_ask: monitor refused the request"
                    );
                }
            }
            cycle.on_outcome(&outcome, now);
        }
        Err(e) => {
            tracing::warn!(error = %e, "[sentinel] chain_ask: request failed (transport)");
            cycle.on_outcome(
                &ChainAskOutcome::Refused {
                    status: 0,
                    body: e.to_string(),
                },
                now,
            );
        }
    }
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

    // ── K1: chain ask ─────────────────────────────────────────────────────

    #[test]
    fn chain_ask_spec_matches_the_monitor_default_shape() {
        // Mirrors `mirage2monitor::auto_chain::default_auto_axis_spec()`'s
        // JSON exactly (field order + enum tag strings) except `hops`.
        let spec = default_chain_ask_spec(2, None);
        let json = serde_json::to_value(&spec).unwrap();
        let expected = serde_json::json!({
            "transport": "tcp",
            "camouflage": "none",
            "hops": 2,
            "relay_mode": "onion_peel",
            "exit_mode": {"kind": "direct"},
            "aead_suite": "cha_cha20_poly1305",
            "byte_cipher": "none",
            "reply_mode": "onion",
            "handshake": "telescopic",
            "anchor": {
                "entry_region": null,
                "exit_region": null,
                "entry_node_id": null,
                "exit_node_id": null,
                "hop_nodes": []
            },
            "ttl_secs": null,
            "camo_params": null
        });
        assert_eq!(json, expected);
    }

    /// K4: a box configured with an egress country carries it into the ask
    /// as `anchor.exit_region`, and touches nothing else in the anchor.
    #[test]
    fn chain_ask_spec_carries_the_configured_exit_region() {
        let spec = default_chain_ask_spec(2, Some("nl".to_string()));
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["anchor"]["exit_region"], serde_json::json!("nl"));
        assert_eq!(json["anchor"]["entry_region"], serde_json::json!(null));
        assert_eq!(json["anchor"]["hop_nodes"], serde_json::json!([]));
    }

    #[test]
    fn a_403_is_classified_as_a_policy_rejection_not_a_transport_error() {
        let outcome =
            classify_chain_ask_response(403, b"policy reject: camo=ssh not allowed").unwrap();
        match outcome {
            ChainAskOutcome::PolicyRejected { reason } => {
                assert_eq!(reason, "camo=ssh not allowed");
            }
            other => panic!("expected PolicyRejected, got {other:?}"),
        }
    }

    #[test]
    fn a_policy_rejection_is_not_retried_on_the_fast_path() {
        let mut cycle = ChainAskCycle::new(
            Duration::from_secs(90),
            Duration::from_secs(30),
            Duration::from_secs(300),
        );
        let now = Instant::now();
        cycle.on_outcome(
            &ChainAskOutcome::PolicyRejected {
                reason: "nope".to_owned(),
            },
            now,
        );
        // Immediately after the rejection: no re-ask yet.
        assert!(cycle.tick(now).is_empty(), "must not retry on the fast path");
        assert!(cycle.tick(now + Duration::from_secs(29)).is_empty());
        // Only once the retry floor elapses.
        assert_eq!(
            cycle.tick(now + Duration::from_secs(30)),
            vec![ChainAskAction::ReAsk]
        );
    }

    #[test]
    fn backoff_grows_by_doubling_and_resets_to_the_floor() {
        let mut b = Backoff::new(Duration::from_secs(30), Duration::from_secs(300));
        assert_eq!(b.next(), Duration::from_secs(30));
        assert_eq!(b.next(), Duration::from_secs(60));
        assert_eq!(b.next(), Duration::from_secs(120));
        assert_eq!(b.next(), Duration::from_secs(240));
        assert_eq!(b.next(), Duration::from_secs(300)); // capped
        assert_eq!(b.next(), Duration::from_secs(300)); // stays capped
        b.reset();
        assert_eq!(b.next(), Duration::from_secs(30));
    }

    #[test]
    fn wait_bound_timeout_warns_exactly_once_not_per_poll() {
        let mut cycle = ChainAskCycle::new(
            Duration::from_secs(90),
            Duration::from_secs(30),
            Duration::from_secs(300),
        );
        let now = Instant::now();
        cycle.on_outcome(
            &ChainAskOutcome::Granted {
                chain_id: "auto-deadbeef".to_owned(),
                expires_at_ms: 0,
                cached: false,
                hop_count: 2,
            },
            now,
        );

        // Before the wait bound: nothing.
        assert!(cycle.tick(now + Duration::from_secs(10)).is_empty());
        assert!(cycle.tick(now + Duration::from_secs(89)).is_empty());

        // Crossing the wait bound: exactly one WarnNeverPushed.
        let first = cycle.tick(now + Duration::from_secs(90));
        assert_eq!(first.len(), 1);
        assert!(matches!(
            &first[0],
            ChainAskAction::WarnNeverPushed { chain_id, .. } if chain_id == "auto-deadbeef"
        ));

        // Repeated polling before the retry floor elapses must not re-warn.
        for extra in 1..10 {
            let actions = cycle.tick(now + Duration::from_secs(90 + extra));
            assert!(
                actions.is_empty(),
                "must not warn again on poll {extra}: {actions:?}"
            );
        }
    }

    // ── K2 v2: sentinel registration ─────────────────────────────────────

    /// Byte-exact test vector for [`sentinel_register_signing_bytes`] — pins
    /// the documented layout the same way
    /// `mirage_core::identity::tests::signing_bytes_none_case_matches_the_
    /// documented_encoding` pins `DeclaredIdentity::signing_bytes`'s own.
    #[test]
    fn sentinel_register_signing_bytes_matches_the_documented_encoding() {
        let self_id = uuid::Uuid::nil();
        let mesh_id = "MESH1";
        let monitor_self_id = "monitor-self-id-1";
        let nonce_hex = "bb".repeat(16);
        let ts_ms: u64 = 1_700_000_000_000;

        let mut expected = Vec::new();
        expected.extend_from_slice(b"MIRAGE-SENTINEL-CLIENT-REGISTER-V1\0");
        expected.extend_from_slice(self_id.to_string().as_bytes());
        expected.push(0);
        expected.extend_from_slice(mesh_id.as_bytes());
        expected.push(0);
        expected.extend_from_slice(monitor_self_id.as_bytes());
        expected.push(0);
        expected.extend_from_slice(nonce_hex.as_bytes());
        expected.push(0);
        expected.extend_from_slice(ts_ms.to_string().as_bytes());

        let actual =
            sentinel_register_signing_bytes(self_id, mesh_id, monitor_self_id, &nonce_hex, ts_ms);
        assert_eq!(actual, expected, "must match the documented byte layout exactly");
    }

    #[test]
    fn sentinel_register_signing_bytes_differ_by_monitor_self_id() {
        let self_id = uuid::Uuid::new_v4();
        let a = sentinel_register_signing_bytes(self_id, "MESH1", "monitor-a", "cc", 1);
        let b = sentinel_register_signing_bytes(self_id, "MESH1", "monitor-b", "cc", 1);
        assert_ne!(a, b, "binding to a different monitor must change the preimage");
    }

    #[test]
    fn classify_sentinel_register_response_parses_a_success() {
        let body = serde_json::json!({"client_id": "c0ffee00-0000-4000-8000-000000000001", "client_token": "tok-a"})
            .to_string();
        match classify_sentinel_register_response(200, body.as_bytes()).unwrap() {
            SentinelRegisterOutcome::Registered { client_id, client_token } => {
                assert_eq!(client_id, "c0ffee00-0000-4000-8000-000000000001");
                assert_eq!(client_token, Some("tok-a".to_owned()));
            }
            other => panic!("expected Registered, got {other:?}"),
        }
    }

    #[test]
    fn classify_sentinel_register_response_names_a_refusal() {
        let body = serde_json::json!({"error": "revoked", "detail": "identity is revoked"}).to_string();
        match classify_sentinel_register_response(401, body.as_bytes()).unwrap() {
            SentinelRegisterOutcome::Refused { reason, status } => {
                assert_eq!(reason, "revoked");
                assert_eq!(status, 401);
            }
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    #[test]
    fn classify_sentinel_register_response_falls_back_to_raw_body_when_unshaped() {
        match classify_sentinel_register_response(503, b"service unavailable").unwrap() {
            SentinelRegisterOutcome::Refused { reason, status } => {
                assert_eq!(reason, "service unavailable");
                assert_eq!(status, 503);
            }
            other => panic!("expected Refused, got {other:?}"),
        }
    }
}
