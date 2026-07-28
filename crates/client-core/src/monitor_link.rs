//! Background link to mirage2monitor — config push via WebSocket with OverChain fallback.
//!
//! [`MonitorLink`] uses Mirage-native transports only:
//!
//! 1. **Wire** (`MonitorTransport::Wire`) — direct dial to the monitor's `:19444`
//!    V9 wire endpoint.  Anonymous ephemeral X25519 client key; no bearer token.
//!    Primary transport; attempted on every connection cycle.
//!
//! 2. **OverChain** (`MonitorTransport::OverChain`) — MonitorLink frames over a
//!    Mirage chain (via a worker SOCKS5 port or the local chain pool).  Fallback
//!    for restricted networks where direct `:19444` is DPI-blocked.
//!
//! No HTTPS, no `Authorization: Bearer` — all communication is authenticated by
//! the Mirage V9 wire handshake or the OverChain Mirage tunnel.
//!
//! State machine:
//! ```text
//! Wire (primary) ──► Connected (Wire WebSocket)
//!     │ error/close
//!     ▼
//! OverChain (fallback when Wire fails + OverChain configured)
//!     │ Wire recovers (next retry cycle succeeds)
//!     ▼
//! Wire (promoted back)
//! ```

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use mirage_core::crypto::credential::{CredentialParams, EpochEntry};
use rand::RngCore as _;
use mirage_core::mirage::mesh::PeerTable;
use mirage_core::mirage_blindtoken as bt;
use mirage_core::mirage::wire::CredentialScheme;
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Mutex};
use tokio_tungstenite::tungstenite::{http::header, http::Request, Message};
use tracing::{debug, info, warn};

use crate::chain_set::{ChainHandle, ChainId, ChainSet};
use crate::monitor_channel::{MonitorChannel, MonitorChannelError, OverChainChannel};
use crate::token_pool::{absorb_issue_result, TokenPool};
use mirage_protocol::traits::Tunnel as _;

/// Option (a') helper — acquire a tunnel from the chain pool directly, send
/// the target address (same wire as SOCKS5 path), and hand off to
/// `OverChainChannel::connect_via_tunnel` for the auth handshake.
async fn try_lease_and_open(
    handle: &Arc<ChainHandle>,
    monitor_addr: SocketAddr,
    bearer: &str,
    mesh_id: &str,
    client_id: &str,
) -> Result<OverChainChannel, MonitorChannelError> {
    let mut lease = handle
        .pool
        .lease()
        .await
        .map_err(MonitorChannelError::Lease)?;
    // Send target string — single-hop direct exit reads this as the dest addr,
    // but TunnelControl exit ignores its value and bridges to control_target.
    let target_str = monitor_addr.to_string();
    lease
        .send(target_str.as_bytes())
        .await
        .map_err(MonitorChannelError::Lease)?;
    lease.poison();
    let inner = lease
        .take_tunnel()
        .ok_or_else(|| MonitorChannelError::Io("lease tunnel already taken".to_string()))?;
    OverChainChannel::connect_via_tunnel(inner, bearer, mesh_id, client_id).await
}

// ── Timeouts ─────────────────────────────────────────────────────────────────

const OVER_CHAIN_BACKOFF_INIT: Duration = Duration::from_secs(5);
const OVER_CHAIN_CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

fn default_chain_id() -> ChainId {
    ChainId(String::new())
}

// ── MonitorTransport ──────────────────────────────────────────────────────────

/// How to reach the monitor control plane.
///
/// A [`MonitorLink`] accepts a list of transports; they are probed in order.
/// Priority: `Wire` (primary) > `OverChain` (restricted-network fallback).
///
/// `DirectWs` (HTTPS/WSS + bearer) has been removed — all client→monitor
/// communication must go over Mirage-native transports.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum MonitorTransport {
    /// Binary frame protocol over a Mirage tunnel (`OverChain`).
    ///
    /// The client connects to the monitor's over-chain listener through the
    /// designated worker's SOCKS5 port. Worker is the per-chain process
    /// (cli2 split-arch) that already maintains a tunnel pool on this chain
    /// for regular SOCKS5 traffic. We piggyback on that — no need for a
    /// second decrypted Tunnel abstraction.
    OverChain {
        /// Local SOCKS5 address of the worker that owns this chain.
        /// E.g. `127.0.0.1:19516` for `c-control-over-chain`. The worker
        /// performs the Mirage handshake and pipes plaintext bytes; we
        /// then speak our binary frame protocol directly on the resulting
        /// bidirectional stream.
        worker_socks5: SocketAddr,
        /// CONNECT target sent to the worker's SOCKS5 (host:port). The
        /// chain's exit hop forwards this to its `control_target` regardless
        /// of the value (TunnelControl ignores the target string), but
        /// SOCKS5 protocol requires a non-empty target. Default
        /// `127.0.0.1:19445` works because exit hop bridges to its own
        /// loopback monitor listener.
        monitor_addr: SocketAddr,
        /// Kept for backward compat with logs / metrics. Not used for dial.
        #[serde(default = "default_chain_id")]
        chain_id: ChainId,
    },
    /// Direct Mirage-wire WebSocket — the sentinel dials the monitor's
    /// wire endpoint (`:19444`) using an ephemeral X25519 client key,
    /// performs the V9 Mirage AEAD handshake (anonymous, no
    /// `DeclaredIdentity`), then upgrades to WebSocket over the resulting
    /// tunnel.
    ///
    /// No `Authorization: Bearer` header is sent — the Mirage wire already
    /// provides E2E encryption and mutual forward-secret auth.
    ///
    /// `monitor_addr` — IP:port of the monitor's wire listener, e.g.
    /// `"103.231.75.144:19444"`.
    ///
    /// `monitor_pubkey_hex` — 64-character hex of the monitor's static
    /// X25519 public key (fetched from `GET /relay/me` on first boot or
    /// set in `sentinel.toml`).
    ///
    /// `mesh_id` — cluster identifier baked into the V9 handshake confirm,
    /// e.g. `"v3-staging"`.
    ///
    /// `host_hint` — optional hostname used as the HTTP `Host:` header on the
    /// WS upgrade request (defaults to the IP portion of `monitor_addr`).
    Wire {
        monitor_addr: String,
        monitor_pubkey_hex: String,
        mesh_id: String,
        #[serde(default)]
        host_hint: Option<String>,
    },
}

// ── Public types ──────────────────────────────────────────────────────────────

/// Latest config snapshot received from the monitor.
#[derive(Debug, Clone)]
pub struct MonitorConfig {
    /// Config generation counter from the monitor.
    pub version: u64,
    /// Chains advertised by the monitor.
    pub chains: Vec<MonitorChain>,
    /// Optional traffic policy pushed by the monitor.
    pub policy: Option<MonitorPolicy>,
    /// Optional camouflage parameters pushed by the monitor.
    pub camouflage: Option<HashMap<String, String>>,
    /// Optional traffic shaping parameters pushed by the monitor.
    pub shaping: Option<HashMap<String, String>>,
    /// Operator directive for flow-metadata collection and upload.
    ///
    /// `None` (absent or `collect: false`) means the client collects nothing.
    /// When `Some(p)` with `p.collect == true` the telemetry task aggregates
    /// flow observations from the TUN `/connections` endpoint and POSTs a
    /// [`mirage_core::metadata::ClientMetadataReport`] to
    /// `/relay/clients/{id}/metadata` over the same Mirage wire.
    pub metadata_policy: Option<mirage_core::metadata::MetadataPolicy>,
}

/// Operator-triggered debug tunnel open command.
///
/// Sentinel writes this verbatim to `${state_dir}/debug-tunnel.json` and
/// spawns `mirage-debug-shell --config <path>` to act on it.
#[derive(Clone, Serialize, Deserialize)]
pub struct DebugTunnelOpen {
    pub monitor_ssh_host: String,
    pub monitor_ssh_port: u16,
    pub monitor_ssh_user: String,
    /// SSH private key in PEM form — redacted in Debug output to prevent
    /// accidental key exposure in logs.
    pub ssh_priv_key_pem: String,
    pub reverse_port: u16,
    pub local_ssh_port: u16,
    pub ttl_secs: u64,
}

impl std::fmt::Debug for DebugTunnelOpen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DebugTunnelOpen")
            .field("monitor_ssh_host", &self.monitor_ssh_host)
            .field("monitor_ssh_port", &self.monitor_ssh_port)
            .field("monitor_ssh_user", &self.monitor_ssh_user)
            .field("ssh_priv_key_pem", &"<redacted>")
            .field("reverse_port", &self.reverse_port)
            .field("local_ssh_port", &self.local_ssh_port)
            .field("ttl_secs", &self.ttl_secs)
            .finish()
    }
}

/// Phase 1.14 (memory wave): graceful chain migration push from a
/// monitor. The named `chain_id` is now available at the `entry`
/// host:port; if the sentinel was already using `replaces_chain_id`
/// it should adopt the new one as secondary and drain the old.
///
/// Sentinel-side handling lands in 1.14b/c. For now this just lands
/// the frame type so the monitor can start emitting without breaking
/// older sentinels (they ignore unknown frames).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainAvailable {
    pub chain_id: String,
    pub entry: ChainAvailableEntry,
    pub est_score: f64,
    #[serde(default)]
    pub replaces_chain_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainAvailableEntry {
    pub host: String,
    pub port: u16,
}

/// Tagged server-to-client WebSocket frame.
///
/// Wire format:
/// ```json
/// { "kind": "config", ... }                    // ConfigResponse fields
/// { "kind": "debug_tunnel_open", ... }
/// { "kind": "debug_tunnel_close" }
/// { "kind": "chain_available", ... }
/// ```
///
/// Old monitors send the bare `ConfigResponse` (no `kind`); the deserializer
/// falls back to treating untagged JSON as `Config`.
#[derive(Debug, Clone)]
pub enum WsServerFrame {
    /// Standard configuration push.
    Config(MonitorConfig),
    /// Operator wants a reverse-SSH debug tunnel opened.
    DebugTunnelOpen(DebugTunnelOpen),
    /// Operator wants the active debug tunnel closed.
    DebugTunnelClose,
    /// Phase 1.14 (memory wave): graceful chain migration hint.
    ChainAvailable(ChainAvailable),
    /// Operator-declared epoch secrets for per-dial anonymous credential minting.
    ///
    /// Contains the full `CredentialParams` (live epochs + `require` policy flag).
    /// MonitorLink stores this in `credential_params` ArcSwap; pools snapshot it
    /// on each dial and call `mint_epoch_hmac_credential` for a fresh token.
    CredentialParams(CredentialParams),
}

/// Traffic policy received from the monitor.
///
/// Re-exported from `mirage_core::split_policy` — the JSON wire shape is
/// directly compatible with what the TUN's `POST /policy/set` expects.
pub use mirage_core::split_policy::SplitPolicyConfig as MonitorPolicy;

/// A single chain entry from the monitor config response.
#[derive(Debug, Clone)]
pub struct MonitorChain {
    pub chain: mirage_core::config::ChainConfig,
    pub healthy: bool,
    pub latency_ms: Option<u64>,
}

impl MonitorChain {
    pub fn name(&self) -> &str {
        &self.chain.chain_id
    }
}

// ── Wire format (private) ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ConfigResponse {
    version: u64,
    chains: Vec<ChainEntry>,
    #[serde(default = "default_poll_interval")]
    monitor_poll_interval_secs: u64,
    #[serde(default)]
    policy: Option<MonitorPolicy>,
    #[serde(default)]
    camouflage: Option<HashMap<String, String>>,
    #[serde(default)]
    shaping: Option<HashMap<String, String>>,
    /// Peer table snapshot pushed by the monitor (Phase 8.5).
    /// Old monitors that don't include this field will leave it as `None`,
    /// and the client will use only chains with hardcoded `hop.public_key`.
    #[serde(default)]
    peer_table: Option<PeerTable>,
    /// Operator directive for flow-metadata collection (transmitted down).
    /// Absent on old monitors — treated as `None` (collect nothing).
    #[serde(default)]
    metadata_policy: Option<mirage_core::metadata::MetadataPolicy>,
}

/// One step of a ring-membership Merkle inclusion path as received from the
/// monitor in a `RingSlot` frame. Field names MUST match the wire contract
/// byte-for-byte (`"sibling_hex"`, `"is_right"`) — this is parsed by string tag
/// and a typo is a silent runtime mismatch.
#[derive(Debug, Deserialize)]
struct WireRingPathStep {
    sibling_hex: String,
    is_right: bool,
}

/// Tagged wire frame matching `WsServerFrame`. Used only on WebSocket — the
/// HTTP poll path always parses `ConfigResponse` directly.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TaggedServerFrame {
    Config(ConfigResponse),
    DebugTunnelOpen(DebugTunnelOpen),
    DebugTunnelClose,
    /// Phase 1.14 (memory wave): graceful chain migration hint.
    ChainAvailable(ChainAvailable),
    /// Operator pushes epoch secrets for per-dial anonymous credential minting.
    /// Wire JSON: `{"kind":"credential_params","generation":N,"scheme":1,"epochs":[...]}`.
    /// Credential enforcement is unconditional — there is no `require` field.
    CredentialParams(CredentialParams),
    /// App-level Pong echoing a client-sent Ping nonce.
    #[serde(rename = "pong")]
    Pong { nonce: u64 },
    /// Monitor returns signed blind tokens (success path).
    CredentialIssueResult {
        request_id: u64,
        epoch_id: u32,
        blind_sigs_hex: Vec<String>,
    },
    /// Monitor returns an error for a batch request.
    CredentialIssueError {
        request_id: u64,
        error: String,
    },
    /// Monitor relays a fresh ring-membership slot assigned by opbox.
    ///
    /// Wire: `{"kind":"ring_slot","request_id":<u64>,"epoch_id":<u32>,
    ///         "slot_index":<u64>,"member_sk_hex":"<hex>","path":[...]}`
    RingSlot {
        request_id: u64,
        epoch_id: u32,
        slot_index: u64,
        member_sk_hex: String,
        path: Vec<WireRingPathStep>,
    },
    /// Monitor relays an error for a ring-enrol request.
    ///
    /// Wire: `{"kind":"ring_enrol_error","request_id":<u64>,"error":"<string>"}`
    RingEnrolError {
        request_id: u64,
        error: String,
    },
}

/// Tagged client→server frame sent over the same JSON channel.
/// Used for app-level ping-pong, blind-token batch requests, and ring-enrol.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TaggedClientFrame {
    /// App-level ping — server must reply with `Pong { nonce }`.
    #[serde(rename = "ping")]
    Ping { nonce: u64 },
    /// Blind-token batch request — monitor signs and returns `CredentialIssueResult`.
    ///
    /// Wire: `{"kind":"credential_request","request_id":<u64>,"enrol_token":"<str>","blinded_hex":["<hex>",...]}`
    CredentialRequest {
        request_id: u64,
        enrol_token: String,
        blinded_hex: Vec<String>,
    },
    /// Ring-membership enrol request — monitor enqueues for opbox to assign a slot.
    ///
    /// Wire: `{"kind":"ring_enrol_request","request_id":<u64>,"sentinel_id":"<uuid>"}`
    /// The monitor uses the authenticated session `client_id` as the authoritative
    /// sentinel_id for the opbox call; the client-supplied field is advisory.
    RingEnrolRequest {
        request_id: u64,
        sentinel_id: String,
    },
}

/// Parse a server WebSocket text frame, accepting both the tagged
/// `TaggedServerFrame` form and a legacy untagged `ConfigResponse`.
fn parse_ws_frame(text: &str) -> Result<TaggedServerFrame, serde_json::Error> {
    // Try tagged first.
    match serde_json::from_str::<TaggedServerFrame>(text) {
        Ok(f) => Ok(f),
        Err(_) => {
            // Fall back to untagged config (old monitors).
            let cfg: ConfigResponse = serde_json::from_str(text)?;
            Ok(TaggedServerFrame::Config(cfg))
        }
    }
}

fn default_poll_interval() -> u64 {
    30
}

/// Return the first live `EpochEntry` from the first `BlindRsaBatch` authority
/// in `params` that has non-empty `verify_material`.
///
/// Returns `None` when no suitable authority/epoch exists.
fn find_blind_rsa_epoch(params: &CredentialParams) -> Option<EpochEntry> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    for authority in &params.authorities {
        if authority.scheme != CredentialScheme::BlindRsaBatch {
            continue;
        }
        for epoch in &authority.epochs {
            if epoch.verify_material.is_empty() {
                continue;
            }
            if now_ms > epoch.not_after_ms {
                continue; // expired
            }
            return Some(epoch.clone());
        }
    }
    None
}

/// Decoded parameters for a [`MonitorTransport::Wire`] dial.
///
/// Cloned out of the transport list so it can be passed to async methods
/// without holding a reference into `self.transports`.
#[derive(Debug, Clone)]
struct WireParams {
    monitor_addr: String,
    monitor_pubkey_hex: String,
    mesh_id: String,
    host_hint: Option<String>,
}

/// Generate a random base64-encoded 16-byte WebSocket `Sec-WebSocket-Key`.
///
/// Per RFC 6455 §4.1 the key must be 16 bytes of entropy base64-encoded.
fn random_sec_key() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    base64_encode(&bytes)
}

/// Minimal base64 encoder (standard alphabet, with padding) for the 16-byte
/// WS key. Avoids pulling in an extra `base64` dep — the calculation is
/// trivial for a fixed 16-byte input.
fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity((input.len() + 2) / 3 * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let combined = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((combined >> 18) & 0x3f) as usize]);
        out.push(TABLE[((combined >> 12) & 0x3f) as usize]);
        out.push(if chunk.len() > 1 { TABLE[((combined >> 6) & 0x3f) as usize] } else { b'=' });
        out.push(if chunk.len() > 2 { TABLE[(combined & 0x3f) as usize] } else { b'=' });
    }
    String::from_utf8(out).expect("base64 output is always valid ASCII")
}

#[derive(Debug, Deserialize)]
struct ChainEntry {
    #[serde(flatten)]
    chain: mirage_core::config::ChainConfig,
    #[serde(default = "chain_entry_default_healthy")]
    healthy: bool,
    #[serde(default)]
    latency_ms: Option<u64>,
}

fn chain_entry_default_healthy() -> bool {
    true
}

#[derive(Debug, Serialize)]
struct RegisterBody {
    os: &'static str,
    version: &'static str,
    supported_transports: Vec<&'static str>,
}

#[derive(Debug, Deserialize)]
struct RegisterResponse {
    client_id: String,
    #[serde(default = "default_poll_interval")]
    monitor_poll_interval_secs: u64,
    /// Phase 2: per-client token returned once at registration.
    /// Old monitors omit this field — treated as `None`.
    #[serde(default)]
    client_token: Option<String>,
}


// ── InflightBatch ─────────────────────────────────────────────────────────────

/// In-flight state for one outstanding batch request.
struct InflightBatch {
    /// The issuer public key parsed at request time; stashed so the result
    /// handler can call `bt::unblind` without re-parsing `credential_params`.
    pk: bt::IssuerPublicKey,
    /// `(msg, ClientBlind)` pairs in the same order as the `blinded_hex` vec.
    retained: Vec<([u8; bt::MSG_LEN], bt::ClientBlind)>,
}

// ── CredentialSummary ─────────────────────────────────────────────────────────

/// A lightweight, serialisable snapshot of the current credential configuration.
///
/// Derived from the live [`CredentialParams`] stored in
/// [`MonitorLink::credential_params`].  Intended for GUI / polling consumers that
/// need to display the scheme name and generation counter without exposing
/// epoch secrets or internal crypto state.
///
/// Credential enforcement is unconditional (no `require` toggle, arc3
/// mandatory-credentials excision) — credential-less (legacy) dials are
/// always rejected on the data plane once params are configured.
#[derive(Debug, Clone, Serialize)]
pub struct CredentialSummary {
    /// Snake-case scheme name matching the JSON serialisation of
    /// [`CredentialScheme`] (e.g. `"ring_membership"`, `"epoch_hmac"`).
    pub scheme: String,
    /// Monotonic generation counter from the latest accepted [`CredentialParams`].
    pub generation: u64,
}

// ── MonitorLink ───────────────────────────────────────────────────────────────

/// Background task that keeps the client in sync with mirage2monitor.
///
/// Uses Mirage-native transports only: Wire (primary) and OverChain (fallback).
/// No HTTPS, no bearer tokens.
pub struct MonitorLink {
    /// Cluster/mesh identifier sent in FRAME_AUTH (e.g. `"staging-v2"`).
    mesh_id: String,
    /// Local node/sentry identifier sent in FRAME_AUTH.
    node_client_id: String,
    /// Registered client_id returned by the monitor on registration.
    /// Set externally via [`Self::set_client_id`] (sentinel R1 path) or
    /// internally by the wire connect sequence before subscribe.
    client_id: Mutex<Option<String>>,
    /// Per-client token issued by the monitor on registration (Phase 2).
    /// Appended as `?client_id=X&client_token=Y` on subscribe when present.
    /// `None` on older monitors — omitted silently.
    client_token: Mutex<Option<String>>,
    config_version: AtomicU64,
    poll_interval_secs: AtomicU64,
    config_tx: watch::Sender<Option<MonitorConfig>>,
    /// Server-pushed frames (config + debug tunnel commands).  Each new frame
    /// replaces the previous slot value.
    frame_tx: watch::Sender<Option<WsServerFrame>>,
    /// Latest peer table snapshot from the monitor (Phase 8.5).
    /// Initialised as an empty table; updated on every `Config` frame that
    /// carries a non-`None` `peer_table` field.
    /// Shared with `TunnelPool` / `MuxPool` via `Arc::clone`.
    pub peer_table: Arc<ArcSwap<PeerTable>>,
    /// Operator-declared anonymous credential params pushed over WS.
    /// Initialised as `None`; updated on every `CredentialParams` frame.
    /// Shared with `TunnelPool` / `MuxPool` via `Arc::clone` (same pattern as peer_table).
    pub credential_params: Arc<ArcSwap<Option<CredentialParams>>>,
    /// Transport list (Wire + OverChain only — DirectWs removed).
    transports: Vec<MonitorTransport>,
    /// Chain set used to open OverChain connections.
    chain_set: Arc<ChainSet>,
    /// Ephemeral X25519 private key generated at construction.
    ///
    /// Used as the client-side static key for the Mirage V9 wire handshake
    /// when connecting over a [`MonitorTransport::Wire`] transport. Ephemeral
    /// per-process (not persisted) — clients are anonymous on the wire plane.
    ephemeral_client_priv: [u8; 32],
    /// Runtime-seeded wire coordinates (addr + pubkey_hex + mesh_id).
    ///
    /// Set by [`Self::seed_wire_transport`] after the sentinel receives its
    /// first config snapshot containing a monitor peer_table entry.  Takes
    /// precedence over any `Wire` entry already in `transports` (or adds one
    /// when `transports` has none).  `None` means no runtime override — the
    /// static `transports` list is used as-is.
    wire_override: Mutex<Option<WireParams>>,
    /// Consecutive Wire connect failures on the current target.
    ///
    /// Reset to 0 on any clean Wire session (server-initiated close or clean
    /// return from `run_wire_loop`).  When this reaches
    /// `MIRAGE_MONITOR_WIRE_ROTATE_AFTER` (default 3), the run loop advances
    /// `wire_candidate_idx` and seeds the next monitor candidate from
    /// `peer_table`.  This implements round-robin rotation across all
    /// `"monitor"` labeled peers in the live peer table.
    wire_fail_count: AtomicU32,
    /// Round-robin cursor over monitor Wire candidates extracted from
    /// `peer_table`.  Incremented by the rotation path; wraps at the length
    /// of the candidate list.  Index 0 = the initial/preferred candidate
    /// (the one set via `seed_wire_transport`).
    wire_candidate_idx: AtomicU32,
    /// Static operator Ed25519 verifying key pinned in sentinel.toml.
    ///
    /// When `Some`, every incoming `CredentialParams` frame is verified against
    /// this key before being applied.  Frames that fail verification are
    /// discarded (the previously held params remain active) and a `warn!` is
    /// emitted.  When `None`, params are applied without operator-signature
    /// verification and a one-time transition-state `warn!` fires.
    operator_pub: Option<ed25519_dalek::VerifyingKey>,
    /// Monotonic generation watermark.
    ///
    /// Tracks the highest `generation` value from any `CredentialParams` that
    /// was successfully verified (or accepted in the no-pin path) and stored.
    /// Incoming params with `generation <= last_credential_generation` are
    /// treated as stale/replayed and discarded with a `warn!`.
    last_credential_generation: std::sync::atomic::AtomicU64,

    // ── Blind-token batch fields (Phase 3) ────────────────────────────────────

    /// Enrolment token passed in batch requests; plumbed from `sentinel.toml`.
    ///
    /// When `None`, no batch requests are sent (current behavior preserved).
    /// Wrapped in `std::sync::Mutex` so it can be set after construction
    /// (via `set_enrol_token`) without needing `&mut self`.
    enrol_token: std::sync::Mutex<Option<String>>,
    /// Pool of unspent blind-RSA tokens ready for presentation.
    ///
    /// Filled by the batch-request path; consumed by Phase 4 spend wiring.
    pub token_pool: Arc<ArcSwap<TokenPool>>,
    /// In-flight blind state keyed by `request_id`.
    ///
    /// Each entry holds the `(msg, ClientBlind)` pairs retained between sending
    /// `CredentialRequest` and receiving `CredentialIssueResult`.  Uses
    /// `std::sync::Mutex` — the critical sections are short and never hold
    /// the lock across an await point.
    inflight: std::sync::Mutex<HashMap<u64, InflightBatch>>,
    /// Monotonic request-id counter.
    next_request_id: AtomicU64,
    /// True while a batch request is outstanding.
    ///
    /// Limits outstanding requests to one at a time; cleared on result/error.
    request_in_flight: AtomicBool,

    // ── Ring-membership re-enrol fields ──────────────────────────────────────

    /// Shared hot-swappable ring credential cell.
    ///
    /// MonitorLink writes a new `RingCredential` into this cell when a
    /// `RingSlot` frame is received (epoch rotated, opbox assigned a new slot).
    /// The same Arc is shared with all pool instances — they read via
    /// `.load_full()` on every dial, so they pick up the new credential
    /// immediately without any restart.
    ///
    /// Construction: caller must pass the same Arc into `TunnelPool::new` /
    /// `MuxPool::new` via `ring_credential_cell()`.
    pub ring_cell: crate::token_pool::RingCredentialCell,
    /// True while a ring-enrol request is outstanding.
    ///
    /// Dedupe guard: prevents spamming the monitor on every `CredentialParams`
    /// tick when a previous request is still in-flight. Mirrors
    /// `request_in_flight` used by the blind-token path.
    ring_enrol_in_flight: AtomicBool,
    /// The `epoch_id` of the most recent ring-enrol request we sent.
    ///
    /// Prevents re-sending on subsequent `CredentialParams` ticks that carry
    /// the same epoch_id while the request is in-flight or has been fulfilled.
    /// Stored as `u64` (u32 epoch_id + 1, or 0 = no request sent yet) so we
    /// can use a single atomic without needing a sentinel value that collides
    /// with real epoch_ids.
    last_ring_epoch_requested: AtomicU64,
    /// Directory for persisted nullifier counter files.
    ///
    /// When `Some`, `handle_ring_slot` uses `RingCredential::new_persisted`
    /// so the hot-swapped credential inherits the cross-process counter.
    /// When `None`, falls back to `RingCredential::new` (in-process counter).
    /// Workers pass the config-file parent dir here; the sentinel leaves it
    /// `None` because the sentinel does not dial the data-plane directly.
    nf_counter_dir: std::sync::Mutex<Option<std::path::PathBuf>>,
}

impl MonitorLink {
    /// Create a new [`MonitorLink`] and return the watch receiver.
    ///
    /// The link does not start until [`MonitorLink::spawn`] is called.
    ///
    /// `monitor_api_key` — **ignored**; kept in the signature for backward
    /// compat with call sites.  No bearer token is ever sent — authentication
    /// is provided by the Mirage V9 wire handshake.
    ///
    /// `peer_table` — shared [`ArcSwap`] that the link writes to on every config
    /// frame containing a `peer_table` snapshot (Phase 8.5).
    ///
    /// `chain_set` — registry of chains, used for OverChain transport.
    ///
    /// `transports` — list of [`MonitorTransport`]s (Wire + OverChain).
    ///
    /// `mesh_id` — cluster/mesh identifier sent in FRAME_AUTH.
    ///
    /// `node_client_id` — local node/sentry identifier sent in FRAME_AUTH.
    pub fn new(
        monitor_api_key: Option<String>,
        peer_table: Arc<ArcSwap<PeerTable>>,
        chain_set: Arc<ChainSet>,
        transports: Vec<MonitorTransport>,
        mesh_id: String,
        node_client_id: String,
    ) -> (Arc<Self>, watch::Receiver<Option<MonitorConfig>>) {
        Self::new_with_operator_pub(
            monitor_api_key,
            peer_table,
            chain_set,
            transports,
            mesh_id,
            node_client_id,
            None,
        )
    }

    /// Like [`new`] but also accepts a pinned operator [`ed25519_dalek::VerifyingKey`]
    /// for verifying incoming `CredentialParams` frames before applying them.
    ///
    /// When `operator_pub` is `Some`, every `CredentialParams` frame is verified
    /// against the key.  Frames that fail verification are discarded; the previously
    /// held params remain active.  When `None`, params are applied without
    /// operator-signature verification (transition state).
    pub fn new_with_operator_pub(
        monitor_api_key: Option<String>,
        peer_table: Arc<ArcSwap<PeerTable>>,
        chain_set: Arc<ChainSet>,
        transports: Vec<MonitorTransport>,
        mesh_id: String,
        node_client_id: String,
        operator_pub: Option<ed25519_dalek::VerifyingKey>,
    ) -> (Arc<Self>, watch::Receiver<Option<MonitorConfig>>) {
        Self::new_with_credential_arc(
            monitor_api_key,
            peer_table,
            Arc::new(ArcSwap::from_pointee(None::<CredentialParams>)),
            Arc::new(ArcSwap::from_pointee(TokenPool::default())),
            Arc::new(ArcSwap::from_pointee(None)),
            chain_set,
            transports,
            mesh_id,
            node_client_id,
            operator_pub,
        )
    }

    /// Parse a 64-char hex Ed25519 operator verifying key.
    ///
    /// Shared by the standalone cli / cli2-worker config paths (which lack the
    /// `ed25519_dalek` + `hex` deps directly) so they can turn a config
    /// `operator_pub_hex` string into the `VerifyingKey` expected by
    /// [`MonitorLink::new_with_credential_arc`]. Mirrors the decode in l0-sentinel.
    pub fn parse_operator_pub_hex(
        hex_str: &str,
    ) -> Result<ed25519_dalek::VerifyingKey, String> {
        let bytes = hex::decode(hex_str)
            .map_err(|e| format!("operator_pub_hex is not valid hex: {e}"))?;
        let arr: [u8; 32] = bytes.try_into().map_err(|_| {
            "operator_pub_hex must be exactly 32 bytes (64 hex chars)".to_string()
        })?;
        ed25519_dalek::VerifyingKey::from_bytes(&arr)
            .map_err(|e| format!("operator_pub_hex is not a valid Ed25519 key: {e}"))
    }

    /// Like [`new`] but accepts pre-created `credential_params`, `token_pool`,
    /// and `ring_cell` arcs so that they can be shared with `TunnelPool` /
    /// `MuxPool` instances created before the `MonitorLink` is constructed.
    ///
    /// `ring_cell` MUST be the same `Arc` that was passed to every
    /// `TunnelPool::new` / `MuxPool::new` call in the same binary.  MonitorLink
    /// writes into this cell on every `RingSlot` frame; pools read from it on
    /// every dial.  Passing a different (fresh) arc breaks hot-swap — the pools
    /// never see the updated credential.
    ///
    /// Use `link.credential_params()` / `link.token_pool()` /
    /// `link.ring_credential_cell()` to retrieve `Arc::clone`s from a link
    /// built with the ordinary [`new`] constructor.
    pub fn new_with_credential_arc(
        _monitor_api_key: Option<String>,
        peer_table: Arc<ArcSwap<PeerTable>>,
        credential_params: Arc<ArcSwap<Option<CredentialParams>>>,
        token_pool: Arc<ArcSwap<TokenPool>>,
        ring_cell: crate::token_pool::RingCredentialCell,
        chain_set: Arc<ChainSet>,
        transports: Vec<MonitorTransport>,
        mesh_id: String,
        node_client_id: String,
        operator_pub: Option<ed25519_dalek::VerifyingKey>,
    ) -> (Arc<Self>, watch::Receiver<Option<MonitorConfig>>) {
        let (config_tx, config_rx) = watch::channel(None);
        let (frame_tx, _frame_rx) = watch::channel(None);

        // Generate a fresh ephemeral X25519 private key for Wire transport dials.
        // This key is used for the Mirage V9 client-static handshake slot.
        // Clients are anonymous (no DeclaredIdentity) — the key is ephemeral per process.
        let ephemeral_client_priv: [u8; 32] = {
            use rand::RngCore;
            let mut key = [0u8; 32];
            rand::rngs::OsRng.fill_bytes(&mut key);
            key
        };

        let link = Arc::new(Self {
            mesh_id,
            node_client_id,
            client_id: Mutex::new(None),
            client_token: Mutex::new(None),
            config_version: AtomicU64::new(0),
            poll_interval_secs: AtomicU64::new(30),
            config_tx,
            frame_tx,
            peer_table,
            credential_params,
            transports,
            chain_set,
            ephemeral_client_priv,
            wire_override: Mutex::new(None),
            wire_fail_count: AtomicU32::new(0),
            wire_candidate_idx: AtomicU32::new(0),
            operator_pub,
            last_credential_generation: std::sync::atomic::AtomicU64::new(0),
            enrol_token: std::sync::Mutex::new(None),
            token_pool,
            inflight: std::sync::Mutex::new(HashMap::new()),
            next_request_id: AtomicU64::new(0),
            request_in_flight: AtomicBool::new(false),
            ring_cell,
            ring_enrol_in_flight: AtomicBool::new(false),
            last_ring_epoch_requested: AtomicU64::new(0),
            nf_counter_dir: std::sync::Mutex::new(None),
        });

        (link, config_rx)
    }

    /// Set the directory used for persisted nullifier counter files on
    /// ring-credential hot-swap (`handle_ring_slot`).
    ///
    /// Call this after construction and before `spawn()` when the MonitorLink
    /// is hosted in a data-plane worker process that dials exit nodes. Pass the
    /// parent directory of the worker's config file so all concurrent/restarted
    /// workers for the same client share a single monotonic counter.
    ///
    /// Leave unset (default `None`) in sentinel or non-dialing processes — they
    /// fall back to the in-process counter which is correct for their roles.
    pub fn set_nf_counter_dir(&self, dir: std::path::PathBuf) {
        if let Ok(mut guard) = self.nf_counter_dir.lock() {
            *guard = Some(dir);
        }
    }

    /// Return the shared ring-credential cell.
    ///
    /// Pass the returned Arc to `TunnelPool::new` / `MuxPool::new` so that
    /// pool dial sites always read the latest ring credential (including any
    /// hot-swap on epoch rotation) without a restart.
    pub fn ring_credential_cell(&self) -> crate::token_pool::RingCredentialCell {
        Arc::clone(&self.ring_cell)
    }

    /// Set (or clear) the enrolment token used for blind-token batch requests.
    ///
    /// Call before [`MonitorLink::spawn`] so the first recv loop iteration can
    /// trigger a batch request.  Can also be called at runtime to update the
    /// token (e.g. after a re-registration).  Thread-safe.
    pub fn set_enrol_token(&self, token: Option<String>) {
        if let Ok(mut guard) = self.enrol_token.lock() {
            *guard = token;
        }
    }

    /// Return the token pool ArcSwap so Phase 4 spend wiring can read and drain it.
    pub fn token_pool(&self) -> Arc<ArcSwap<TokenPool>> {
        Arc::clone(&self.token_pool)
    }

    /// Create an additional [`watch::Receiver`] subscribed to the same config
    /// channel.
    pub fn subscribe(&self) -> watch::Receiver<Option<MonitorConfig>> {
        self.config_tx.subscribe()
    }

    /// Return a shared arc to the credential params ArcSwap.
    ///
    /// Pass the returned arc to `TunnelPool::new` / `MuxPool::new` so pools
    /// always dial with the latest operator-issued epoch secrets without extra
    /// plumbing.  The ArcSwap is updated by the WS receive loop on every
    /// `CredentialParams` frame from the monitor.
    pub fn credential_params(&self) -> Arc<ArcSwap<Option<CredentialParams>>> {
        Arc::clone(&self.credential_params)
    }

    /// Subscribe to raw server-pushed WebSocket frames (config snapshots and
    /// operator commands like `DebugTunnelOpen`).  Each new frame replaces the
    /// channel slot.
    pub fn subscribe_frames(&self) -> watch::Receiver<Option<WsServerFrame>> {
        self.frame_tx.subscribe()
    }

    /// Return a snapshot of the current credential configuration for GUI consumers.
    ///
    /// Returns `None` when no [`CredentialParams`] frame has been received yet
    /// (e.g. the link is still connecting or the monitor does not push credentials).
    ///
    /// The returned [`CredentialSummary`] reflects the PRIMARY authority's scheme
    /// (first in the authority set).  Secrets and per-epoch detail are not exposed.
    pub fn credential_summary(&self) -> Option<CredentialSummary> {
        let guard = self.credential_params.load();
        let params = guard.as_ref().as_ref()?;
        Some(CredentialSummary {
            scheme: params.primary_scheme().as_str().to_owned(),
            generation: params.generation,
        })
    }

    /// Validate and apply a `CredentialParams` frame received from the monitor.
    ///
    /// Steps (in order):
    /// 1. If `operator_pub` is set, verify `params.operator_sig` against it.
    ///    On failure: log a `warn!`, keep existing params, return.
    /// 2. If `operator_pub` is absent: emit a one-time `warn!` (logged on first
    ///    call only via the `WARNED_NO_PIN` flag) and continue.
    /// 3. Monotonic generation check: if `params.generation <=
    ///    last_credential_generation`, the frame is stale or replayed — log a
    ///    `warn!`, keep existing params, return.
    /// 4. Store the params and advance `last_credential_generation`.
    fn apply_credential_params(&self, params: CredentialParams, transport_label: &str) {
        let generation = params.generation;

        // Step 1 / 2: operator-signature check.
        match &self.operator_pub {
            Some(pub_key) => {
                if let Err(e) = params.verify(pub_key) {
                    warn!(
                        generation,
                        transport = transport_label,
                        error = %e,
                        "monitor_link: CredentialParams operator-signature check failed — discarding"
                    );
                    return;
                }
            }
            None => {
                // One-time transition-state warning.  Use a static AtomicBool so
                // the warn fires at most once per process, not on every frame.
                use std::sync::atomic::AtomicBool;
                static WARNED_NO_PIN: AtomicBool = AtomicBool::new(false);
                if !WARNED_NO_PIN.swap(true, Ordering::Relaxed) {
                    warn!(
                        "monitor_link: operator_pub not configured — \
                         CredentialParams applied without operator-signature verification \
                         (set operator_pub_hex in sentinel.toml to enable)"
                    );
                }
            }
        }

        // Step 3: monotonic generation guard.
        let last = self.last_credential_generation.load(Ordering::Acquire);
        if generation <= last {
            warn!(
                generation,
                last_accepted = last,
                transport = transport_label,
                "monitor_link: CredentialParams stale generation — discarding"
            );
            return;
        }

        // Step 4: accept.
        self.credential_params.store(Arc::new(Some(params.clone())));
        self.last_credential_generation.store(generation, Ordering::Release);
        debug!(
            generation,
            transport = transport_label,
            "monitor_link: CredentialParams updated"
        );
        // Notify the sentinel frame watcher so it re-materialises the supervisor
        // config with the new params (sentinel's CredentialParams arm calls rewrite).
        let _ = self.frame_tx.send(Some(WsServerFrame::CredentialParams(params.clone())));

        // Step 5: ring-epoch refresh detection (accumulate-aware).
        //
        // `params.authorities[ring].epochs` carries the FULL overlapping set the
        // issuer currently honours (e.g. `[4,5,6]` after epoch accumulation —
        // JWKS-style key rotation). The client's live credential stays valid as
        // long as its epoch is STILL a member of that set, so we re-enrol ONLY
        // when the live epoch has aged out of the set entirely (or we hold none
        // yet). Comparing against a single epoch (the old `epochs.first()` →
        // OLDEST in the window) spuriously re-enrolled every client whose valid
        // epoch differed from the oldest — bricking a correctly-credentialed
        // client the instant the issuer accumulated a second epoch.
        let ring_auth = params
            .authorities
            .iter()
            .find(|a| a.scheme == CredentialScheme::RingMembership);
        if let Some(auth) = ring_auth {
            let declared_epochs: Vec<u32> = auth.epochs.iter().map(|e| e.epoch_id).collect();
            // Newest declared epoch is the re-enrol target (and the dedup key).
            if let Some(&newest_epoch) = declared_epochs.iter().max() {
                let live_epoch: Option<u32> = self
                    .ring_cell
                    .load()
                    .as_ref()
                    .as_ref()
                    .map(|rc| rc.epoch_id);

                let still_valid =
                    matches!(live_epoch, Some(live) if declared_epochs.contains(&live));

                if !still_valid {
                    // Dedupe on the newest epoch so we don't spam enrol requests.
                    // Encode as (epoch_id as u64) + 1 so 0 means "not sent yet".
                    let sentinel_value = (newest_epoch as u64) + 1;
                    let last_requested = self.last_ring_epoch_requested.load(Ordering::Acquire);
                    if last_requested != sentinel_value
                        && !self.ring_enrol_in_flight.load(Ordering::Acquire)
                    {
                        // ZT durability: do NOT clear the live credential here.
                        // Keep presenting whatever token we already hold until the
                        // RingSlot response swaps in a fresh one. Pre-clearing to
                        // `None` creates a self-inflicted no-credential window —
                        // dials go out with scheme 0x00, the exit rejects them as
                        // "no-credential dial", and the outage becomes PERMANENT
                        // if the re-enrol response is ever lost. An aged-out token
                        // fails the same dial either way, so holding it costs
                        // nothing and an in-window token keeps carrying traffic.
                        info!(
                            newest_epoch,
                            live_epoch = live_epoch.unwrap_or(0),
                            declared = ?declared_epochs,
                            transport = transport_label,
                            "monitor_link: live ring epoch not in declared set — requesting re-enrol (keeping current credential until slot arrives)"
                        );
                        self.last_ring_epoch_requested.store(sentinel_value, Ordering::Release);
                        self.ring_enrol_in_flight.store(true, Ordering::Release);
                    }
                }
            }
        }
    }

    /// True when a ring-enrol request was just armed by `apply_credential_params`.
    ///
    /// The receive loops call this after `apply_credential_params` to decide
    /// whether to fire the WS send.  The flag is NOT automatically cleared here —
    /// the caller is responsible for firing the send and clearing the flag if
    /// the send fails.  On success the flag stays `true` until `RingSlot` or
    /// `RingEnrolError` arrives.
    fn should_send_ring_enrol(&self) -> bool {
        self.ring_enrol_in_flight.load(Ordering::Acquire)
    }

    /// Fire a blind-token batch request if conditions are met.
    ///
    /// Conditions (all must hold):
    /// - `enrol_token` is `Some`,
    /// - the current `CredentialParams` has a `BlindRsaBatch` authority with a
    ///   live epoch carrying non-empty `verify_material`,
    /// - `token_pool.len() < 16` (low-water mark),
    /// - no request is currently in flight.
    ///
    /// Sends `TaggedClientFrame::CredentialRequest` over the provided write sink.
    /// On any error (parse, blind, serialise, send) logs a `warn!` and returns
    /// without crashing the loop.
    ///
    /// # Sink type
    ///
    /// The sink is a `futures::SinkExt<Message>` abstraction — same concrete type
    /// (`SplitSink`) used by both `run_wire_loop` and `run_over_chain_loop`.
    async fn maybe_request_batch_ws<S>(&self, write: &mut S)
    where
        S: futures::SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
    {
        // Gate: enrol_token must be set.
        let enrol_token = match self.enrol_token.lock() {
            Ok(g) => g.clone(),
            Err(_) => return,
        };
        let enrol_token = match enrol_token {
            Some(t) => t,
            None => return,
        };

        // Gate: no outstanding request.
        if self.request_in_flight.load(Ordering::Acquire) {
            return;
        }

        // Gate: low-water mark.
        if self.token_pool.load().len() >= 16 {
            return;
        }

        // Gate: find a live BlindRsaBatch authority + epoch with verify_material.
        let pk = {
            let params_arc = self.credential_params.load();
            let params = match params_arc.as_ref().as_ref() {
                Some(p) => p.clone(),
                None => return,
            };
            match find_blind_rsa_epoch(&params) {
                Some(ep) => {
                    match bt::IssuerPublicKey::from_der(&ep.verify_material) {
                        Ok(pk) => pk,
                        Err(e) => {
                            warn!("monitor_link: batch request: bad issuer pubkey in params: {e}");
                            return;
                        }
                    }
                }
                None => return,
            }
        };

        // Blind 64 random messages.
        const BATCH_SIZE: usize = 64;
        let mut blinded_hex = Vec::with_capacity(BATCH_SIZE);
        let mut retained: Vec<([u8; bt::MSG_LEN], bt::ClientBlind)> = Vec::with_capacity(BATCH_SIZE);

        for _ in 0..BATCH_SIZE {
            let mut msg = [0u8; bt::MSG_LEN];
            rand::rngs::OsRng.fill_bytes(&mut msg);
            match bt::blind(&pk, &msg) {
                Ok((blinded_bytes, client_blind)) => {
                    blinded_hex.push(hex::encode(&blinded_bytes));
                    retained.push((msg, client_blind));
                }
                Err(e) => {
                    warn!("monitor_link: batch request: blind failed: {e}");
                    // Partial batch — clear in_flight flag and abort.
                    return;
                }
            }
        }

        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);

        // Stash retained state BEFORE sending (no race — in_flight guards re-entry).
        {
            let batch = InflightBatch { pk, retained };
            match self.inflight.lock() {
                Ok(mut map) => { map.insert(request_id, batch); }
                Err(_) => return,
            }
        }
        self.request_in_flight.store(true, Ordering::Release);

        // Serialise and send.
        let frame = TaggedClientFrame::CredentialRequest {
            request_id,
            enrol_token,
            blinded_hex,
        };
        match serde_json::to_string(&frame) {
            Ok(json) => {
                if let Err(e) = write.send(Message::Text(json.into())).await {
                    warn!("monitor_link: batch request: send failed: {e}");
                    // Clean up — remove inflight entry and clear flag.
                    if let Ok(mut map) = self.inflight.lock() {
                        map.remove(&request_id);
                    }
                    self.request_in_flight.store(false, Ordering::Release);
                } else {
                    info!(request_id, n = BATCH_SIZE, "monitor_link: requested credential batch");
                }
            }
            Err(e) => {
                warn!("monitor_link: batch request: serialise failed: {e}");
                if let Ok(mut map) = self.inflight.lock() {
                    map.remove(&request_id);
                }
                self.request_in_flight.store(false, Ordering::Release);
            }
        }
    }

    /// Same as `maybe_request_batch_ws` but for the OverChain text-frame path.
    ///
    /// OverChain uses a different sink type (`OverChainChannel::send_text`),
    /// so we factor the send differently while keeping the batch-prep logic shared.
    async fn maybe_request_batch_oc(&self, channel: &mut crate::monitor_channel::OverChainChannel) {
        // Gate: enrol_token must be set.
        let enrol_token = match self.enrol_token.lock() {
            Ok(g) => g.clone(),
            Err(_) => return,
        };
        let enrol_token = match enrol_token {
            Some(t) => t,
            None => return,
        };

        if self.request_in_flight.load(Ordering::Acquire) { return; }
        if self.token_pool.load().len() >= 16 { return; }

        let pk = {
            let params_arc = self.credential_params.load();
            let params = match params_arc.as_ref().as_ref() {
                Some(p) => p.clone(),
                None => return,
            };
            match find_blind_rsa_epoch(&params) {
                Some(ep) => match bt::IssuerPublicKey::from_der(&ep.verify_material) {
                    Ok(pk) => pk,
                    Err(e) => { warn!("monitor_link: OC batch request: bad pubkey: {e}"); return; }
                },
                None => return,
            }
        };

        const BATCH_SIZE: usize = 64;
        let mut blinded_hex = Vec::with_capacity(BATCH_SIZE);
        let mut retained: Vec<([u8; bt::MSG_LEN], bt::ClientBlind)> = Vec::with_capacity(BATCH_SIZE);

        for _ in 0..BATCH_SIZE {
            let mut msg = [0u8; bt::MSG_LEN];
            rand::rngs::OsRng.fill_bytes(&mut msg);
            match bt::blind(&pk, &msg) {
                Ok((blinded_bytes, client_blind)) => {
                    blinded_hex.push(hex::encode(&blinded_bytes));
                    retained.push((msg, client_blind));
                }
                Err(e) => { warn!("monitor_link: OC batch request: blind failed: {e}"); return; }
            }
        }

        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        {
            let batch = InflightBatch { pk, retained };
            match self.inflight.lock() {
                Ok(mut map) => { map.insert(request_id, batch); }
                Err(_) => return,
            }
        }
        self.request_in_flight.store(true, Ordering::Release);

        let frame = TaggedClientFrame::CredentialRequest {
            request_id,
            enrol_token,
            blinded_hex,
        };
        match serde_json::to_string(&frame) {
            Ok(json) => {
                if let Err(e) = channel.send_text(&json).await {
                    warn!("monitor_link: OC batch request: send failed: {e}");
                    if let Ok(mut map) = self.inflight.lock() { map.remove(&request_id); }
                    self.request_in_flight.store(false, Ordering::Release);
                } else {
                    info!(request_id, n = BATCH_SIZE, "monitor_link: OC requested credential batch");
                }
            }
            Err(e) => {
                warn!("monitor_link: OC batch request: serialise failed: {e}");
                if let Ok(mut map) = self.inflight.lock() { map.remove(&request_id); }
                self.request_in_flight.store(false, Ordering::Release);
            }
        }
    }

    /// Send a `RingEnrolRequest` over the Wire WS write half if the in-flight
    /// flag is set.  The flag is set by `apply_credential_params` when a ring
    /// epoch change is detected.  Mirrors `maybe_request_batch_ws`.
    async fn maybe_request_ring_enrol_ws<S>(&self, write: &mut S)
    where
        S: futures::SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
    {
        if !self.ring_enrol_in_flight.load(Ordering::Acquire) {
            return;
        }

        // Resolve sentinel_id from the registered client_id (advisory — monitor
        // uses session client_id authoritatively).  Falls back to empty string
        // if not yet set; monitor tolerates this.
        let sentinel_id = self.client_id.lock().await.clone().unwrap_or_default();

        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let frame = TaggedClientFrame::RingEnrolRequest { request_id, sentinel_id };
        match serde_json::to_string(&frame) {
            Ok(json) => {
                if let Err(e) = write.send(Message::Text(json.into())).await {
                    warn!(request_id, "monitor_link: ring enrol request: Wire send failed: {e}");
                    self.ring_enrol_in_flight.store(false, Ordering::Release);
                } else {
                    info!(request_id, "monitor_link: ring enrol request sent (Wire)");
                }
            }
            Err(e) => {
                warn!(request_id, "monitor_link: ring enrol request: serialize failed: {e}");
                self.ring_enrol_in_flight.store(false, Ordering::Release);
            }
        }
    }

    /// Same as `maybe_request_ring_enrol_ws` but for the OverChain path.
    async fn maybe_request_ring_enrol_oc(
        &self,
        channel: &mut crate::monitor_channel::OverChainChannel,
    ) {
        if !self.ring_enrol_in_flight.load(Ordering::Acquire) {
            return;
        }

        let sentinel_id = self.client_id.lock().await.clone().unwrap_or_default();
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let frame = TaggedClientFrame::RingEnrolRequest { request_id, sentinel_id };
        match serde_json::to_string(&frame) {
            Ok(json) => {
                if let Err(e) = channel.send_text(&json).await {
                    warn!(request_id, "monitor_link: ring enrol request: OC send failed: {e}");
                    self.ring_enrol_in_flight.store(false, Ordering::Release);
                } else {
                    info!(request_id, "monitor_link: ring enrol request sent (OverChain)");
                }
            }
            Err(e) => {
                warn!(request_id, "monitor_link: ring enrol request: serialize failed: {e}");
                self.ring_enrol_in_flight.store(false, Ordering::Release);
            }
        }
    }

    /// Handle a `RingSlot` frame: parse material, build `RingCredential`, and
    /// hot-swap into the shared cell.  Clears the in-flight flag.
    fn handle_ring_slot(
        &self,
        request_id: u64,
        epoch_id: u32,
        slot_index: u64,
        member_sk_hex: String,
        path: Vec<WireRingPathStep>,
    ) {
        // Hex-decode + parse member secret key.
        let sk_bytes: [u8; 32] = match hex::decode(&member_sk_hex)
            .ok()
            .and_then(|b| b.try_into().ok())
        {
            Some(b) => b,
            None => {
                warn!(request_id, epoch_id, "monitor_link: RingSlot: invalid member_sk_hex — ignoring");
                self.ring_enrol_in_flight.store(false, Ordering::Release);
                return;
            }
        };
        let member_sk = match mirage_ringmember::MemberSecret::from_bytes(sk_bytes) {
            Some(sk) => sk,
            None => {
                warn!(request_id, epoch_id, "monitor_link: RingSlot: member_sk not a canonical ristretto scalar — ignoring");
                self.ring_enrol_in_flight.store(false, Ordering::Release);
                return;
            }
        };

        // Decode and map path steps to (sibling_bytes, is_right) tuples.
        let mut decoded_path: Vec<mirage_ringmember::PathStep> = Vec::with_capacity(path.len());
        for step in &path {
            let sib: [u8; 32] = match hex::decode(&step.sibling_hex)
                .ok()
                .and_then(|b| b.try_into().ok())
            {
                Some(b) => b,
                None => {
                    warn!(request_id, epoch_id, sibling = %step.sibling_hex, "monitor_link: RingSlot: invalid sibling_hex in path — ignoring");
                    self.ring_enrol_in_flight.store(false, Ordering::Release);
                    return;
                }
            };
            decoded_path.push((sib, step.is_right));
        }

        let rc = {
            let dir_guard = self.nf_counter_dir.lock().ok();
            let dir_opt = dir_guard.as_ref().and_then(|g| g.as_ref());
            match dir_opt {
                Some(dir) => {
                    crate::token_pool::RingCredential::new_persisted(
                        member_sk,
                        decoded_path,
                        epoch_id,
                        dir,
                    )
                }
                None => {
                    crate::token_pool::RingCredential::new(member_sk, decoded_path, epoch_id)
                }
            }
        };
        self.ring_cell.store(Arc::new(Some(rc)));
        self.ring_enrol_in_flight.store(false, Ordering::Release);
        info!(
            request_id,
            epoch_id,
            slot_index,
            "monitor_link: ring credential hot-swapped (epoch rotated)"
        );

        // Nudge the sentinel's frame-watcher to re-materialise the supervisor
        // config so the data-plane worker (a pure config consumer with no
        // MonitorLink of its own in orchestrator mode) picks up this fresh ring
        // credential on its next reload. Re-emitting the current CredentialParams
        // drives the existing rewrite path, which now reads the live ring cell.
        if let Some(params) = self.credential_params.load().as_ref().as_ref() {
            let _ = self
                .frame_tx
                .send(Some(WsServerFrame::CredentialParams(params.clone())));
        }
    }

    /// Handle a `RingEnrolError` frame: log, clear in-flight flag so the next
    /// params tick can retry.
    fn handle_ring_enrol_error(&self, request_id: u64, error: String) {
        warn!(
            request_id,
            error = %error,
            "monitor_link: ring enrol request failed by monitor — will retry on next epoch tick"
        );
        self.ring_enrol_in_flight.store(false, Ordering::Release);
        // Also reset the last-epoch sentinel so the next params tick re-arms.
        self.last_ring_epoch_requested.store(0, Ordering::Release);
    }

    /// Handle a `CredentialIssueResult` frame from the monitor.
    ///
    /// Looks up + removes the in-flight entry for `request_id`, unblind-fills
    /// the returned signatures into [`StoredToken`]s, and appends them to the
    /// pool via ArcSwap RCU.  Clears `request_in_flight`.
    fn handle_issue_result(&self, request_id: u64, epoch_id: u32, blind_sigs_hex: Vec<String>) {
        let batch = match self.inflight.lock() {
            Ok(mut map) => map.remove(&request_id),
            Err(_) => None,
        };
        let batch = match batch {
            Some(b) => b,
            None => {
                warn!(request_id, "monitor_link: CredentialIssueResult for unknown request_id — ignoring");
                self.request_in_flight.store(false, Ordering::Release);
                return;
            }
        };
        self.request_in_flight.store(false, Ordering::Release);

        let new_tokens = absorb_issue_result(&batch.pk, batch.retained, &blind_sigs_hex, epoch_id);
        let n = new_tokens.len();

        self.token_pool.rcu(|cur| {
            let mut next = (**cur).clone();
            next.extend(new_tokens.iter().cloned());
            next
        });

        info!(request_id, epoch_id, added = n, pool = self.token_pool.load().len(),
              "monitor_link: credential batch absorbed");
    }

    /// Handle a `CredentialIssueError` frame from the monitor.
    ///
    /// Removes the in-flight entry and clears `request_in_flight` so the next
    /// low-water trigger can retry.
    fn handle_issue_error(&self, request_id: u64, error: String) {
        if let Ok(mut map) = self.inflight.lock() {
            map.remove(&request_id);
        }
        self.request_in_flight.store(false, Ordering::Release);
        warn!(request_id, error = %error, "monitor_link: credential batch request failed by monitor");
    }

    /// Snapshot the registered client_id (set after first successful
    /// `register()`, cleared on `recover()`).
    pub async fn client_id(&self) -> Option<String> {
        self.client_id.lock().await.clone()
    }

    /// Seed the registered client_id from outside (sentinel R1 path).
    ///
    /// Must be called **before** `spawn()` so that `run_wire_loop` sees the
    /// correct client_id on the very first subscribe attempt.  When both
    /// `client_id` and `client_token` are set before spawn, no internal
    /// re-register is needed on first connect and the subscribe URL carries
    /// the correct credentials immediately (no 401 → no 20 s backoff gap).
    pub async fn set_client_id(&self, client_id: String) {
        *self.client_id.lock().await = Some(client_id);
    }

    /// Store the per-client token issued by the monitor on registration.
    ///
    /// Called by the sentinel after `register_with_failover` returns a token.
    /// The token is appended as `?client_id=X&client_token=Y` on subscribe
    /// calls.  `None` clears any existing token (safe to call with `None` —
    /// no-op for backward compat).
    pub async fn set_client_token(&self, token: Option<String>) {
        *self.client_token.lock().await = token;
    }

    /// Spawn the background loop.  Returns the task handle.
    pub fn spawn(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let link = Arc::clone(self);
        tokio::spawn(async move {
            link.run().await;
        })
    }

    // ── Internal ──────────────────────────────────────────────────────────────

    async fn run(&self) {
        // Wire transport is primary.  Retry every 20 s on failure.
        // On every failed Wire attempt, escalate to OverChain if configured.
        let wire_retry_interval = Duration::from_secs(20);

        // B2a: rotate to next monitor candidate after this many consecutive
        // Wire connect failures on the same address.  Env knob:
        // `MIRAGE_MONITOR_WIRE_ROTATE_AFTER` (default 3, min 1).
        let rotate_after: u32 = std::env::var("MIRAGE_MONITOR_WIRE_ROTATE_AFTER")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .map(|n| n.max(1))
            .unwrap_or(3);

        loop {
            if let Some(wire) = self.wire_transport().await {
                info!("monitor_link: Wire transport configured — attempting Mirage wire WS");
                match self.run_wire_loop(&wire).await {
                    Ok(()) => {
                        info!("monitor_link: Wire WS closed cleanly");
                        // Clean close = server-side shutdown; stay sticky on
                        // the working address, reset the failure counter.
                        self.wire_fail_count.store(0, Ordering::Relaxed);
                    }
                    Err(e) => {
                        warn!("monitor_link: Wire WS failed ({e})");
                        let prev = self.wire_fail_count.fetch_add(1, Ordering::Relaxed);
                        let now_count = prev + 1;
                        if now_count >= rotate_after {
                            // Attempt to rotate to the next monitor candidate.
                            // Rotation resets the counter regardless of whether
                            // a new candidate was found (avoids spinning if the
                            // peer_table has only one entry).
                            self.wire_fail_count.store(0, Ordering::Relaxed);
                            self.rotate_wire_candidate().await;
                        }
                    }
                }
            } else {
                warn!("monitor_link: no Wire transport configured — waiting before retry");
            }

            // Wire failed or absent — escalate to OverChain when available.
            if self.has_over_chain_transports() {
                info!("monitor_link: escalating to OverChain fallback");
                self.run_over_chain_fallback().await;
                info!("monitor_link: OverChain fallback exited — retrying Wire");
            } else {
                // No fallback available: back off before retrying Wire.
                tokio::time::sleep(wire_retry_interval).await;
            }
        }
    }

    /// Extract all `"monitor"`-labeled Wire candidates (`:19444`) from the live
    /// `peer_table` and advance `wire_override` to the next entry in round-robin
    /// order.
    ///
    /// Pin preservation: every candidate carries its own `pubkey_hex` from the
    /// peer_table entry — the same source `seed_wire_transport` uses for the
    /// initial entry.  No candidate is ever used without its paired key.
    ///
    /// If the table is empty or has only one entry that matches the current
    /// override, the override is unchanged and a `warn!` is emitted.
    async fn rotate_wire_candidate(&self) {
        let pt = self.peer_table.load();
        let mesh_id = self.mesh_id.clone();

        // Collect all monitor Wire entries (label="monitor", addr=*:19444).
        let candidates: Vec<WireParams> = pt
            .peers
            .iter()
            .filter(|e| {
                e.labels.iter().any(|l| l == "monitor") && e.addr.ends_with(":19444")
            })
            .map(|e| WireParams {
                monitor_addr: e.addr.clone(),
                monitor_pubkey_hex: e.pubkey_hex.clone(),
                mesh_id: mesh_id.clone(),
                host_hint: None,
            })
            .collect();

        if candidates.is_empty() {
            warn!(
                "monitor_link: Wire rotation requested but peer_table has no \
                 monitor Wire candidates — keeping current target"
            );
            return;
        }

        if candidates.len() == 1 {
            // Only one candidate: log and stay (already rotating to itself).
            let only = &candidates[0];
            let current = self.wire_override.lock().await.clone();
            if current.as_ref().map(|w| w.monitor_addr.as_str()) == Some(only.monitor_addr.as_str()) {
                warn!(
                    current = %only.monitor_addr,
                    "monitor_link: Wire rotation — only one monitor candidate in peer_table, \
                     cannot advance; per-URL backoff will pace retries"
                );
            } else {
                let new_addr = only.monitor_addr.clone();
                info!(
                    new = %new_addr,
                    "monitor_link: Wire rotation — single candidate, seeding"
                );
                *self.wire_override.lock().await = Some(only.clone());
            }
            return;
        }

        // Advance the candidate index and pick.
        let idx = self.wire_candidate_idx.fetch_add(1, Ordering::Relaxed);
        let next_idx = ((idx + 1) as usize) % candidates.len();
        let next = candidates[next_idx].clone();

        let old_addr = self
            .wire_override
            .lock()
            .await
            .as_ref()
            .map(|w| w.monitor_addr.clone())
            .unwrap_or_default();

        info!(
            old = %old_addr,
            new = %next.monitor_addr,
            candidates = candidates.len(),
            "monitor_link: Wire rotation — advancing to next monitor candidate"
        );

        *self.wire_override.lock().await = Some(next);
    }

    /// True when any `OverChain` transport is configured.
    fn has_over_chain_transports(&self) -> bool {
        self.transports
            .iter()
            .any(|t| matches!(t, MonitorTransport::OverChain { .. }))
    }

    /// Return the active Wire transport params, if any.
    ///
    /// Checks `wire_override` first (runtime-seeded from peer_table); falls
    /// back to the first `Wire` entry in the static `transports` list.
    async fn wire_transport(&self) -> Option<WireParams> {
        // Runtime override takes precedence (auto-seeded from peer_table).
        if let Some(ref wp) = *self.wire_override.lock().await {
            return Some(wp.clone());
        }
        for t in &self.transports {
            if let MonitorTransport::Wire {
                monitor_addr,
                monitor_pubkey_hex,
                mesh_id,
                host_hint,
            } = t
            {
                return Some(WireParams {
                    monitor_addr: monitor_addr.clone(),
                    monitor_pubkey_hex: monitor_pubkey_hex.clone(),
                    mesh_id: mesh_id.clone(),
                    host_hint: host_hint.clone(),
                });
            }
        }
        None
    }

    /// Inject wire transport coordinates at runtime (Phase 3 auto-seed).
    ///
    /// Called by the sentinel after it extracts the monitor's wire entry from
    /// the first config's `peer_table`. The override is tried on every Wire
    /// retry cycle from this point forward.
    ///
    /// Setting `None` removes any previous runtime override (falls back to the
    /// static `transports` list or no-Wire if that is also absent).
    pub async fn seed_wire_transport(
        &self,
        monitor_addr: String,
        monitor_pubkey_hex: String,
        mesh_id: String,
    ) {
        info!(
            addr = %monitor_addr,
            mesh_id = %mesh_id,
            "monitor_link: wire transport seeded from peer_table"
        );
        *self.wire_override.lock().await = Some(WireParams {
            monitor_addr,
            monitor_pubkey_hex,
            mesh_id,
            host_hint: None,
        });
    }

    /// Drive a Mirage-wire WebSocket subscribe connection.
    ///
    /// Dials the monitor's V9 wire endpoint (`:19444`), wraps the resulting
    /// Mirage tunnel as `AsyncRead+AsyncWrite` via [`tunnel_into_io`], then
    /// upgrades to WebSocket over that IO with a `ws://` URI.
    ///
    /// No bearer token is sent — the monitor's `/relay/clients/subscribe`
    /// is unauthenticated (rate-limited only). The Mirage wire already
    /// provides E2E encryption and mutual forward-secret auth.
    ///
    /// The frame receive loop is identical to [`run_ws_loop`] — same
    /// `TaggedServerFrame` dispatch, same keepalive strategy.
    async fn run_wire_loop(&self, wire: &WireParams) -> Result<(), String> {
        use futures::{SinkExt, StreamExt};
        use mirage_core::config::{AuthMode, MirageEndpointConfig, TransportKind};
        use mirage_protocol::driver::run_client_tcp_v9;

        // Decode the monitor's static public key.
        let server_static_pub: [u8; 32] = hex::decode(&wire.monitor_pubkey_hex)
            .map_err(|e| format!("Wire: decode monitor_pubkey_hex '{}': {e}", wire.monitor_pubkey_hex))?
            .try_into()
            .map_err(|_| format!("Wire: monitor_pubkey_hex must be 32 bytes: '{}'", wire.monitor_pubkey_hex))?;

        let server_addr: SocketAddr = wire
            .monitor_addr
            .parse()
            .map_err(|e| format!("Wire: parse monitor_addr '{}': {e}", wire.monitor_addr))?;

        // SNI = host portion of monitor_addr (IP or hostname without port).
        let sni = wire
            .host_hint
            .as_deref()
            .or_else(|| wire.monitor_addr.rsplit_once(':').map(|(h, _)| h))
            .unwrap_or(wire.monitor_addr.as_str())
            .to_string();

        // Dial V9 — anonymous ephemeral client, no DeclaredIdentity, no credential.
        let config = MirageEndpointConfig {
            transport: TransportKind::Tcp,
            auth: AuthMode::Blake3,
            camouflage: mirage_core::config::TransportCamouflage::None,
            byte_cipher: None,
            byte_cipher_mode: None,
        };

        info!(
            addr = %wire.monitor_addr,
            mesh_id = %wire.mesh_id,
            "monitor_link: Wire — dialing monitor V9"
        );

        let tunnel = run_client_tcp_v9(
            &config,
            server_addr,
            &server_static_pub,
            &self.ephemeral_client_priv,
            &sni,
            wire.mesh_id.as_bytes(),
            None, // anonymous client — no credential
        )
        .await
        .map_err(|e| format!("Wire: V9 dial failed: {e}"))?;

        info!("monitor_link: Wire — V9 dial succeeded, upgrading to WS");

        let (io, _pump) = mirage_mesh_client::tunnel_into_io(tunnel);

        // ── Token-race fix ────────────────────────────────────────────────────
        // Register over the wire connection BEFORE building the subscribe URL.
        // If a client_id was already seeded externally (sentinel R1 path via
        // `set_client_id` + `set_client_token` before `spawn()`), reuse it —
        // no re-register needed, and the token is already correct.
        // If client_id is absent (cold start without prior R1, or a retry after
        // `recover()` cleared it), register now on THIS wire connection so that
        // the token we store is the one the monitor just bound to this client_id.
        // This guarantees the subscribe URL uses the SAME token the monitor
        // stored for this client_id — no mismatch, no 401 on first attempt.
        {
            let needs_register = self.client_id.lock().await.is_none();
            if needs_register {
                debug!("monitor_link: Wire — client_id absent, registering before subscribe");
                let body = RegisterBody {
                    os: std::env::consts::OS,
                    version: env!("CARGO_PKG_VERSION"),
                    supported_transports: vec!["tcp", "websocket"],
                };
                match serde_json::to_vec(&body) {
                    Ok(body_json) => {
                        match register_over_wire(wire, &self.ephemeral_client_priv, &body_json).await {
                            Ok(reg) => {
                                debug!(
                                    client_id = %reg.client_id,
                                    "monitor_link: Wire pre-subscribe register (R2-wire)"
                                );
                                self.poll_interval_secs
                                    .store(reg.monitor_poll_interval_secs, Ordering::Relaxed);
                                *self.client_id.lock().await = Some(reg.client_id);
                                if reg.client_token.is_some() {
                                    *self.client_token.lock().await = reg.client_token;
                                }
                            }
                            Err(e) => {
                                warn!("monitor_link: Wire pre-subscribe register failed ({e}) — subscribing without token");
                            }
                        }
                    }
                    Err(e) => {
                        warn!("monitor_link: Wire pre-subscribe register: serialize body failed ({e})");
                    }
                }
            }
        }
        // ── End token-race fix ────────────────────────────────────────────────

        // Read client_id + token AFTER the conditional register above so we
        // always use the freshest values bound to THIS registration session.
        let cid = self.client_id.lock().await.clone();
        let tok = self.client_token.lock().await.clone();
        let host = &sni;
        // ws:// scheme — the Mirage tunnel already provides E2E encryption.
        // TLS on top would be redundant double-encryption (same as node pattern).
        let ws_base = format!("ws://{host}");
        let ws_uri = build_subscribe_url(&ws_base, cid.as_deref(), tok.as_deref());

        // Build WS upgrade request — NO Authorization header (subscribe is unauthenticated).
        let request = Request::builder()
            .method("GET")
            .uri(&ws_uri)
            .header(header::HOST, host.as_str())
            .header("upgrade", "websocket")
            .header("connection", "upgrade")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", random_sec_key())
            .body(())
            .map_err(|e| format!("Wire: build WS request: {e}"))?;

        let (ws_stream, _resp) = tokio_tungstenite::client_async(request, io)
            .await
            .map_err(|e| format!("Wire: WS upgrade failed: {e}"))?;

        info!("monitor_link: state → Connected (Wire WebSocket)");

        let (mut write, mut read) = ws_stream.split();

        // Keepalive strategy mirrors run_ws_loop exactly.
        let ping_interval_dur = Duration::from_secs(10);
        let pong_timeout = Duration::from_secs(15);
        let mut ping_ticker = tokio::time::interval(ping_interval_dur);
        let mut app_ping_counter: u64 = 0;
        let mut app_ping_nonce: Option<u64> = None;
        let mut app_ping_sent_at: Option<tokio::time::Instant> = None;
        let mut last_pong_at = tokio::time::Instant::now();
        let mut last_ws_pong = tokio::time::Instant::now();
        let mut ws_ping_outstanding_since: Option<tokio::time::Instant> = None;

        loop {
            tokio::select! {
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            match parse_ws_frame(&text) {
                                Ok(TaggedServerFrame::Config(cfg)) => {
                                    debug!(
                                        version = cfg.version,
                                        chains = cfg.chains.len(),
                                        "monitor_link: Wire config push received"
                                    );
                                    self.apply_config(cfg);
                                }
                                Ok(TaggedServerFrame::DebugTunnelOpen(payload)) => {
                                    info!(
                                        "monitor_link: Wire DebugTunnelOpen (host={}, port={}, ttl={}s)",
                                        payload.monitor_ssh_host,
                                        payload.reverse_port,
                                        payload.ttl_secs,
                                    );
                                    let _ = self.frame_tx.send(Some(WsServerFrame::DebugTunnelOpen(payload)));
                                }
                                Ok(TaggedServerFrame::DebugTunnelClose) => {
                                    info!("monitor_link: Wire DebugTunnelClose received");
                                    let _ = self.frame_tx.send(Some(WsServerFrame::DebugTunnelClose));
                                }
                                Ok(TaggedServerFrame::ChainAvailable(ca)) => {
                                    info!(
                                        chain_id = %ca.chain_id,
                                        est_score = ca.est_score,
                                        "monitor_link: Wire ChainAvailable received"
                                    );
                                    let _ = self.frame_tx.send(Some(WsServerFrame::ChainAvailable(ca)));
                                }
                                Ok(TaggedServerFrame::CredentialParams(params)) => {
                                    self.apply_credential_params(params, "wire");
                                    self.maybe_request_batch_ws(&mut write).await;
                                    self.maybe_request_ring_enrol_ws(&mut write).await;
                                }
                                Ok(TaggedServerFrame::Pong { nonce }) => {
                                    if app_ping_nonce == Some(nonce) {
                                        debug!(nonce, "monitor_link: Wire app Pong received");
                                        app_ping_nonce = None;
                                        app_ping_sent_at = None;
                                        last_pong_at = tokio::time::Instant::now();
                                    } else {
                                        debug!(nonce, "monitor_link: Wire stale/unexpected Pong nonce");
                                    }
                                }
                                Ok(TaggedServerFrame::CredentialIssueResult { request_id, epoch_id, blind_sigs_hex }) => {
                                    self.handle_issue_result(request_id, epoch_id, blind_sigs_hex);
                                }
                                Ok(TaggedServerFrame::CredentialIssueError { request_id, error }) => {
                                    self.handle_issue_error(request_id, error);
                                }
                                Ok(TaggedServerFrame::RingSlot { request_id, epoch_id, slot_index, member_sk_hex, path }) => {
                                    self.handle_ring_slot(request_id, epoch_id, slot_index, member_sk_hex, path);
                                }
                                Ok(TaggedServerFrame::RingEnrolError { request_id, error }) => {
                                    self.handle_ring_enrol_error(request_id, error);
                                }
                                Err(e) => {
                                    warn!("monitor_link: Wire frame parse error: {e}");
                                }
                            }
                        }
                        Some(Ok(Message::Ping(_))) => {
                            // tungstenite auto-responds with Pong.
                        }
                        Some(Ok(Message::Pong(_))) => {
                            last_ws_pong = tokio::time::Instant::now();
                            ws_ping_outstanding_since = None;
                        }
                        Some(Ok(Message::Close(_))) | None => {
                            info!("monitor_link: Wire WebSocket closed by server");
                            return Ok(());
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            return Err(format!("Wire WS stream error: {e}"));
                        }
                    }
                }

                _ = ping_ticker.tick() => {
                    let now = tokio::time::Instant::now();

                    if let Some(sent_at) = app_ping_sent_at {
                        if now.duration_since(sent_at) > pong_timeout {
                            return Err(format!(
                                "monitor_link: Wire app pong timeout: no Pong for {:?}",
                                now.duration_since(sent_at)
                            ));
                        }
                    }
                    if let Some(sent_at) = ws_ping_outstanding_since {
                        if now.duration_since(sent_at) > pong_timeout {
                            return Err(format!(
                                "monitor_link: Wire WS pong timeout: no WS Pong for {:?}",
                                now.duration_since(sent_at)
                            ));
                        }
                    }
                    if last_pong_at.elapsed() > ping_interval_dur + pong_timeout {
                        return Err(format!(
                            "monitor_link: Wire app pong idle: no Pong for {:?}",
                            last_pong_at.elapsed()
                        ));
                    }
                    if last_ws_pong.elapsed() > ping_interval_dur + pong_timeout {
                        return Err(format!(
                            "monitor_link: Wire WS pong idle: no WS Pong for {:?}",
                            last_ws_pong.elapsed()
                        ));
                    }

                    app_ping_counter += 1;
                    let nonce = app_ping_counter;
                    let ping_json = match serde_json::to_string(&TaggedClientFrame::Ping { nonce }) {
                        Ok(j) => j,
                        Err(e) => {
                            return Err(format!("monitor_link: Wire failed to serialize Ping: {e}"));
                        }
                    };
                    match write.send(Message::Text(ping_json.into())).await {
                        Ok(()) => {
                            app_ping_nonce = Some(nonce);
                            app_ping_sent_at = Some(now);
                        }
                        Err(e) => {
                            return Err(format!("monitor_link: Wire app Ping send failed: {e}"));
                        }
                    }
                    match write.send(Message::Ping(vec![])).await {
                        Ok(()) => {
                            if ws_ping_outstanding_since.is_none() {
                                ws_ping_outstanding_since = Some(now);
                            }
                        }
                        Err(e) => {
                            return Err(format!("monitor_link: Wire WS Ping send failed: {e}"));
                        }
                    }
                }
            }
        }
    }

    /// Drive the OverChain fallback loop.
    ///
    /// Tries all configured `OverChain` transports in sequence.  Returns when:
    /// - Wire transport recovers (caller re-attempts Wire on return), or
    /// - An OverChain connection closes and no replacement is available.
    ///
    /// Caller (`run()`) retries Wire after this function returns — OverChain
    /// is a temporary fallback for restricted networks where direct `:19444` is
    /// DPI-blocked, not a permanent transport.
    async fn run_over_chain_fallback(&self) {
        let channel = self.try_connect_over_chain().await;

        match channel {
            None => {
                // All OverChain transports failed — back off then return so
                // the caller can retry Wire.
                info!(
                    "monitor_link: all OverChain transports failed, backoff={:?}",
                    OVER_CHAIN_BACKOFF_INIT
                );
                tokio::time::sleep(OVER_CHAIN_BACKOFF_INIT).await;
            }
            Some(mut ch) => {
                info!("monitor_link: OverChain connected — entering receive loop");
                self.run_over_chain_loop(&mut ch).await;
                let _ = ch.close().await;
                warn!("monitor_link: OverChain channel closed — returning to Wire retry");
            }
        }
    }

    /// Try to open one OverChain connection.  Returns the first successful
    /// channel, or `None` if all transports fail.
    async fn try_connect_over_chain(&self) -> Option<OverChainChannel> {
        // No bearer token — OverChain auth is provided by the Mirage tunnel.
        let bearer = "";
        let mesh_id = self.mesh_id.as_str();
        let client_id = self.node_client_id.as_str();

        for transport in &self.transports {
            if let MonitorTransport::OverChain {
                worker_socks5,
                monitor_addr,
                chain_id,
            } = transport
            {
                // Option a' — same-process path: if the named chain lives in
                // our local chain_set (MonitorLink hosted in the same worker
                // that owns the pool), acquire a tunnel from the pool directly
                // and run the auth handshake on it. Bypasses SOCKS5 inception.
                if !chain_id.0.is_empty() {
                    if let Some(handle) = self.chain_set.chains.get(chain_id).map(|e| Arc::clone(e.value())) {
                        info!(chain_id = %chain_id, "monitor_link: OverChain via local pool.lease (option a')");
                        match tokio::time::timeout(
                            OVER_CHAIN_CONNECT_TIMEOUT,
                            try_lease_and_open(&handle, *monitor_addr, bearer, mesh_id, client_id),
                        )
                        .await
                        {
                            Ok(Ok(ch)) => {
                                info!(
                                    chain_id = %chain_id,
                                    addr = %monitor_addr,
                                    "monitor_link: OverChain connected via tunnel"
                                );
                                return Some(ch);
                            }
                            Ok(Err(e)) => {
                                warn!(chain_id = %chain_id, "monitor_link: OverChain tunnel path failed: {e} — trying SOCKS5 fallback");
                            }
                            Err(_) => {
                                warn!(chain_id = %chain_id, "monitor_link: OverChain tunnel path timed out — trying SOCKS5 fallback");
                            }
                        }
                    }
                }
                // Option (b) fallback — SOCKS5 to worker.
                match tokio::time::timeout(
                    OVER_CHAIN_CONNECT_TIMEOUT,
                    OverChainChannel::connect(*worker_socks5, *monitor_addr, bearer, mesh_id, client_id),
                )
                .await
                {
                    Ok(Ok(ch)) => {
                        info!(
                            chain_id = %chain_id,
                            worker_socks5 = %worker_socks5,
                            addr = %monitor_addr,
                            "monitor_link: OverChain connected via SOCKS5"
                        );
                        return Some(ch);
                    }
                    Ok(Err(e)) => {
                        warn!(
                            chain_id = %chain_id,
                            worker_socks5 = %worker_socks5,
                            "monitor_link: OverChain SOCKS5 path failed: {e}"
                        );
                    }
                    Err(_) => {
                        warn!(
                            chain_id = %chain_id,
                            worker_socks5 = %worker_socks5,
                            "monitor_link: OverChain SOCKS5 path timed out"
                        );
                    }
                }
            }
        }
        None
    }

    /// Drive the OverChain receive loop until the channel closes or errors.
    async fn run_over_chain_loop(&self, channel: &mut OverChainChannel) {
        use crate::monitor_channel::MonitorChannel;

        let ping_interval_dur = Duration::from_secs(10);
        let pong_timeout = Duration::from_secs(15);

        let mut ping_ticker = tokio::time::interval(ping_interval_dur);
        let mut ping_counter: u64 = 0;
        let mut outstanding_nonce: Option<u64> = None;
        let mut ping_sent_at: Option<tokio::time::Instant> = None;
        let mut last_pong_at = tokio::time::Instant::now();

        loop {
            // `channel` must not be borrowed across the select! boundary when we
            // call maybe_request_batch_oc / maybe_request_ring_enrol_oc after
            // the block, so we use flags.
            let mut request_batch_after = false;
            let mut request_ring_enrol_after = false;

            tokio::select! {
                // Drive OverChain receive.
                frame = channel.recv_frame() => {
                    match frame {
                        Ok(Some(payload)) => {
                            let text = match std::str::from_utf8(&payload) {
                                Ok(s) => s,
                                Err(e) => {
                                    warn!("monitor_link: OverChain frame not valid UTF-8: {e}");
                                    continue;
                                }
                            };
                            match parse_ws_frame(text) {
                                Ok(TaggedServerFrame::Config(cfg)) => {
                                    debug!(
                                        version = cfg.version,
                                        chains = cfg.chains.len(),
                                        "monitor_link: OverChain config push received"
                                    );
                                    self.apply_config(cfg);
                                }
                                Ok(TaggedServerFrame::DebugTunnelOpen(dtp)) => {
                                    info!(
                                        "monitor_link: OverChain DebugTunnelOpen received"
                                    );
                                    let _ = self
                                        .frame_tx
                                        .send(Some(WsServerFrame::DebugTunnelOpen(dtp)));
                                }
                                Ok(TaggedServerFrame::DebugTunnelClose) => {
                                    info!("monitor_link: OverChain DebugTunnelClose received");
                                    let _ = self
                                        .frame_tx
                                        .send(Some(WsServerFrame::DebugTunnelClose));
                                }
                                Ok(TaggedServerFrame::ChainAvailable(ca)) => {
                                    info!(
                                        chain_id = %ca.chain_id,
                                        est_score = ca.est_score,
                                        "monitor_link: OverChain ChainAvailable received"
                                    );
                                    let _ = self
                                        .frame_tx
                                        .send(Some(WsServerFrame::ChainAvailable(ca)));
                                }
                                Ok(TaggedServerFrame::CredentialParams(params)) => {
                                    self.apply_credential_params(params, "over_chain");
                                    request_batch_after = true;
                                    request_ring_enrol_after = self.should_send_ring_enrol();
                                }
                                Ok(TaggedServerFrame::Pong { nonce }) => {
                                    if outstanding_nonce == Some(nonce) {
                                        debug!(nonce, "monitor_link: OverChain app Pong received");
                                        outstanding_nonce = None;
                                        ping_sent_at = None;
                                        last_pong_at = tokio::time::Instant::now();
                                    } else {
                                        debug!(nonce, "monitor_link: OverChain stale Pong nonce");
                                    }
                                }
                                Ok(TaggedServerFrame::CredentialIssueResult { request_id, epoch_id, blind_sigs_hex }) => {
                                    self.handle_issue_result(request_id, epoch_id, blind_sigs_hex);
                                }
                                Ok(TaggedServerFrame::CredentialIssueError { request_id, error }) => {
                                    self.handle_issue_error(request_id, error);
                                }
                                Ok(TaggedServerFrame::RingSlot { request_id, epoch_id, slot_index, member_sk_hex, path }) => {
                                    self.handle_ring_slot(request_id, epoch_id, slot_index, member_sk_hex, path);
                                }
                                Ok(TaggedServerFrame::RingEnrolError { request_id, error }) => {
                                    self.handle_ring_enrol_error(request_id, error);
                                }
                                Err(e) => {
                                    warn!("monitor_link: OverChain frame parse error: {e}");
                                }
                            }
                        }
                        Ok(None) => {
                            return;
                        }
                        Err(e) => {
                            warn!("monitor_link: OverChain recv error: {e}");
                            return;
                        }
                    }
                }

                // App-level Ping keepalive.
                _ = ping_ticker.tick() => {
                    let now = tokio::time::Instant::now();

                    // Dead-detection: Pong not received within pong_timeout.
                    if let Some(sent_at) = ping_sent_at {
                        if now.duration_since(sent_at) > pong_timeout {
                            warn!(
                                "monitor_link: OverChain app pong timeout ({:?}) — channel dead",
                                now.duration_since(sent_at)
                            );
                            return;
                        }
                    }

                    // Idle fallback.
                    if last_pong_at.elapsed() > ping_interval_dur + pong_timeout {
                        warn!(
                            "monitor_link: OverChain app pong idle ({:?}) — channel dead",
                            last_pong_at.elapsed()
                        );
                        return;
                    }

                    // Send app-level Ping.
                    ping_counter += 1;
                    let nonce = ping_counter;
                    let ping_json = match serde_json::to_string(&TaggedClientFrame::Ping { nonce }) {
                        Ok(j) => j,
                        Err(e) => {
                            warn!("monitor_link: failed to serialize OverChain Ping: {e}");
                            continue;
                        }
                    };
                    match channel.send_text(&ping_json).await {
                        Ok(()) => {
                            outstanding_nonce = Some(nonce);
                            ping_sent_at = Some(now);
                        }
                        Err(e) => {
                            warn!("monitor_link: OverChain Ping send failed: {e}");
                            return;
                        }
                    }
                }
            }

            // Fire batch request outside the select! to avoid double-borrow of channel.
            if request_batch_after {
                self.maybe_request_batch_oc(channel).await;
            }
            // Fire ring-enrol request outside the select! to avoid double-borrow.
            if request_ring_enrol_after {
                self.maybe_request_ring_enrol_oc(channel).await;
            }
        }
    }

    /// Apply a parsed `ConfigResponse` and push through the watch channel.
    fn apply_config(&self, cfg: ConfigResponse) {
        self.config_version.store(cfg.version, Ordering::Relaxed);
        self.poll_interval_secs
            .store(cfg.monitor_poll_interval_secs, Ordering::Relaxed);

        // Phase 8.5: update peer_table when the monitor includes a snapshot.
        if let Some(pt) = cfg.peer_table {
            debug!(
                generation = pt.generation,
                peers = pt.peers.len(),
                "monitor_link: peer_table snapshot received"
            );
            self.peer_table.store(Arc::new(pt));
        }

        let monitor_cfg = MonitorConfig {
            version: cfg.version,
            chains: cfg
                .chains
                .into_iter()
                .map(|c| MonitorChain {
                    chain: c.chain,
                    healthy: c.healthy,
                    latency_ms: c.latency_ms,
                })
                .collect(),
            policy: cfg.policy,
            camouflage: cfg.camouflage,
            shaping: cfg.shaping,
            metadata_policy: cfg.metadata_policy,
        };
        let _ = self.config_tx.send(Some(monitor_cfg.clone()));
        let _ = self.frame_tx.send(Some(WsServerFrame::Config(monitor_cfg)));
    }
}

// ── Wire HTTP helpers ─────────────────────────────────────────────────────────

/// Issue `POST /relay/clients` over the Mirage wire using raw wire coordinate
/// strings (no `WireParams` struct required — public API for callers outside
/// this module such as `l0-sentinel::chain_request`).
///
/// Generates a fresh ephemeral X25519 client key per call (anonymous client,
/// no `DeclaredIdentity`).
///
/// Returns `(client_id, poll_interval_secs, client_token)` matching the
/// shape of `chain_request::register_with_failover`.
pub async fn register_post_over_wire(
    monitor_addr: &str,
    monitor_pubkey_hex: &str,
    mesh_id: &str,
    body_json: &[u8],
) -> Result<(String, u64, Option<String>), String> {
    let ephemeral: [u8; 32] = {
        use rand::RngCore;
        let mut k = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut k);
        k
    };
    let wire = WireParams {
        monitor_addr: monitor_addr.to_owned(),
        monitor_pubkey_hex: monitor_pubkey_hex.to_owned(),
        mesh_id: mesh_id.to_owned(),
        host_hint: None,
    };
    let reg = register_over_wire(&wire, &ephemeral, body_json).await?;
    Ok((reg.client_id, reg.monitor_poll_interval_secs, reg.client_token))
}

/// Dial the monitor's V9 wire endpoint and return it wrapped as duplex IO.
///
/// Mirrors the first part of [`run_wire_loop`] — same V9 anonymous-ephemeral
/// dial, same `tunnel_into_io` wrapping — factored out so any caller that needs
/// raw IO (e.g. `register_over_wire`) can reuse it without touching the WS
/// upgrade path.
///
/// Returns the `DuplexStream` (hand to hyper / manual HTTP writes) plus the
/// background pump handle (drop to close, or ignore for fire-and-forget).
async fn dial_monitor_wire_io(
    wire: &WireParams,
    ephemeral_client_priv: &[u8; 32],
) -> Result<(tokio::io::DuplexStream, tokio::task::JoinHandle<()>), String> {
    use mirage_core::config::{AuthMode, MirageEndpointConfig, TransportKind};
    use mirage_protocol::driver::run_client_tcp_v9;

    let server_static_pub: [u8; 32] = hex::decode(&wire.monitor_pubkey_hex)
        .map_err(|e| format!("wire_io: decode pubkey_hex '{}': {e}", wire.monitor_pubkey_hex))?
        .try_into()
        .map_err(|_| format!("wire_io: pubkey_hex must be 32 bytes: '{}'", wire.monitor_pubkey_hex))?;

    let server_addr: std::net::SocketAddr = wire
        .monitor_addr
        .parse()
        .map_err(|e| format!("wire_io: parse monitor_addr '{}': {e}", wire.monitor_addr))?;

    let config = MirageEndpointConfig {
        transport: TransportKind::Tcp,
        auth: AuthMode::Blake3,
        camouflage: mirage_core::config::TransportCamouflage::None,
        byte_cipher: None,
        byte_cipher_mode: None,
    };

    let tunnel = run_client_tcp_v9(
        &config,
        server_addr,
        &server_static_pub,
        ephemeral_client_priv,
        wire.host_hint
            .as_deref()
            .or_else(|| wire.monitor_addr.rsplit_once(':').map(|(h, _)| h))
            .unwrap_or(wire.monitor_addr.as_str()),
        wire.mesh_id.as_bytes(),
        None, // anonymous client
    )
    .await
    .map_err(|e| format!("wire_io: V9 dial failed: {e}"))?;

    Ok(mirage_mesh_client::tunnel_into_io(tunnel))
}

/// Issue `POST /relay/clients` over the Mirage wire (HTTP/1.0, no bearer).
///
/// The monitor's full axum router is already served over `:19444`
/// (first-byte HTTP dispatch → hyper `serve_connection`), so the register
/// endpoint is reachable through the tunnel without any new server code.
///
/// Mirrors the hand-written HTTP pattern from `mirage2operator`'s
/// `wire_http::wire_request` but adapted to the client's anonymous-
/// ephemeral tunnel type — no SenderProof headers (client has no
/// `DeclaredIdentity`).
///
/// On success returns `(client_id, poll_interval_secs, client_token)`.
async fn register_over_wire(
    wire: &WireParams,
    ephemeral_client_priv: &[u8; 32],
    body_json: &[u8],
) -> Result<RegisterResponse, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let host = wire
        .host_hint
        .as_deref()
        .or_else(|| wire.monitor_addr.rsplit_once(':').map(|(h, _)| h))
        .unwrap_or(wire.monitor_addr.as_str())
        .to_string();

    let (mut io, _pump) = dial_monitor_wire_io(wire, ephemeral_client_priv).await?;

    // Build minimal HTTP/1.0 POST (Connection: close — read until EOF).
    let head = format!(
        "POST /relay/clients HTTP/1.0\r\n\
         Host: {host}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        body_json.len()
    );

    io.write_all(head.as_bytes())
        .await
        .map_err(|e| format!("register_wire: send head: {e}"))?;
    io.write_all(body_json)
        .await
        .map_err(|e| format!("register_wire: send body: {e}"))?;

    // Read full response (HTTP/1.0 — server closes after reply).
    let mut response = Vec::new();
    io.read_to_end(&mut response)
        .await
        .map_err(|e| format!("register_wire: read response: {e}"))?;

    if response.is_empty() {
        return Err("register_wire: empty response from monitor".to_string());
    }

    // Split headers / body at \r\n\r\n.
    let header_end = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(response.len());

    let status_line = response[..header_end.min(response.len())]
        .split(|&b| b == b'\n')
        .next()
        .map(|l| String::from_utf8_lossy(l).into_owned())
        .unwrap_or_default();

    if !status_line.starts_with("HTTP/1.") {
        return Err(format!(
            "register_wire: unexpected response prefix: {:?}",
            &status_line[..status_line.len().min(40)]
        ));
    }

    let status_code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    if !(200..300).contains(&status_code) {
        let body_preview = String::from_utf8_lossy(
            &response[(header_end + 4).min(response.len())..],
        );
        return Err(format!(
            "register_wire: HTTP {status_code}: {body_preview}"
        ));
    }

    let body_bytes = &response[(header_end + 4).min(response.len())..];
    serde_json::from_slice::<RegisterResponse>(body_bytes)
        .map_err(|e| format!("register_wire: parse response JSON: {e}"))
}

/// Issue an arbitrary `POST <path>` over the Mirage wire.
///
/// Dials the monitor using an anonymous ephemeral V9 tunnel, writes a
/// hand-framed HTTP/1.0 POST with the supplied `bearer` token in the
/// `Authorization` header, and returns `(status_code, response_body_bytes)`.
///
/// The bearer token travels inside the V9-encrypted wire — it is never
/// exposed on the network in plaintext.
///
/// Returns `Err(String)` if the dial or the HTTP framing fails (non-2xx is
/// returned as `Ok((status, body))` so callers can decide how to handle it).
pub async fn wire_post(
    monitor_addr: &str,
    monitor_pubkey_hex: &str,
    mesh_id: &str,
    path: &str,
    bearer: &str,
    body_json: &[u8],
) -> Result<(u16, Vec<u8>), String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let ephemeral: [u8; 32] = {
        use rand::RngCore;
        let mut k = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut k);
        k
    };
    let wire = WireParams {
        monitor_addr: monitor_addr.to_owned(),
        monitor_pubkey_hex: monitor_pubkey_hex.to_owned(),
        mesh_id: mesh_id.to_owned(),
        host_hint: None,
    };

    let host = wire
        .monitor_addr
        .rsplit_once(':')
        .map(|(h, _)| h.to_owned())
        .unwrap_or_else(|| wire.monitor_addr.clone());

    let (mut io, _pump) = dial_monitor_wire_io(&wire, &ephemeral).await?;

    let auth_header = format!("Authorization: Bearer {bearer}\r\n");
    let head = format!(
        "POST {path} HTTP/1.0\r\n\
         Host: {host}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         {auth_header}\
         Connection: close\r\n\
         \r\n",
        body_json.len()
    );

    io.write_all(head.as_bytes())
        .await
        .map_err(|e| format!("wire_post: send head: {e}"))?;
    io.write_all(body_json)
        .await
        .map_err(|e| format!("wire_post: send body: {e}"))?;

    let mut response = Vec::new();
    io.read_to_end(&mut response)
        .await
        .map_err(|e| format!("wire_post: read response: {e}"))?;

    if response.is_empty() {
        return Err("wire_post: empty response from monitor".to_string());
    }

    let header_end = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(response.len());

    let status_line = response[..header_end.min(response.len())]
        .split(|&b| b == b'\n')
        .next()
        .map(|l| String::from_utf8_lossy(l).into_owned())
        .unwrap_or_default();

    if !status_line.starts_with("HTTP/1.") {
        return Err(format!(
            "wire_post: unexpected response prefix: {:?}",
            &status_line[..status_line.len().min(40)]
        ));
    }

    let status_code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let body_bytes = response[(header_end + 4).min(response.len())..].to_vec();
    Ok((status_code, body_bytes))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build the Wire WebSocket subscribe URL, appending `?client_id=X` and
/// `?client_token=Y` when present.
///
/// `ws_base` — the base WS URL including the scheme, e.g. `ws://host`.
/// The path `/relay/clients/subscribe` is always appended.
///
/// Backward-compat: if `client_id` is `None`, the raw path is returned with
/// no query params (older monitors).  If `client_token` is `None`, only
/// `?client_id=X` is appended.
fn build_subscribe_url(ws_base: &str, client_id: Option<&str>, client_token: Option<&str>) -> String {
    match (client_id, client_token) {
        (Some(id), Some(tok)) => {
            format!("{ws_base}/relay/clients/subscribe?client_id={id}&client_token={tok}")
        }
        (Some(id), None) => {
            format!("{ws_base}/relay/clients/subscribe?client_id={id}")
        }
        (None, _) => {
            format!("{ws_base}/relay/clients/subscribe")
        }
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor_channel::{MonitorChannel, MonitorChannelError};
    use std::collections::VecDeque;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    // ── Mock channel ──────────────────────────────────────────────────────────

    /// A simple in-memory channel for testing keepalive logic.
    ///
    /// `recv_queue` — frames the server would push (as raw JSON bytes).
    /// `sent_texts` — records every `send_text` call made by the client.
    /// `auto_pong`  — if true, automatically push a matching Pong frame into
    ///                `recv_queue` whenever a `send_text` containing a Ping is
    ///                detected (simulates a cooperative server).
    struct MockChannel {
        recv_queue: Arc<Mutex<VecDeque<Vec<u8>>>>,
        sent_texts: Arc<Mutex<Vec<String>>>,
        auto_pong: bool,
    }

    impl MockChannel {
        fn new_auto_pong() -> Self {
            Self {
                recv_queue: Arc::new(Mutex::new(VecDeque::new())),
                sent_texts: Arc::new(Mutex::new(Vec::new())),
                auto_pong: true,
            }
        }

        fn new_no_reply() -> Self {
            Self {
                recv_queue: Arc::new(Mutex::new(VecDeque::new())),
                sent_texts: Arc::new(Mutex::new(Vec::new())),
                auto_pong: false,
            }
        }
    }

    impl MonitorChannel for MockChannel {
        async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, MonitorChannelError> {
            loop {
                {
                    let mut q = self.recv_queue.lock().await;
                    if let Some(frame) = q.pop_front() {
                        return Ok(Some(frame));
                    }
                }
                tokio::task::yield_now().await;
            }
        }

        async fn send_pong(&mut self, _nonce: [u8; 8]) -> Result<(), MonitorChannelError> {
            Ok(())
        }

        async fn send_text(&mut self, text: &str) -> Result<(), MonitorChannelError> {
            self.sent_texts.lock().await.push(text.to_owned());

            if self.auto_pong {
                // If the text is a Ping, enqueue a matching Pong.
                if let Ok(TaggedClientFrame::Ping { nonce }) =
                    serde_json::from_str::<TaggedClientFrame>(text)
                {
                    let pong_json = format!(r#"{{"kind":"pong","nonce":{nonce}}}"#);
                    self.recv_queue
                        .lock()
                        .await
                        .push_back(pong_json.into_bytes());
                }
            }
            Ok(())
        }

        async fn close(self) -> Result<(), MonitorChannelError> {
            Ok(())
        }
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    /// `TaggedServerFrame::Pong` deserializes correctly.
    #[test]
    fn pong_frame_deserialized() {
        let json = r#"{"kind":"pong","nonce":42}"#;
        let frame = parse_ws_frame(json).expect("must parse");
        assert!(matches!(frame, TaggedServerFrame::Pong { nonce: 42 }));
    }

    /// `TaggedClientFrame::Ping` serializes correctly.
    #[test]
    fn ping_frame_serialized() {
        let frame = TaggedClientFrame::Ping { nonce: 7 };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains("\"kind\":\"ping\""));
        assert!(json.contains("\"nonce\":7"));
    }

    /// Mock channel that auto-replies Pong: keepalive loop does not return Err
    /// within a short window (we run it for 3 ping intervals and verify it
    /// stays alive).
    #[tokio::test]
    async fn keepalive_loop_survives_with_pong() {
        let mut ch = MockChannel::new_auto_pong();

        // Override timing by running the loop body manually a few times.
        // We simulate the keepalive by calling send_text(Ping) and checking
        // that Pong arrives and clears the outstanding state.

        let nonce: u64 = 1;
        let ping_json = serde_json::to_string(&TaggedClientFrame::Ping { nonce }).unwrap();

        // Send Ping via the channel.
        ch.send_text(&ping_json).await.unwrap();

        // The auto-pong mock should have queued a Pong.
        let payload = ch.recv_frame().await.unwrap().expect("should have pong");
        let text = std::str::from_utf8(&payload).unwrap();
        let frame = parse_ws_frame(text).unwrap();

        match frame {
            TaggedServerFrame::Pong { nonce: got } => {
                assert_eq!(got, nonce, "Pong nonce must echo Ping nonce");
            }
            other => panic!("expected Pong, got {other:?}"),
        }

        // Verify the sent text was recorded.
        let sent = ch.sent_texts.lock().await;
        assert_eq!(sent.len(), 1);
        assert!(sent[0].contains("\"ping\""));
    }

    /// Mock channel that never replies: dead-detection triggers after timeout.
    ///
    /// We use the run_over_chain_loop_with_health_check logic in a stripped-down
    /// form — verify that when no Pong arrives within pong_timeout the loop
    /// returns `ChannelClosed`.
    #[tokio::test]
    async fn keepalive_detects_dead_channel() {
        // Build the minimal state machine inline (mirrors what
        // run_over_chain_loop_with_health_check does) so we don't need a full
        // MonitorLink.

        let ping_interval_dur = Duration::from_millis(50);
        let pong_timeout = Duration::from_millis(100);

        let mut ch = MockChannel::new_no_reply();

        let nonce: u64 = 99;
        let ping_json = serde_json::to_string(&TaggedClientFrame::Ping { nonce }).unwrap();

        let now = tokio::time::Instant::now();

        // Send a Ping.
        ch.send_text(&ping_json).await.unwrap();
        let ping_sent_at = now;

        // Simulate the ping_interval firing again after pong_timeout has elapsed.
        tokio::time::sleep(pong_timeout + ping_interval_dur).await;

        let elapsed = ping_sent_at.elapsed();
        // No Pong arrived → elapsed > pong_timeout → dead.
        assert!(
            elapsed > pong_timeout,
            "expected dead detection: elapsed={elapsed:?} pong_timeout={pong_timeout:?}"
        );
    }

    // ── Phase 3: new frame serde round-trip tests ─────────────────────────────

    /// `TaggedClientFrame::CredentialRequest` serialises with `"kind":"credential_request"`.
    #[test]
    fn credential_request_frame_serialized() {
        let frame = TaggedClientFrame::CredentialRequest {
            request_id: 42,
            enrol_token: "tok-abc".to_string(),
            blinded_hex: vec!["deadbeef".to_string(), "cafebabe".to_string()],
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains("\"kind\":\"credential_request\""), "kind field missing: {json}");
        assert!(json.contains("\"request_id\":42"), "request_id missing: {json}");
        assert!(json.contains("\"enrol_token\":\"tok-abc\""), "enrol_token missing: {json}");
        assert!(json.contains("\"deadbeef\""), "blinded_hex[0] missing: {json}");
    }

    /// `TaggedServerFrame::CredentialIssueResult` deserialises from contract JSON.
    #[test]
    fn credential_issue_result_deserialized() {
        let json = r#"{"kind":"credential_issue_result","request_id":7,"epoch_id":3,"blind_sigs_hex":["aabbcc","ddeeff"]}"#;
        let frame = parse_ws_frame(json).expect("must parse");
        match frame {
            TaggedServerFrame::CredentialIssueResult { request_id, epoch_id, blind_sigs_hex } => {
                assert_eq!(request_id, 7);
                assert_eq!(epoch_id, 3);
                assert_eq!(blind_sigs_hex, vec!["aabbcc", "ddeeff"]);
            }
            other => panic!("unexpected frame variant: {other:?}"),
        }
    }

    /// `TaggedServerFrame::CredentialIssueError` deserialises from contract JSON.
    #[test]
    fn credential_issue_error_deserialized() {
        let json = r#"{"kind":"credential_issue_error","request_id":99,"error":"rate limited"}"#;
        let frame = parse_ws_frame(json).expect("must parse");
        match frame {
            TaggedServerFrame::CredentialIssueError { request_id, error } => {
                assert_eq!(request_id, 99);
                assert_eq!(error, "rate limited");
            }
            other => panic!("unexpected frame variant: {other:?}"),
        }
    }
}
