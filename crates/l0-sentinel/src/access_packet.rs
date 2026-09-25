//! The access packet — K2's delivery unit.
//!
//! `docs/mirage/plans/client-track-checklist-2026-09-17.md` K2: a paying
//! subscriber must go from ONE artefact to carried bytes without a
//! hand-authored `sentinel.toml` or a hand-rendered `client-config.json`.
//! This module owns that artefact's shape, its two encodings (JSON on disk,
//! base64url — no padding — of the compact JSON for the QR form), and the
//! one-shot apply that seeds every piece of state a fresh sentinel would
//! otherwise need a human to type in.
//!
//! **What the packet must never contain**, and this is a product rule, not a
//! style preference: no node id, no peer-table row, no monitor beyond the
//! subscriber's own entrance, no fleet address. [`AccessPacket::monitor`] is
//! a single struct field (not a list) — a packet literally cannot carry a
//! second monitor without this type changing. There is nothing else in this
//! shape a node id or a peer-table row could ride in on; keeping it that way
//! on every future field is the enforcement.
//!
//! Three steps, always in this order: [`AccessPacket::from_json`] /
//! [`AccessPacket::from_qr`] parse; [`AccessPacket::validate`] refuses a
//! malformed packet BY NAME before anything is touched; [`AccessPacket::apply`]
//! writes state exactly once (a repeat apply of the same bytes is a no-op —
//! see its doc comment). `apply` calls `validate` itself first, so a caller
//! that forgets the explicit step still gets "never a partial apply".

use std::path::{Path, PathBuf};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::chain_request;
use crate::monitor_wire;

/// The only packet version this sentinel understands.
pub const CURRENT_VERSION: u32 = 1;

/// The only [`AccessPacketV2`] version this sentinel understands. A
/// separate namespace from [`CURRENT_VERSION`] — mirrors
/// `mirage2panel::mesh_access_packet::MeshAccessPacket::CURRENT_VERSION`'s
/// own doc: the two packet shapes are versioned independently, and this
/// crate keeps its own copy of the v2 number rather than sharing one with
/// v1's. The wire value is `2` (not `1`) — v2 packets self-identify by this
/// field as well as by shape.
pub const CURRENT_VERSION_V2: u32 = 2;

/// Filename (under `state_dir`) for the default, unconsumed access packet —
/// the entry point used when the sentinel is started with no `--access`
/// flag.
const DEFAULT_FILE: &str = "access.json";

/// Filename (under `state_dir`) recording the fingerprint of the last
/// successfully applied packet. See [`AccessPacket::apply`].
const CONSUMED_FILE: &str = "access-packet-consumed.txt";

/// Filename (under `state_dir`) for pre-minted SignedCapability (`0x02`)
/// tokens written by [`AccessPacket::apply`]. The materialised `ClientConfig`
/// always points `capability_tokens_path` at this same file (`lib.rs::run`),
/// whether or not it exists yet — an absent file is simply an empty source
/// (see `client-core::capability_pool::FileCapabilityTokenSource`).
const CAPABILITY_TOKENS_FILE: &str = "capability-tokens.txt";

/// Filename (under `state_dir`) for a persisted `credential_params` seed.
/// See [`load_persisted_credential_params`].
const CREDENTIAL_PARAMS_SEED_FILE: &str = "credential-params-seed.json";

/// Filename (under `state_dir`) for the v2 device identity — see
/// [`AccessPacketV2::apply`]. Holds private key material, so it is written
/// with owner-only permissions ([`write_owner_only_atomic`]) and must NEVER
/// be logged.
const DEVICE_IDENTITY_FILE: &str = "sentinel-device-identity.json";

/// Expected decoded length, in bytes, of every plain 32-byte hex key/token
/// field in the packet (`client_token`, `monitor.wire_pubkey_hex`,
/// `operator_pub_hex`) — 64 hex characters.
const HEX32_BYTES: usize = 32;

/// The full path a fresh sentinel consumes when no `--access <path>` flag
/// was given.
pub fn default_path(state_dir: &Path) -> PathBuf {
    state_dir.join(DEFAULT_FILE)
}

fn consumed_marker_path(state_dir: &Path) -> PathBuf {
    state_dir.join(CONSUMED_FILE)
}

/// Path of the pre-minted capability-token file — shared by [`AccessPacket::apply`]
/// (writer) and `lib.rs::run` (which always points the materialised
/// `ClientConfig.capability_tokens_path` here).
pub fn capability_tokens_path(state_dir: &Path) -> PathBuf {
    state_dir.join(CAPABILITY_TOKENS_FILE)
}

fn credential_params_seed_path(state_dir: &Path) -> PathBuf {
    state_dir.join(CREDENTIAL_PARAMS_SEED_FILE)
}

/// The full path a v2 device identity is persisted to. See
/// [`AccessPacketV2::apply`] / [`load_device_identity`].
pub fn device_identity_path(state_dir: &Path) -> PathBuf {
    state_dir.join(DEVICE_IDENTITY_FILE)
}

/// Load the device identity [`AccessPacketV2::apply`] persisted, if any.
/// `lib.rs::run` calls this once, on every boot, to decide whether this
/// sentinel registers via identity (v2) or anonymously (v1) — mirrors
/// [`load_persisted_credential_params`]'s own "read the cache, not the
/// original delivery file" shape.
pub fn load_device_identity(state_dir: &Path) -> Option<AccessPacketV2> {
    let raw = std::fs::read_to_string(device_identity_path(state_dir)).ok()?;
    match serde_json::from_str(&raw) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(
                "[sentinel] {} parse failed: {e} — ignoring",
                DEVICE_IDENTITY_FILE
            );
            None
        }
    }
}

/// Write `content` to `path` atomically (tmp + rename), same discipline as
/// [`crate::write_atomic`], plus owner-only permissions on the tmp file
/// before the rename (unix `0o600`; on Windows this relies on the default
/// per-user-profile ACL of `state_dir`, matching the precedent in
/// `mlc-updater::token_store::save_token`). Used only for files that carry
/// private key material — never log the bytes passed here.
fn write_owner_only_atomic(path: &Path, content: &str) -> Result<(), AccessPacketError> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content)
        .map_err(|e| AccessPacketError::Io(format!("write {:?}: {e}", tmp)))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        if let Err(e) = std::fs::set_permissions(&tmp, perms) {
            tracing::warn!("[sentinel] chmod 0600 on {:?} failed: {e}", tmp);
        }
    }
    std::fs::rename(&tmp, path)
        .map_err(|e| AccessPacketError::Io(format!("rename {:?} -> {:?}: {e}", tmp, path)))?;
    Ok(())
}

/// Load the `credential_params` seed [`AccessPacket::apply`] persisted, if
/// any. Called once by `lib.rs::run` before `MonitorLink` is constructed, so
/// the very first materialise — before any live `CredentialParams` push
/// arrives — already carries operator-signed params. Without this a worker
/// with no params presents scheme `0x00` and every exit refuses it while the
/// log reports its token source as configured (the defect this closes,
/// 2026-09-16). A live push still wins unconditionally: `MonitorLink::
/// apply_credential_params` overwrites the same `ArcSwap` this seeds.
pub fn load_persisted_credential_params(
    state_dir: &Path,
) -> Option<mirage_core::crypto::credential::CredentialParams> {
    let raw = std::fs::read_to_string(credential_params_seed_path(state_dir)).ok()?;
    match serde_json::from_str(&raw) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(
                "[sentinel] {} parse failed: {e} — ignoring",
                CREDENTIAL_PARAMS_SEED_FILE
            );
            None
        }
    }
}

/// The subscriber's own ingress — exactly one monitor, never a list. See the
/// module doc for why that shape (not the field's presence) is the
/// enforcement against a packet mapping the mesh.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessPacketMonitor {
    /// `host:port` of the monitor's Mirage wire listener.
    pub wire_addr: String,
    /// 64-character hex of the monitor's static X25519 public key.
    pub wire_pubkey_hex: String,
}

/// The K2 delivery unit — see the module doc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessPacket {
    /// Packet format version. Only [`CURRENT_VERSION`] is accepted.
    pub v: u32,
    pub mesh_id: String,
    pub client_id: String,
    /// Ownership proof the subscriber's sentinel presents on re-registration
    /// — see `chain_request::persist_client_token`.
    pub client_token: String,
    pub monitor: AccessPacketMonitor,
    /// 64-character hex Ed25519 operator verifying key — every pushed
    /// `CredentialParams` frame is checked against it before being applied.
    pub operator_pub_hex: String,
    /// The operator-signed `mirage_core::crypto::credential::CredentialParams`
    /// — reused verbatim, never restated. OPTIONAL, and its absence is a
    /// real state a packet can be in (e.g. an unenforced pilot mesh); when
    /// present it is what lets the worker present a credential without ever
    /// needing its own monitor link. Its absence cost this line an hour on
    /// 2026-09-16: a worker with no params presents scheme `0x00` and every
    /// exit refuses it while the log cheerfully reports its token source as
    /// configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_params: Option<mirage_core::crypto::credential::CredentialParams>,
    /// Pre-minted `0x02` SignedCapability tokens (hex, one per entry),
    /// OPTIONAL and pilot-only: the `0x02` relay replaces this later, and
    /// once it does this field is expected to be absent. [`AccessPacket::apply`]
    /// writes them into the file `ClientConfig.capability_tokens_path` names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capability_tokens: Vec<String>,
    pub issued_at_ms: u64,
    /// A human tag for logs. It must NEVER carry customer identity — it is
    /// not validated as one and must not be treated as one anywhere it is
    /// read.
    pub label: String,
}

/// Every way [`AccessPacket::from_json`], [`AccessPacket::from_qr`],
/// [`AccessPacket::validate`] and [`AccessPacket::apply`] refuse — by name,
/// never a panic.
#[derive(Debug, Error)]
pub enum AccessPacketError {
    #[error("access packet: unsupported version {found} (this sentinel understands {expected})")]
    UnsupportedVersion { found: u32, expected: u32 },

    #[error("access packet: mesh_id is empty")]
    EmptyMeshId,

    #[error("access packet: field '{field}' is not valid hex: {reason}")]
    MalformedHex { field: String, reason: String },

    #[error(
        "access packet: field '{field}' must decode to {expected_bytes} bytes, got {got_bytes}"
    )]
    WrongHexLength { field: String, expected_bytes: usize, got_bytes: usize },

    #[error("access packet: monitor.wire_addr {addr:?} is not host:port")]
    BadMonitorAddr { addr: String },

    #[error("access packet: invalid JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("access packet: QR payload is not valid base64url: {0}")]
    Base64(#[from] base64::DecodeError),

    #[error("access packet: QR payload decodes to invalid UTF-8: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),

    #[error("access packet: {0}")]
    Io(String),

    #[error("access packet apply: {0}")]
    Apply(String),

    // ── v2-specific refusals ────────────────────────────────────────────

    #[error("access packet v2: self_id {self_id:?} is not a valid UUID: {reason}")]
    BadSelfId { self_id: String, reason: String },

    #[error(
        "access packet v2: self_id {packet_self_id} does not match declared_identity.self_id {identity_self_id}"
    )]
    SelfIdMismatch { packet_self_id: String, identity_self_id: String },

    #[error("access packet v2: declared_identity.roles {roles:?} does not include Sentinel")]
    RoleNotSentinel { roles: Vec<String> },

    #[error(
        "access packet v2: mesh_id {packet_mesh_id} does not match declared_identity.mesh_id {identity_mesh_id}"
    )]
    MeshMismatch { packet_mesh_id: String, identity_mesh_id: String },

    #[error("access packet v2: monitor.self_id is empty")]
    EmptyMonitorSelfId,

    #[error("access packet v2: field '{field}' does not match its corresponding private key")]
    KeyMismatch { field: String },

    #[error("access packet: top-level value is not a JSON object")]
    NotAnObject,

    #[error(
        "access packet: ambiguous shape — carries both v1's 'client_token' and v2's 'declared_identity'"
    )]
    AmbiguousVersion,

    #[error(
        "access packet: unrecognised shape — carries neither v1's 'client_token' nor v2's 'declared_identity'"
    )]
    UnknownVersion,
}

fn check_hex_field(field: &str, value: &str, expected_bytes: usize) -> Result<Vec<u8>, AccessPacketError> {
    let bytes = hex::decode(value).map_err(|e| AccessPacketError::MalformedHex {
        field: field.to_owned(),
        reason: e.to_string(),
    })?;
    if bytes.len() != expected_bytes {
        return Err(AccessPacketError::WrongHexLength {
            field: field.to_owned(),
            expected_bytes,
            got_bytes: bytes.len(),
        });
    }
    Ok(bytes)
}

/// Convert an already-length-checked [`check_hex_field`] result into a fixed
/// 32-byte array. `check_hex_field` already guarantees `v.len() ==
/// HEX32_BYTES` on `Ok`, so the conversion cannot fail in practice — but the
/// fallible path is kept (rather than an `expect`) so a future caller that
/// forgets the length precondition gets a named refusal, never a panic.
fn to_array32(v: Vec<u8>, field: &str) -> Result<[u8; 32], AccessPacketError> {
    v.try_into().map_err(|got: Vec<u8>| AccessPacketError::WrongHexLength {
        field: field.to_owned(),
        expected_bytes: HEX32_BYTES,
        got_bytes: got.len(),
    })
}

/// Which `AccessPacket` shape a `DeliveryKind::Packet` file (per
/// `entrance_artefact::detect_kind`) carries — decided purely by which
/// discriminating field is present, mirroring `entrance_artefact::
/// detect_kind`'s own technique: v2 carries an operator-signed
/// `declared_identity` and no `client_token`; v1 carries `client_token` and
/// no `declared_identity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketVersion {
    V1,
    V2,
}

/// Shape-sniff a `Packet`-kind delivery file to tell v1 from v2 — a cheap,
/// generic pre-parse (`serde_json::Value`) purely to pick which typed parser
/// to call next, exactly like `entrance_artefact::detect_kind`'s own role for
/// `Artefact` vs `Packet`.
pub fn detect_packet_version(raw: &str) -> Result<PacketVersion, AccessPacketError> {
    let value: serde_json::Value = serde_json::from_str(raw)?;
    let obj = value.as_object().ok_or(AccessPacketError::NotAnObject)?;
    match (obj.contains_key("declared_identity"), obj.contains_key("client_token")) {
        (true, false) => Ok(PacketVersion::V2),
        (false, true) => Ok(PacketVersion::V1),
        (true, true) => Err(AccessPacketError::AmbiguousVersion),
        (false, false) => Err(AccessPacketError::UnknownVersion),
    }
}

/// `true` for a plain `host:port` pair — non-empty host, port parses as a
/// `u16`. Good enough for the `ip:port` shape every monitor wire address
/// actually is; not an IPv6-bracket parser.
fn looks_like_host_port(s: &str) -> bool {
    match s.rsplit_once(':') {
        Some((host, port)) => !host.is_empty() && port.parse::<u16>().is_ok(),
        None => false,
    }
}

impl AccessPacket {
    /// Parse a packet from its JSON form. Does NOT validate — call
    /// [`AccessPacket::validate`] (or rely on [`AccessPacket::apply`], which
    /// validates first).
    pub fn from_json(raw: &str) -> Result<Self, AccessPacketError> {
        Ok(serde_json::from_str(raw)?)
    }

    /// Compact JSON form (no pretty-printing) — the plaintext the QR form
    /// base64url-encodes.
    pub fn to_json(&self) -> Result<String, AccessPacketError> {
        Ok(serde_json::to_string(self)?)
    }

    /// Parse a packet from its QR form: base64url (no padding) of the
    /// compact JSON encoding.
    pub fn from_qr(qr: &str) -> Result<Self, AccessPacketError> {
        let bytes = URL_SAFE_NO_PAD.decode(qr.trim())?;
        let raw = String::from_utf8(bytes)?;
        Self::from_json(&raw)
    }

    /// Encode this packet into its QR form: base64url (no padding) of the
    /// compact JSON encoding.
    pub fn to_qr(&self) -> Result<String, AccessPacketError> {
        Ok(URL_SAFE_NO_PAD.encode(self.to_json()?))
    }

    /// Refuse a malformed packet BY NAME. Never partially applies anything —
    /// this only inspects `self`.
    pub fn validate(&self) -> Result<(), AccessPacketError> {
        if self.v != CURRENT_VERSION {
            return Err(AccessPacketError::UnsupportedVersion {
                found: self.v,
                expected: CURRENT_VERSION,
            });
        }
        if self.mesh_id.trim().is_empty() {
            return Err(AccessPacketError::EmptyMeshId);
        }
        check_hex_field("client_token", &self.client_token, HEX32_BYTES)?;
        check_hex_field("monitor.wire_pubkey_hex", &self.monitor.wire_pubkey_hex, HEX32_BYTES)?;
        check_hex_field("operator_pub_hex", &self.operator_pub_hex, HEX32_BYTES)?;
        for (i, tok) in self.capability_tokens.iter().enumerate() {
            check_hex_field(
                &format!("capability_tokens[{i}]"),
                tok,
                mirage_core::crypto::credential::signed_capability::SIGNED_CAP_TOKEN_LEN,
            )?;
        }
        if !looks_like_host_port(&self.monitor.wire_addr) {
            return Err(AccessPacketError::BadMonitorAddr { addr: self.monitor.wire_addr.clone() });
        }
        Ok(())
    }

    /// The fingerprint recorded in [`CONSUMED_FILE`] on a successful apply:
    /// `issued_at_ms` plus a BLAKE3 hash of the packet's canonical (compact
    /// JSON) bytes. Two packets that hash the same are the same packet.
    fn consumed_fingerprint(&self) -> Result<String, AccessPacketError> {
        let canonical = serde_json::to_vec(self)?;
        Ok(format!(
            "{}:{}",
            self.issued_at_ms,
            hex::encode(mirage_core::crypto::hash_blake3(&canonical))
        ))
    }

    /// Apply this packet to `state_dir`, exactly once.
    ///
    /// Validates first — a malformed packet never touches disk. Then:
    /// 1. stores `client_token` in the per-monitor token store
    ///    (`chain_request::persist_client_token`), keyed exactly as
    ///    `WireRegisterCoords::token_key` keys it;
    /// 2. persists `client_id` (`chain_request::persist_client_id`);
    /// 3. persists the monitor wire coordinates (`monitor_wire::save`) — the
    ///    cache `lib.rs::run` falls back to when `sentinel.toml` carries no
    ///    static `monitor_wire_addr`;
    /// 4. writes `capability_tokens` (if any) to [`capability_tokens_path`];
    /// 5. persists `credential_params` (if present) so `lib.rs::run` can seed
    ///    `MonitorLink`'s live `ArcSwap` before construction.
    ///
    /// A repeat call with byte-identical content (same `issued_at_ms` +
    /// content hash) is a no-op with one INFO line — a restart must not
    /// re-apply stale material over state that has since moved on (e.g. a
    /// dropped stale token after a 401). A packet with DIFFERENT content
    /// (a fresh issuance, even at an earlier `issued_at_ms`) applies again.
    pub fn apply(&self, state_dir: &Path) -> Result<(), AccessPacketError> {
        self.validate()?;

        std::fs::create_dir_all(state_dir)
            .map_err(|e| AccessPacketError::Io(format!("create state_dir {}: {e}", state_dir.display())))?;

        let fingerprint = self.consumed_fingerprint()?;
        let consumed_path = consumed_marker_path(state_dir);
        if std::fs::read_to_string(&consumed_path).ok().as_deref() == Some(fingerprint.as_str()) {
            tracing::info!(
                issued_at_ms = self.issued_at_ms,
                label = %self.label,
                "[sentinel] access packet already consumed — no-op"
            );
            return Ok(());
        }

        // 1) Per-monitor client_token store, keyed EXACTLY as chain_request
        //    keys it (monitor_pubkey_hex when present, else monitor_addr —
        //    see WireRegisterCoords::token_key).
        let wire_coords_for_key = chain_request::WireRegisterCoords {
            monitor_addr: &self.monitor.wire_addr,
            monitor_pubkey_hex: &self.monitor.wire_pubkey_hex,
            mesh_id: &self.mesh_id,
        };
        chain_request::persist_client_token(
            state_dir,
            wire_coords_for_key.token_key(),
            &self.client_token,
        )
        .map_err(|e| AccessPacketError::Apply(format!("persist_client_token: {e}")))?;

        // 2) client_id — two files, and both are needed.
        //
        //    `persist_client_id` writes `client-id.txt`, which is what the
        //    sentinel reports about itself. But the id actually SENT on
        //    `POST /relay/clients` comes from `sentinel-id.txt`, and on a fresh
        //    box `ensure_sentinel_id` mints a UUID of its own. A box that mints
        //    its own id registers as somebody the issuer never heard of: the
        //    monitor sees a first registration of an unknown id, mints a token
        //    for it, and the token THIS packet carries belongs to a different
        //    row. Everything after that fails in a way that reads like a
        //    credential problem and is an identity problem — so the packet's id
        //    is adopted as the sentinel id too.
        chain_request::persist_client_id(state_dir, &self.client_id)
            .map_err(|e| AccessPacketError::Apply(format!("persist_client_id: {e}")))?;
        crate::sentinel_id::adopt_sentinel_id(state_dir, &self.client_id)
            .map_err(|e| AccessPacketError::Apply(format!("adopt_sentinel_id: {e}")))?;

        // 3) Monitor wire coordinates.
        let coords = monitor_wire::WireCoords {
            monitor_addr: self.monitor.wire_addr.clone(),
            monitor_pubkey_hex: self.monitor.wire_pubkey_hex.clone(),
            mesh_id: self.mesh_id.clone(),
        };
        monitor_wire::save(state_dir, &coords)
            .map_err(|e| AccessPacketError::Apply(format!("monitor_wire::save: {e}")))?;

        // 4) Pre-minted 0x02 capability tokens (optional, pilot-only).
        if self.capability_tokens.is_empty() {
            tracing::info!("[sentinel] access packet: no capability_tokens — nothing to write");
        } else {
            let mut body = String::new();
            for tok in &self.capability_tokens {
                body.push_str(tok);
                body.push('\n');
            }
            crate::write_atomic(&capability_tokens_path(state_dir), &body)
                .map_err(|e| AccessPacketError::Apply(format!("write capability tokens: {e}")))?;
            tracing::info!(
                count = self.capability_tokens.len(),
                "[sentinel] access packet: wrote capability tokens"
            );
        }

        // 5) credential_params (optional) — seeded for the materialise path;
        //    see load_persisted_credential_params.
        match &self.credential_params {
            Some(cp) => {
                let body = serde_json::to_string(cp)?;
                crate::write_atomic(&credential_params_seed_path(state_dir), &body).map_err(|e| {
                    AccessPacketError::Apply(format!("write credential_params seed: {e}"))
                })?;
                tracing::info!("[sentinel] access packet: credential_params present — seeded");
            }
            None => {
                tracing::info!(
                    "[sentinel] access packet: no credential_params — applying without one"
                );
            }
        }

        crate::write_atomic(&consumed_path, &fingerprint)
            .map_err(|e| AccessPacketError::Apply(format!("record consumed: {e}")))?;

        tracing::info!(
            client_id = %self.client_id,
            mesh_id = %self.mesh_id,
            label = %self.label,
            "[sentinel] access packet applied"
        );
        Ok(())
    }
}

// ── AccessPacket v2 ──────────────────────────────────────────────────────
//
// Carries an operator-signed Sentinel `DeclaredIdentity` and this device's
// own X25519 (+ Ed25519 signing) keys, minted by mirage2panel's mode-3
// private-mesh issuance (`mirage2panel::mesh_access_packet::MeshAccessPacket`
// — the panel-side type this mirrors field-for-field). Unlike v1, a v2
// packet never carries a `client_token`: the sentinel registers by proving
// possession of the identity's own signing key
// (`chain_request::register_sentinel_v2`), and the monitor mints the token
// on that call, exactly as `mirage2monitor`'s existing `/relay/clients/edge`
// route does for an Edge box (`clients_api::register_edge_client`).

/// The monitor a v2 packet's Sentinel dials — adds `self_id` (the monitor's
/// own operator-declared identifier) alongside the two fields
/// [`AccessPacketMonitor`] carries. `self_id` is bound into
/// `chain_request::sentinel_register_signing_bytes`'s preimage so a
/// possession proof minted for one monitor cannot be replayed against a
/// different monitor in the same mesh (mirrors the reasoning
/// `mirage2monitor::clients_api::edge_register_signing_bytes`'s own doc
/// gives for its `monitor_self_id` parameter).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessPacketV2Monitor {
    /// `host:port` of the monitor's Mirage wire listener.
    pub wire_addr: String,
    /// 64-character hex of the monitor's static X25519 public key — what
    /// [`AccessPacketV2`]'s registration dial PINS the wire connection to.
    pub wire_pubkey_hex: String,
    /// The monitor's own operator-declared identifier — carried into the
    /// possession-sig preimage, never used to pin the wire dial (that is
    /// `wire_pubkey_hex`'s job).
    pub self_id: String,
}

/// The K2 v2 delivery unit — see this section's own doc for how it differs
/// from [`AccessPacket`] (v1).
///
/// A custom [`std::fmt::Debug`] impl (below) redacts both private-key
/// fields: this type MUST NEVER be logged whole — see [`AccessPacketV2::apply`]
/// for the one place it is persisted (owner-only permissions) and the only
/// fields that are safe to log individually.
#[derive(Clone, Serialize, Deserialize)]
pub struct AccessPacketV2 {
    /// Packet format version. Only [`CURRENT_VERSION_V2`] is accepted — a
    /// separate namespace from v1's [`CURRENT_VERSION`].
    pub v: u32,
    /// The FLEET mesh id this device joins — never a personal
    /// per-subscriber identifier.
    pub mesh_id: String,
    /// The Sentinel's own identifier, minted by the panel — carried
    /// unchanged into [`Self::declared_identity`]'s own `self_id`. Checked
    /// for equality with it in [`Self::validate`].
    pub self_id: String,
    /// 64-hex X25519 static secret — this device's persistent identity key,
    /// presented on the wire hello of every registration dial (never an
    /// ephemeral key — see `chain_request::register_sentinel_v2`).
    pub client_private_key_hex: String,
    /// The public half of [`Self::client_private_key_hex`].
    pub client_pubkey_hex: String,
    /// 64-hex Ed25519 signing secret — always present in v2 (unlike the
    /// panel's own optional field on older issuance paths): a v2 packet has
    /// no way to register without one, since `possession_sig` is signed with
    /// it.
    pub client_signing_private_key_hex: String,
    /// The public half of [`Self::client_signing_private_key_hex`].
    pub client_signing_pubkey_hex: String,
    /// opbox's signed `DeclaredIdentity` envelope for this device's
    /// `ServiceRole::Sentinel` role. Unlike the panel's own module (which
    /// keeps this opaque to avoid a `mirage-core` dependency it otherwise
    /// has no use for), this crate already depends on `mirage-core` for
    /// several other types, so this rides typed rather than as
    /// `serde_json::Value` — [`Self::validate`] inspects it directly, and it
    /// is sent verbatim as the `identity` field of
    /// `chain_request::SentinelRegisterBody`.
    pub declared_identity: mirage_core::identity::DeclaredIdentity,
    pub monitor: AccessPacketV2Monitor,
    /// The operator-signed `CredentialParams` this device presents, if the
    /// mesh enforces one. `None` is a real state (an unenforced pilot mesh),
    /// not an omission — same as v1's own field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_params: Option<mirage_core::crypto::credential::CredentialParams>,
    pub issued_at_ms: u64,
    /// A human tag for logs. Same rule as v1's own field: NEVER customer
    /// identity.
    pub label: String,
}

impl std::fmt::Debug for AccessPacketV2 {
    /// Redacts [`Self::client_private_key_hex`] and
    /// [`Self::client_signing_private_key_hex`] — every other field is
    /// printed normally. This is what makes "never logged" a property of the
    /// TYPE rather than only of code-review discipline at every call site.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessPacketV2")
            .field("v", &self.v)
            .field("mesh_id", &self.mesh_id)
            .field("self_id", &self.self_id)
            .field("client_private_key_hex", &"<redacted>")
            .field("client_pubkey_hex", &self.client_pubkey_hex)
            .field("client_signing_private_key_hex", &"<redacted>")
            .field("client_signing_pubkey_hex", &self.client_signing_pubkey_hex)
            .field("declared_identity", &self.declared_identity)
            .field("monitor", &self.monitor)
            .field("credential_params", &self.credential_params)
            .field("issued_at_ms", &self.issued_at_ms)
            .field("label", &self.label)
            .finish()
    }
}

impl AccessPacketV2 {
    /// Parse a v2 packet from its JSON form. Does NOT validate — call
    /// [`Self::validate`] (or rely on [`Self::apply`], which validates
    /// first).
    pub fn from_json(raw: &str) -> Result<Self, AccessPacketError> {
        Ok(serde_json::from_str(raw)?)
    }

    /// Refuse a malformed v2 packet BY NAME. Never partially applies
    /// anything — this only inspects `self`. Checks, in order: version,
    /// mesh_id shape, `self_id` is a UUID matching
    /// `declared_identity.self_id`, `declared_identity` names the Sentinel
    /// role, `declared_identity.mesh_id` matches `self.mesh_id`, every hex
    /// field's shape, `monitor.wire_addr` shape, `monitor.self_id`
    /// non-empty, and finally that both key pairs (X25519 identity, Ed25519
    /// signing) are actually pairs — the public half really is derived from
    /// the private half.
    ///
    /// Deliberately does NOT verify `declared_identity.operator_sig` — this
    /// crate has no pinned operator key at packet-apply time (unlike
    /// `credential_params`, which IS verified downstream by `MonitorLink`
    /// against `SentinelConfig::operator_pub_hex`). The signature is the
    /// MONITOR's job at registration time, exactly as it is for the
    /// existing `/relay/clients/edge` route
    /// (`mirage2monitor::clients_api::verify_edge_client_registration`).
    pub fn validate(&self) -> Result<(), AccessPacketError> {
        if self.v != CURRENT_VERSION_V2 {
            return Err(AccessPacketError::UnsupportedVersion {
                found: self.v,
                expected: CURRENT_VERSION_V2,
            });
        }
        if self.mesh_id.trim().is_empty() {
            return Err(AccessPacketError::EmptyMeshId);
        }

        let self_id = uuid::Uuid::parse_str(&self.self_id).map_err(|e| AccessPacketError::BadSelfId {
            self_id: self.self_id.clone(),
            reason: e.to_string(),
        })?;
        if self_id != self.declared_identity.self_id {
            return Err(AccessPacketError::SelfIdMismatch {
                packet_self_id: self.self_id.clone(),
                identity_self_id: self.declared_identity.self_id.to_string(),
            });
        }
        if !self.declared_identity.roles.contains(&mirage_core::identity::ServiceRole::Sentinel) {
            return Err(AccessPacketError::RoleNotSentinel {
                roles: self.declared_identity.roles.iter().map(|r| r.as_tag().to_string()).collect(),
            });
        }
        if self.declared_identity.mesh_id.as_str() != self.mesh_id {
            return Err(AccessPacketError::MeshMismatch {
                packet_mesh_id: self.mesh_id.clone(),
                identity_mesh_id: self.declared_identity.mesh_id.as_str().to_string(),
            });
        }

        let priv_bytes = check_hex_field("client_private_key_hex", &self.client_private_key_hex, HEX32_BYTES)?;
        let pub_bytes = check_hex_field("client_pubkey_hex", &self.client_pubkey_hex, HEX32_BYTES)?;
        let signing_priv_bytes = check_hex_field(
            "client_signing_private_key_hex",
            &self.client_signing_private_key_hex,
            HEX32_BYTES,
        )?;
        let signing_pub_bytes = check_hex_field(
            "client_signing_pubkey_hex",
            &self.client_signing_pubkey_hex,
            HEX32_BYTES,
        )?;
        check_hex_field("monitor.wire_pubkey_hex", &self.monitor.wire_pubkey_hex, HEX32_BYTES)?;
        if !looks_like_host_port(&self.monitor.wire_addr) {
            return Err(AccessPacketError::BadMonitorAddr { addr: self.monitor.wire_addr.clone() });
        }
        if self.monitor.self_id.trim().is_empty() {
            return Err(AccessPacketError::EmptyMonitorSelfId);
        }

        // X25519 identity key: pubkey must be derived from the private key.
        let priv_arr = to_array32(priv_bytes, "client_private_key_hex")?;
        let pub_arr = to_array32(pub_bytes, "client_pubkey_hex")?;
        let derived_pub = x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(priv_arr));
        if derived_pub.to_bytes() != pub_arr {
            return Err(AccessPacketError::KeyMismatch { field: "client_pubkey_hex".to_owned() });
        }

        // Ed25519 signing key: same check.
        let signing_priv_arr = to_array32(signing_priv_bytes, "client_signing_private_key_hex")?;
        let signing_pub_arr = to_array32(signing_pub_bytes, "client_signing_pubkey_hex")?;
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&signing_priv_arr);
        if signing_key.verifying_key().to_bytes() != signing_pub_arr {
            return Err(AccessPacketError::KeyMismatch { field: "client_signing_pubkey_hex".to_owned() });
        }

        Ok(())
    }

    /// Apply this packet to `state_dir`. Validates first — a malformed
    /// packet never touches disk. Unlike v1's [`AccessPacket::apply`], this
    /// is naturally idempotent with no "consumed" dedup marker needed: every
    /// field it writes is either a deterministic function of `self`
    /// (rewriting the same content is a no-op in effect) or lives in a
    /// SEPARATE file this fn never touches — the actual `client_token` a
    /// registration mints, which `chain_request::register_sentinel_v2`
    /// persists on ITS OWN, not here.
    ///
    /// Writes, in order:
    /// 1. monitor wire coordinates (`monitor_wire::save`) — the SAME cache
    ///    v1 populates, so `lib.rs::run`'s existing wire_coords resolution
    ///    needs no v2-specific plumbing;
    /// 2. `credential_params` (if present) to the SAME seed file v1 uses
    ///    (`load_persisted_credential_params` reads it back unchanged);
    /// 3. `client_id`/`sentinel_id` forced to `self.self_id` — a v2 device's
    ///    `client_id` on registration success is ALWAYS
    ///    `declared_identity.self_id`, mirroring the monitor's own Edge
    ///    registration rule ("never a freshly minted or caller-chosen id");
    /// 4. the device identity itself (this whole struct, private keys
    ///    included) to [`device_identity_path`], with owner-only permissions
    ///    ([`write_owner_only_atomic`]). NEVER logged — see this fn's own
    ///    final INFO line for the only fields that are safe to log.
    pub fn apply(&self, state_dir: &Path) -> Result<(), AccessPacketError> {
        self.validate()?;

        std::fs::create_dir_all(state_dir)
            .map_err(|e| AccessPacketError::Io(format!("create state_dir {}: {e}", state_dir.display())))?;

        let coords = monitor_wire::WireCoords {
            monitor_addr: self.monitor.wire_addr.clone(),
            monitor_pubkey_hex: self.monitor.wire_pubkey_hex.clone(),
            mesh_id: self.mesh_id.clone(),
        };
        monitor_wire::save(state_dir, &coords)
            .map_err(|e| AccessPacketError::Apply(format!("monitor_wire::save: {e}")))?;

        match &self.credential_params {
            Some(cp) => {
                let body = serde_json::to_string(cp)?;
                crate::write_atomic(&credential_params_seed_path(state_dir), &body).map_err(|e| {
                    AccessPacketError::Apply(format!("write credential_params seed: {e}"))
                })?;
                tracing::info!("[sentinel] access packet v2: credential_params present — seeded");
            }
            None => {
                tracing::info!(
                    "[sentinel] access packet v2: no credential_params — applying without one"
                );
            }
        }

        chain_request::persist_client_id(state_dir, &self.self_id)
            .map_err(|e| AccessPacketError::Apply(format!("persist_client_id: {e}")))?;
        crate::sentinel_id::adopt_sentinel_id(state_dir, &self.self_id)
            .map_err(|e| AccessPacketError::Apply(format!("adopt_sentinel_id: {e}")))?;

        let body = serde_json::to_string_pretty(self)?;
        write_owner_only_atomic(&device_identity_path(state_dir), &body)?;

        tracing::info!(
            self_id = %self.self_id,
            mesh_id = %self.mesh_id,
            label = %self.label,
            "[sentinel] access packet v2 applied — device identity persisted"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mirage_core::crypto::credential::{CredentialParams, EpochEntry};
    use mirage_core::mirage::wire::CredentialScheme;

    fn sample_packet() -> AccessPacket {
        AccessPacket {
            v: CURRENT_VERSION,
            mesh_id: "MESH2".to_owned(),
            client_id: "c0ffee00-0000-4000-8000-000000000001".to_owned(),
            client_token: "22".repeat(HEX32_BYTES),
            monitor: AccessPacketMonitor {
                wire_addr: "103.231.75.144:19444".to_owned(),
                wire_pubkey_hex: "11".repeat(HEX32_BYTES),
            },
            operator_pub_hex: "33".repeat(HEX32_BYTES),
            credential_params: None,
            capability_tokens: Vec::new(),
            issued_at_ms: 1_726_000_000_000,
            label: "pilot-1".to_owned(),
        }
    }

    fn sample_credential_params() -> CredentialParams {
        CredentialParams::single(
            1,
            CredentialScheme::SignedCapability,
            vec![EpochEntry::with_secret(1, u64::MAX, [0u8; 32])],
        )
    }

    // ── parse / encode round-trips ───────────────────────────────────────

    #[test]
    fn round_trips_through_json() {
        let mut p = sample_packet();
        p.credential_params = Some(sample_credential_params());
        let json = p.to_json().unwrap();
        let back = AccessPacket::from_json(&json).unwrap();
        assert_eq!(serde_json::to_value(&p).unwrap(), serde_json::to_value(&back).unwrap());
    }

    #[test]
    fn round_trips_through_qr() {
        let mut p = sample_packet();
        p.capability_tokens = vec!["aa".repeat(100), "bb".repeat(100)];
        let qr = p.to_qr().unwrap();
        // base64url, no padding: none of the standard-alphabet-only chars appear.
        assert!(!qr.contains('=') && !qr.contains('+') && !qr.contains('/'));
        let back = AccessPacket::from_qr(&qr).unwrap();
        assert_eq!(serde_json::to_value(&p).unwrap(), serde_json::to_value(&back).unwrap());
    }

    #[test]
    fn from_qr_refuses_bad_base64() {
        assert!(matches!(
            AccessPacket::from_qr("not base64url!!"),
            Err(AccessPacketError::Base64(_))
        ));
    }

    // ── validate refusals, by name ────────────────────────────────────────

    #[test]
    fn valid_packet_passes() {
        assert!(sample_packet().validate().is_ok());
    }

    #[test]
    fn validate_refuses_unknown_version() {
        let mut p = sample_packet();
        p.v = 2;
        assert!(matches!(
            p.validate(),
            Err(AccessPacketError::UnsupportedVersion { found: 2, expected: 1 })
        ));
    }

    #[test]
    fn validate_refuses_empty_mesh_id() {
        let mut p = sample_packet();
        p.mesh_id = "  ".to_owned();
        assert!(matches!(p.validate(), Err(AccessPacketError::EmptyMeshId)));
    }

    #[test]
    fn validate_refuses_malformed_client_token_hex() {
        let mut p = sample_packet();
        p.client_token = "not-hex".to_owned();
        match p.validate() {
            Err(AccessPacketError::MalformedHex { field, .. }) => assert_eq!(field, "client_token"),
            other => panic!("expected MalformedHex, got {other:?}"),
        }
    }

    #[test]
    fn validate_refuses_wrong_length_wire_pubkey_hex() {
        let mut p = sample_packet();
        p.monitor.wire_pubkey_hex = "aa".repeat(10); // valid hex, wrong length
        match p.validate() {
            Err(AccessPacketError::WrongHexLength { field, expected_bytes, got_bytes }) => {
                assert_eq!(field, "monitor.wire_pubkey_hex");
                assert_eq!(expected_bytes, HEX32_BYTES);
                assert_eq!(got_bytes, 10);
            }
            other => panic!("expected WrongHexLength, got {other:?}"),
        }
    }

    #[test]
    fn validate_refuses_malformed_operator_pub_hex() {
        let mut p = sample_packet();
        p.operator_pub_hex = "zz".repeat(HEX32_BYTES); // not hex at all
        match p.validate() {
            Err(AccessPacketError::MalformedHex { field, .. }) => assert_eq!(field, "operator_pub_hex"),
            other => panic!("expected MalformedHex, got {other:?}"),
        }
    }

    #[test]
    fn validate_refuses_wrong_length_capability_token() {
        let mut p = sample_packet();
        p.capability_tokens = vec!["aa".repeat(10)];
        match p.validate() {
            Err(AccessPacketError::WrongHexLength { field, .. }) => {
                assert_eq!(field, "capability_tokens[0]")
            }
            other => panic!("expected WrongHexLength, got {other:?}"),
        }
    }

    #[test]
    fn validate_refuses_monitor_addr_without_port() {
        let mut p = sample_packet();
        p.monitor.wire_addr = "not-an-addr".to_owned();
        assert!(matches!(p.validate(), Err(AccessPacketError::BadMonitorAddr { .. })));
    }

    // ── apply ────────────────────────────────────────────────────────────

    #[test]
    fn apply_refuses_an_invalid_packet_without_touching_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = sample_packet();
        p.mesh_id = String::new();
        assert!(p.apply(dir.path()).is_err());
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none(), "no file must be written");
    }

    #[test]
    fn apply_stores_client_token_under_the_monitor_pubkey_key() {
        let dir = tempfile::tempdir().unwrap();
        let p = sample_packet();
        p.apply(dir.path()).unwrap();
        assert_eq!(
            chain_request::load_client_token(dir.path(), &p.monitor.wire_pubkey_hex),
            Some(p.client_token.clone())
        );
    }

    #[test]
    fn apply_persists_client_id_and_monitor_wire_coords() {
        let dir = tempfile::tempdir().unwrap();
        let p = sample_packet();
        p.apply(dir.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("client-id.txt")).unwrap(),
            p.client_id
        );
        let coords = monitor_wire::load(dir.path()).expect("monitor_wire.json must be written");
        assert_eq!(coords.monitor_addr, p.monitor.wire_addr);
        assert_eq!(coords.monitor_pubkey_hex, p.monitor.wire_pubkey_hex);
        assert_eq!(coords.mesh_id, p.mesh_id);
    }

    #[test]
    fn apply_with_capability_tokens_writes_exactly_those_lines() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = sample_packet();
        let t1 = "aa".repeat(100);
        let t2 = "bb".repeat(100);
        p.capability_tokens = vec![t1.clone(), t2.clone()];
        p.apply(dir.path()).unwrap();
        let raw = std::fs::read_to_string(capability_tokens_path(dir.path())).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines, vec![t1.as_str(), t2.as_str()]);
    }

    #[test]
    fn apply_without_capability_tokens_leaves_the_file_absent() {
        let dir = tempfile::tempdir().unwrap();
        let p = sample_packet();
        p.apply(dir.path()).unwrap();
        assert!(!capability_tokens_path(dir.path()).exists());
    }

    #[test]
    fn apply_without_credential_params_still_applies_and_leaves_no_seed() {
        let dir = tempfile::tempdir().unwrap();
        let p = sample_packet();
        assert!(p.apply(dir.path()).is_ok());
        assert!(!credential_params_seed_path(dir.path()).exists());
        assert!(load_persisted_credential_params(dir.path()).is_none());
    }

    #[test]
    fn apply_with_credential_params_seeds_them_for_materialise() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = sample_packet();
        p.credential_params = Some(sample_credential_params());
        p.apply(dir.path()).unwrap();
        let seeded =
            load_persisted_credential_params(dir.path()).expect("credential_params must be seeded");
        assert_eq!(seeded.generation, 1);
    }

    #[test]
    fn apply_is_a_no_op_the_second_time() {
        let dir = tempfile::tempdir().unwrap();
        let p = sample_packet();
        p.apply(dir.path()).unwrap();

        // Simulate state that moved on after the first apply (e.g. the
        // stale-token drop on a 401) — the second apply of the SAME packet
        // must not clobber it.
        chain_request::persist_client_token(dir.path(), &p.monitor.wire_pubkey_hex, "moved-on")
            .unwrap();

        p.apply(dir.path()).unwrap();

        assert_eq!(
            chain_request::load_client_token(dir.path(), &p.monitor.wire_pubkey_hex),
            Some("moved-on".to_owned()),
            "a re-applied stale packet must not overwrite state that has since moved on"
        );
    }

    #[test]
    fn apply_of_a_changed_packet_is_not_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = sample_packet();
        p1.apply(dir.path()).unwrap();

        let mut p2 = sample_packet();
        p2.client_token = "44".repeat(HEX32_BYTES);
        p2.apply(dir.path()).unwrap();

        assert_eq!(
            chain_request::load_client_token(dir.path(), &p2.monitor.wire_pubkey_hex),
            Some(p2.client_token.clone()),
            "a genuinely re-issued packet must apply, not be treated as a repeat"
        );
    }
}
