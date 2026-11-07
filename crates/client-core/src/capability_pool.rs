//! Client-held provider of pre-minted SignedCapability (scheme `0x02`) tokens.
//!
//! The `0x02` fleet is asymmetric: only the operator-box (Ed25519 seed) can
//! mint tokens, so the client can never self-mint — it must RECEIVE pre-minted
//! 100-byte tokens from outside. [`CapabilityTokenSource`] is the replaceable
//! seam between "where tokens come from" and "where they are spent": the dial
//! sites in `pool.rs` / `mux_pool.rs` only see the trait, so the current file
//! source (test line) can later be swapped for a monitor-relay source without
//! touching the spending path.
//!
//! Tokens are strictly single-use: the node records each presented nonce in
//! its SpentSet and rejects replays. A popped token is NEVER re-inserted —
//! even when the dial that popped it fails, the token may already have
//! reached the node (same rule as the blind pool — see `token_pool::pop_one`).

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use mirage_core::{
    crypto::credential::{signed_capability::SIGNED_CAP_TOKEN_LEN, CredentialParams},
    mirage::wire::CredentialScheme,
};
use mirage_protocol::driver::ClientCredential;
use tracing::warn;

/// Shared handle passed to the pools.  `Arc<dyn …>` (not a concrete type) so
/// the source stays a replaceable seam — the file source below is the test
/// line; a monitor-relay source implements the same trait later.
pub type CapabilityTokenCell = Arc<dyn CapabilityTokenSource>;

/// Source of pre-minted SignedCapability (`0x02`) tokens.
///
/// Implementations must guarantee single-use: one token is handed to at most
/// one caller, and a handed-out token is never presented again.
pub trait CapabilityTokenSource: Send + Sync {
    /// True when at least one unspent token is available right now.
    fn is_ready(&self) -> bool;
    /// Take one token, or `None` when the source is empty.
    fn pop_one(&self) -> Option<ClientCredential>;
}

/// Source used when `capability_tokens_path` is not configured: always empty.
/// A dial under a `SignedCapability` authority then refuses to proceed
/// credential-less (see [`capability_credential_for_dial`]).
pub struct NullCapabilityTokenSource;

impl CapabilityTokenSource for NullCapabilityTokenSource {
    fn is_ready(&self) -> bool {
        false
    }
    fn pop_one(&self) -> Option<ClientCredential> {
        None
    }
}

/// Convenience constructor for the not-configured case.
pub fn null_capability_cell() -> CapabilityTokenCell {
    Arc::new(NullCapabilityTokenSource)
}

// ── FileCapabilityTokenSource ─────────────────────────────────────────────────

/// File-backed source: one hex line = one 100-byte `0x02` token.
///
/// Test-line provisioning: some external process appends fresh tokens to the
/// file; every pop lazily RE-READS the file (freshness without a watcher),
/// takes the first valid token, and rewrites the file with the remaining
/// lines. Blank lines and `#` comments are ignored (and survive rewrites);
/// lines that fail hex decoding or are not exactly [`SIGNED_CAP_TOKEN_LEN`]
/// bytes are dropped with a `warn!` on the next rewrite.
///
/// Known, accepted race: a concurrent external append between our read and
/// our rewrite is lost.  Tolerated because this source is a test line — the
/// production source (monitor relay) will not share a file with its writer.
/// Pops are serialised by the internal mutex, so two dials never receive the
/// same token.
pub struct FileCapabilityTokenSource {
    path: PathBuf,
    /// Serialises read-parse-rewrite so concurrent pops stay single-use.
    mu: Mutex<()>,
}

impl FileCapabilityTokenSource {
    /// Create a source reading tokens from `path`.  The file does not need to
    /// exist yet — an absent file is simply an empty source (the cold-start
    /// credential gate waits for tokens to appear).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), mu: Mutex::new(()) }
    }

    /// Parse one line into a token.  Returns `Ok(None)` for blank lines and
    /// `#` comments, `Err(())` (after logging) for malformed token lines.
    fn parse_line(line: &str) -> Result<Option<[u8; SIGNED_CAP_TOKEN_LEN]>, ()> {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            return Ok(None);
        }
        let bytes = hex::decode(trimmed).map_err(|e| {
            warn!(
                token_len = trimmed.len(),
                "capability_pool: failed to hex-decode token line, dropping it: {e}"
            );
        })?;
        let token: [u8; SIGNED_CAP_TOKEN_LEN] = bytes.as_slice().try_into().map_err(|_| {
            warn!(
                token_len = bytes.len(),
                expected = SIGNED_CAP_TOKEN_LEN,
                "capability_pool: token line has wrong length, dropping it"
            );
        })?;
        Ok(Some(token))
    }
}

impl CapabilityTokenSource for FileCapabilityTokenSource {
    fn is_ready(&self) -> bool {
        let _guard = self.mu.lock().unwrap_or_else(|e| e.into_inner());
        let content = match std::fs::read_to_string(&self.path) {
            Ok(c) => c,
            // Absent/unreadable file = empty source; pop will warn with detail.
            Err(_) => return false,
        };
        content
            .lines()
            .any(|line| matches!(Self::parse_line(line), Ok(Some(_))))
    }

    fn pop_one(&self) -> Option<ClientCredential> {
        let _guard = self.mu.lock().unwrap_or_else(|e| e.into_inner());
        let content = match std::fs::read_to_string(&self.path) {
            Ok(c) => c,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    warn!(
                        path = %self.path.display(),
                        "capability_pool: failed to read token file: {e}"
                    );
                }
                return None;
            }
        };

        // First valid line is spent; comments and blank lines are kept,
        // malformed lines are dropped (they were already warned about).
        let mut taken: Option<[u8; SIGNED_CAP_TOKEN_LEN]> = None;
        let mut kept: Vec<&str> = Vec::new();
        for line in content.lines() {
            match Self::parse_line(line) {
                Ok(Some(token)) if taken.is_none() => taken = Some(token),
                Ok(Some(_)) => kept.push(line),
                Ok(None) => kept.push(line),
                Err(()) => {} // malformed — drop from the rewrite
            }
        }

        let token = taken?;
        // Rewrite BEFORE handing the token out: if the rewrite fails the token
        // would still be in the file and could be popped twice — violating
        // single-use — so a failed rewrite forfeits the token instead.
        let mut out = kept.join("\n");
        if !out.is_empty() {
            out.push('\n');
        }
        if let Err(e) = std::fs::write(&self.path, out) {
            warn!(
                path = %self.path.display(),
                "capability_pool: failed to rewrite token file, token NOT spent: {e}"
            );
            return None;
        }
        Some(ClientCredential {
            scheme: CredentialScheme::SignedCapability.as_u8(),
            token: token.to_vec(),
        })
    }
}

// ── Dial-time selection ───────────────────────────────────────────────────────

/// True when `params` advertises a SignedCapability (`0x02`) authority — the
/// signal that dials must present a pre-minted capability token.
pub fn has_capability_authority(params: Option<&CredentialParams>) -> bool {
    params.is_some_and(|p| {
        p.authorities
            .iter()
            .any(|a| a.scheme == CredentialScheme::SignedCapability)
    })
}

/// True when `params` advertises a RoleSigned (`0x05`) authority.
///
/// RoleSigned is an EDGE-BOX credential: the box-side edge presents its own
/// operator-signed Edge identity to an exit. A client in public-mesh or
/// dedicated-edge mode knows only its edge's address and never holds — and
/// must never present — this credential (owner ruling 2026-09-24). If a
/// monitor ever declares it as a client-facing epoch scheme, that is a
/// server-side misconfiguration; the client refuses the dial rather than
/// sending no credential or panicking. See [`role_signed_refusal_error`].
pub fn has_role_signed_authority(params: Option<&CredentialParams>) -> bool {
    params.is_some_and(|p| {
        p.authorities
            .iter()
            .any(|a| a.scheme == CredentialScheme::RoleSigned)
    })
}

/// Outcome of asking the capability provider for a dial credential.
pub(crate) enum CapabilityDial {
    /// No SignedCapability authority in params — the caller falls back to the
    /// existing ring (0x04) → blind (0x03) selection, unchanged.
    NotActive,
    /// A token was popped; present it as the dial's `pre_minted` credential.
    Token(ClientCredential),
    /// A SignedCapability authority IS active but the provider is empty.
    /// The dial must NOT proceed credential-less: a 0x02-enforcing node would
    /// reject it (CREDENTIAL_REJECTED) and the spent-less dial only costs the
    /// node a handshake. The caller surfaces this as a dial error so the
    /// slot's reconnect/backoff retries once tokens are provisioned.
    Unavailable,
    /// A RoleSigned (`0x05`) authority is active — an edge-box-only
    /// credential a client can never hold or mint. The dial must NOT proceed
    /// credential-less and must NOT panic; the caller surfaces this as a
    /// named refusal (see [`role_signed_refusal_error`]) instead of
    /// attempting a ring/blind/capability selection that would never apply.
    RoleSignedRefused,
}

/// Select the `0x02` pre-minted credential for one dial.
///
/// Priority is driven by the authorities in `params`: RoleSigned (`0x05`)
/// wins first — a client refuses outright, since it is edge-box-only and the
/// client can never present it. Otherwise, when a SignedCapability authority
/// is present, the dial presents a capability token and the ring / blind
/// pools are left untouched (a 0x03/0x04 token would be rejected by a 0x02
/// fleet anyway, and popping one would waste it).
pub(crate) fn capability_credential_for_dial(
    params: Option<&CredentialParams>,
    capability: &CapabilityTokenCell,
) -> CapabilityDial {
    if has_role_signed_authority(params) {
        return CapabilityDial::RoleSignedRefused;
    }
    if !has_capability_authority(params) {
        return CapabilityDial::NotActive;
    }
    match capability.pop_one() {
        Some(cred) => CapabilityDial::Token(cred),
        None => CapabilityDial::Unavailable,
    }
}

/// Error surfaced when a dial cannot proceed under [`CapabilityDial::Unavailable`].
/// Deliberately does NOT contain "CREDENTIAL_REJECTED" (no rejection happened —
/// the dial never went out), so it follows the ordinary reconnect backoff
/// rather than the credential-transient path.
pub(crate) fn capability_unavailable_error() -> mirage_core::error::MirageError {
    mirage_core::error::MirageError::Protocol(
        "capability token source empty while a SignedCapability authority is active — \
         dial withheld (never dial credential-less under 0x02)"
            .to_string(),
    )
}

/// Error surfaced when a dial cannot proceed under
/// [`CapabilityDial::RoleSignedRefused`].
///
/// Named so a caller (and its logs) can tell this apart from an ordinary
/// transient credential shortage: this scheme can never become available to
/// a client, no matter how long it waits.
pub(crate) fn role_signed_refusal_error() -> mirage_core::error::MirageError {
    mirage_core::error::MirageError::Protocol(
        "role_signed is an edge-box credential; a client cannot present it".to_string(),
    )
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use mirage_core::crypto::credential::EpochEntry;
    use mirage_core::mirage::wire::CredentialScheme;
    use std::path::Path;

    /// Hex line for a token whose every byte is `fill`.
    fn token_line(fill: u8) -> String {
        hex::encode([fill; SIGNED_CAP_TOKEN_LEN])
    }

    /// Unique temp file path (no external tempdir dep in this crate).
    fn temp_file(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "mirage-capability-pool-test-{tag}-{}-{nanos}.txt",
            std::process::id()
        ))
    }

    fn file_source_with(content: &str, tag: &str) -> (PathBuf, FileCapabilityTokenSource) {
        let path = temp_file(tag);
        std::fs::write(&path, content).expect("write token file");
        let src = FileCapabilityTokenSource::new(&path);
        (path, src)
    }

    fn read_lines(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .expect("read back token file")
            .lines()
            .map(|l| l.to_string())
            .collect()
    }

    // ── loading / parsing ─────────────────────────────────────────────────────

    /// Blank lines and `#` comments are ignored; a valid token line loads.
    #[test]
    fn capability_load_ignores_blank_and_comment_lines() {
        let content = format!(
            "# test capability tokens\n\n{}\n   \n",
            token_line(0x11)
        );
        let (path, src) = file_source_with(&content, "load");
        assert!(src.is_ready(), "file with one valid token must be ready");
        let _ = std::fs::remove_file(&path);
    }

    /// Invalid hex and wrong-length lines are skipped, not treated as tokens.
    #[test]
    fn capability_invalid_lines_are_skipped() {
        let content = format!(
            "not-valid-hex!!!\n{}\n{}\n",
            hex::encode([0x22u8; 50]), // valid hex, wrong length
            token_line(0x33)
        );
        let (path, src) = file_source_with(&content, "invalid");
        let cred = src.pop_one().expect("the valid line must pop");
        assert_eq!(cred.scheme, CredentialScheme::SignedCapability.as_u8());
        assert_eq!(cred.token, vec![0x33u8; SIGNED_CAP_TOKEN_LEN]);
        // Malformed lines were dropped by the rewrite — nothing left to spend.
        assert!(!src.is_ready());
        let _ = std::fs::remove_file(&path);
    }

    // ── pop single-use semantics ──────────────────────────────────────────────

    /// Two pops yield two DIFFERENT tokens; the third pop returns None.
    /// Catches a regression where pop hands out the same token twice.
    #[test]
    fn capability_pop_is_single_use() {
        let content = format!("{}\n{}\n", token_line(0xAA), token_line(0xBB));
        let (path, src) = file_source_with(&content, "single-use");

        let c1 = src.pop_one().expect("first pop");
        let c2 = src.pop_one().expect("second pop");
        assert_ne!(c1.token, c2.token, "each pop must hand out a distinct token");
        assert_eq!(c1.token, vec![0xAAu8; SIGNED_CAP_TOKEN_LEN]);
        assert_eq!(c2.token, vec![0xBBu8; SIGNED_CAP_TOKEN_LEN]);
        assert!(src.pop_one().is_none(), "third pop must return None");
        let _ = std::fs::remove_file(&path);
    }

    /// After a pop the file carries only the remaining (unspent) tokens.
    #[test]
    fn capability_pop_rewrites_file_with_remainder() {
        let content = format!(
            "# header comment\n{}\n{}\n",
            token_line(0x01),
            token_line(0x02)
        );
        let (path, src) = file_source_with(&content, "rewrite");
        let _ = src.pop_one().expect("pop must succeed");

        let lines = read_lines(&path);
        assert!(
            !lines.contains(&token_line(0x01)),
            "spent token must be gone from the file: {lines:?}"
        );
        assert!(
            lines.contains(&token_line(0x02)),
            "unspent token must survive the rewrite: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.starts_with('#')),
            "comment lines must survive the rewrite: {lines:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A second pop consumes the token the first rewrite left behind — never
    /// the already-spent one (the practical single-use regression).
    #[test]
    fn capability_second_pop_consumes_remainder_not_spent_token() {
        let content = format!("{}\n{}\n", token_line(0x0A), token_line(0x0B));
        let (path, src) = file_source_with(&content, "second-pop");
        let c1 = src.pop_one().expect("first pop");
        let c2 = src.pop_one().expect("second pop");
        assert_eq!(c1.token, vec![0x0Au8; SIGNED_CAP_TOKEN_LEN]);
        assert_eq!(
            c2.token,
            vec![0x0Bu8; SIGNED_CAP_TOKEN_LEN],
            "second pop must return the remaining token, not replay the spent one"
        );
        let _ = std::fs::remove_file(&path);
    }

    // ── readiness ─────────────────────────────────────────────────────────────

    #[test]
    fn capability_readiness_empty_vs_nonempty() {
        let (path, src) = file_source_with("", "ready-empty");
        assert!(!src.is_ready(), "empty file must not be ready");

        std::fs::write(&path, format!("{}\n", token_line(0x77))).expect("append token");
        assert!(src.is_ready(), "file with a token must be ready");

        // Missing file = not ready (cold start before provisioning).
        let missing = temp_file("ready-missing");
        let missing_src = FileCapabilityTokenSource::new(&missing);
        assert!(!missing_src.is_ready());
        assert!(missing_src.pop_one().is_none());
        let _ = std::fs::remove_file(&path);
    }

    // ── dial-time selection ───────────────────────────────────────────────────

    fn params_with_scheme(scheme: CredentialScheme) -> CredentialParams {
        CredentialParams::single(1, scheme, vec![EpochEntry::with_secret(1, u64::MAX, [0u8; 32])])
    }

    /// With a SignedCapability authority in params, the dial selection pops a
    /// 0x02 token from the provider.
    #[test]
    fn capability_selected_when_authority_active() {
        let content = format!("{}\n", token_line(0x42));
        let (path, src) = file_source_with(&content, "select");
        let cell: CapabilityTokenCell = Arc::new(src);
        let params = params_with_scheme(CredentialScheme::SignedCapability);

        match capability_credential_for_dial(Some(&params), &cell) {
            CapabilityDial::Token(cred) => {
                assert_eq!(cred.scheme, 0x02);
                assert_eq!(cred.token.len(), SIGNED_CAP_TOKEN_LEN);
            }
            _ => panic!("0x02 authority + non-empty provider must yield a token"),
        }
        let _ = std::fs::remove_file(&path);
    }

    /// Without a SignedCapability authority the selection steps aside so the
    /// ring → blind fallback runs unchanged.
    #[test]
    fn capability_not_selected_without_authority() {
        let cell = null_capability_cell();
        let params = params_with_scheme(CredentialScheme::BlindRsaBatch);
        assert!(matches!(
            capability_credential_for_dial(Some(&params), &cell),
            CapabilityDial::NotActive
        ));
        assert!(matches!(
            capability_credential_for_dial(None, &cell),
            CapabilityDial::NotActive
        ));
    }

    /// 0x02 authority + empty provider = Unavailable (the dial must be
    /// withheld, not sent credential-less).
    #[test]
    fn capability_unavailable_when_authority_active_but_empty() {
        let cell = null_capability_cell();
        let params = params_with_scheme(CredentialScheme::SignedCapability);
        assert!(matches!(
            capability_credential_for_dial(Some(&params), &cell),
            CapabilityDial::Unavailable
        ));
        // The synthetic error must NOT masquerade as a node-side rejection.
        let err = capability_unavailable_error();
        assert!(!crate::pool::is_credential_rejected(&err));
    }

    // ── RoleSigned (0x05) refusal ────────────────────────────────────────────

    /// `has_role_signed_authority` is `true` only when a RoleSigned authority
    /// is present, `false` for every other scheme and for absent params.
    #[test]
    fn role_signed_authority_detected() {
        assert!(has_role_signed_authority(Some(&params_with_scheme(
            CredentialScheme::RoleSigned
        ))));
        assert!(!has_role_signed_authority(Some(&params_with_scheme(
            CredentialScheme::SignedCapability
        ))));
        assert!(!has_role_signed_authority(None));
    }

    /// A RoleSigned authority in params refuses the dial outright — never a
    /// token, never `NotActive` (which would fall through to the ring/blind
    /// selection and could dial credential-less).
    #[test]
    fn role_signed_authority_refuses_dial() {
        let cell = null_capability_cell();
        let params = params_with_scheme(CredentialScheme::RoleSigned);
        assert!(matches!(
            capability_credential_for_dial(Some(&params), &cell),
            CapabilityDial::RoleSignedRefused
        ));
    }

    /// RoleSigned wins over a SignedCapability provider that DOES hold a
    /// token — the refusal is unconditional, not a fallback for emptiness.
    #[test]
    fn role_signed_refuses_even_with_capability_tokens_available() {
        let content = format!("{}\n", token_line(0x99));
        let (path, src) = file_source_with(&content, "role-signed-with-tokens");
        let cell: CapabilityTokenCell = Arc::new(src);
        let params = params_with_scheme(CredentialScheme::RoleSigned);
        assert!(matches!(
            capability_credential_for_dial(Some(&params), &cell),
            CapabilityDial::RoleSignedRefused
        ));
        let _ = std::fs::remove_file(&path);
    }

    /// The refusal error names the scheme and never claims a node-side
    /// rejection (no dial ever went out).
    #[test]
    fn role_signed_refusal_error_is_named_and_not_a_rejection() {
        let err = role_signed_refusal_error();
        let msg = err.to_string();
        assert!(msg.contains("role_signed"), "error must name the scheme: {msg}");
        assert!(!crate::pool::is_credential_rejected(&err));
    }
}
