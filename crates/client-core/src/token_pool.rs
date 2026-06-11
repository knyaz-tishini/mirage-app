//! Client-held pool of unblinded blind-RSA tokens ready for spend.
//!
//! [`TokenPool`] is a value type stored behind an `Arc<ArcSwap<TokenPool>>`
//! on [`MonitorLink`].  The monitor's blind-relay path fills it; the spending
//! path (Phase 4) drains it.  Both sides hold an `Arc::clone` of the same
//! ArcSwap so they observe the same generation atomically.

use std::sync::Arc;

use arc_swap::ArcSwap;
use mirage_core::mirage_blindtoken as bt;
use tracing::warn;

// ── StoredToken ───────────────────────────────────────────────────────────────

/// A single unblinded, verified blind-RSA token ready for presentation.
///
/// Fields match the wire layout for `BlindRsaBatch` tokens:
/// `[4B epoch_id BE | 32B msg | 32B randomizer | 256B sig]` (324 bytes total).
#[derive(Clone)]
pub struct StoredToken {
    /// Epoch this token was issued under (4-byte wire field).
    pub epoch_id: u32,
    /// The 32-byte random message blinded + unblinded on the client.
    pub msg: [u8; 32],
    /// The 32-byte randomizer produced by [`bt::unblind`].
    pub randomizer: [u8; bt::RANDOMIZER_LEN],
    /// The 256-byte final RSA signature produced by [`bt::unblind`].
    pub sig: [u8; bt::SIG_LEN],
}

impl StoredToken {
    /// Assemble the 324-byte `BlindRsaBatch` (scheme `0x03`) wire token.
    ///
    /// Wire layout: `[4B epoch_id BE | 32B msg | 32B randomizer | 256B sig]`.
    /// This is exactly what the node's `verify_credential` expects for scheme `0x03`.
    pub fn to_wire(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(4 + 32 + 32 + 256);
        v.extend_from_slice(&self.epoch_id.to_be_bytes());
        v.extend_from_slice(&self.msg);
        v.extend_from_slice(&self.randomizer);
        v.extend_from_slice(&self.sig);
        v
    }
}

// ── TokenPool ─────────────────────────────────────────────────────────────────

/// A flat pool of unspent [`StoredToken`]s.
///
/// Always stored behind `Arc<arc_swap::ArcSwap<TokenPool>>`.  Writers use
/// `rcu` to swap in an updated clone; readers call `load()` for a snapshot.
#[derive(Clone, Default)]
pub struct TokenPool {
    pub tokens: Vec<StoredToken>,
}

impl TokenPool {
    /// Number of tokens currently in the pool.
    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    /// True when the pool holds no tokens.
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// Append a single token.
    pub fn push(&mut self, token: StoredToken) {
        self.tokens.push(token);
    }

    /// Append a batch of tokens.
    pub fn extend(&mut self, iter: impl IntoIterator<Item = StoredToken>) {
        self.tokens.extend(iter);
    }
}

// ── pop_one ───────────────────────────────────────────────────────────────────

/// Atomically remove one token from the pool via compare-and-swap retry.
///
/// Returns `None` when the pool is empty.  Each token is handed to at most one
/// caller — the CAS loop ensures that concurrent dials never receive the same
/// token.  A successful dial may still fail at the node (network error, node
/// restarted, etc.).  **Do not re-insert on dial failure**: the token may have
/// reached the node's SpentSet, and re-insertion would risk a replay-reject on
/// the retried dial.  Treat every popped token as consumed.
pub fn pop_one(pool: &ArcSwap<TokenPool>) -> Option<StoredToken> {
    loop {
        let cur = pool.load();
        if cur.tokens.is_empty() {
            return None;
        }
        let mut next = (**cur).clone();
        // pop() removes from the back; order doesn't matter for spend semantics.
        let token = next.tokens.pop().expect("non-empty checked above");
        let prev = pool.compare_and_swap(&*cur, Arc::new(next));
        if Arc::ptr_eq(&prev, &*cur) {
            // We won the CAS — token is exclusively ours.
            return Some(token);
        }
        // Another dial popped concurrently — reload and retry.
    }
}

// ── absorb_issue_result ───────────────────────────────────────────────────────

/// Process a credential-issue result from the monitor.
///
/// For each `(msg, client_blind, blind_sig_hex)` triple: hex-decode the sig,
/// call [`bt::unblind`] to produce the final token, and return all
/// [`StoredToken`]s that succeeded.  Entries that fail to decode or unblind
/// are skipped with a `warn!` — a partial batch is still useful.
///
/// This is factored out of the receive loop so it can be unit-tested without
/// any network machinery.
pub fn absorb_issue_result(
    pk: &bt::IssuerPublicKey,
    retained: Vec<([u8; bt::MSG_LEN], bt::ClientBlind)>,
    blind_sigs_hex: &[String],
    epoch_id: u32,
) -> Vec<StoredToken> {
    let mut out = Vec::with_capacity(retained.len());

    let pairs = retained.into_iter().zip(blind_sigs_hex.iter());
    for ((msg, client_blind), sig_hex) in pairs {
        // Hex-decode the blind signature.
        let sig_bytes = match hex::decode(sig_hex) {
            Ok(b) => b,
            Err(e) => {
                warn!(epoch_id, "token_pool: failed to hex-decode blind_sig: {e}");
                continue;
            }
        };

        // Unblind into a final (sig, randomizer) pair.
        match bt::unblind(pk, &sig_bytes, &client_blind, &msg) {
            Ok((sig, randomizer)) => {
                out.push(StoredToken { epoch_id, msg, randomizer, sig });
            }
            Err(e) => {
                warn!(epoch_id, "token_pool: unblind failed: {e}");
            }
        }
    }

    out
}

// ── RingCredential (scheme 0x04) ──────────────────────────────────────────────

/// Static ring-membership credential material (scheme 0x04). The member secret
/// + Merkle path + the epoch_id whose tree the path matches are fixed for the
/// process lifetime; only `next_nf` mutates. At dial time the client derives a
/// fresh proof from this WHEN a RingMembership authority is currently published
/// in the gossiped credential params.
///
/// `next_nf` is a monotonic per-process nullifier-index counter. Every dial
/// (initial connection AND each reconnect) consumes the next index so no two
/// live dials present the same nullifier (which the node SpentSet would reject
/// as a replay). The epoch's published `max_dials` bounds it; once exhausted the
/// member must wait for an epoch rotation. Shared across the whole connection
/// pool via the enclosing `Arc<Option<RingCredential>>`.
pub struct RingCredential {
    pub member_sk: mirage_ringmember::MemberSecret,
    pub path: Vec<mirage_ringmember::PathStep>,
    pub epoch_id: u32,
    pub next_nf: std::sync::atomic::AtomicU32,
}

impl RingCredential {
    /// Construct from decoded material; the nullifier counter starts at 0.
    pub fn new(
        member_sk: mirage_ringmember::MemberSecret,
        path: Vec<mirage_ringmember::PathStep>,
        epoch_id: u32,
    ) -> Self {
        Self { member_sk, path, epoch_id, next_nf: std::sync::atomic::AtomicU32::new(0) }
    }
}

/// Build a 0x04 ring-membership credential IFF a RingMembership authority is
/// currently published in `params` AND the client holds ring material AND the
/// member's per-epoch dial budget is not exhausted. Returns `None` otherwise —
/// caller then falls back to the blind 0x03 token / EpochHmac.
///
/// Each call consumes the next nullifier index from `ring.next_nf`, so a pool of
/// N connections (and their reconnects) presents N distinct nullifiers rather
/// than replaying one. `max_dials` is read from the epoch's published
/// `verify_material` (root || max_dials_le); a bare 32-byte root means strict
/// single-use (1).
pub(crate) fn build_ring_credential(
    params: Option<&mirage_core::crypto::credential::CredentialParams>,
    ring: Option<&RingCredential>,
) -> Option<mirage_protocol::driver::ClientCredential> {
    let ring = ring?;
    let params = params?;
    // Find the live RingMembership authority and the epoch whose tree this
    // member's path was issued against.
    let authority = params
        .authorities
        .iter()
        .find(|a| a.scheme == mirage_core::mirage::wire::CredentialScheme::RingMembership)?;
    let entry = authority.epochs.iter().find(|e| e.epoch_id == ring.epoch_id)?;
    let vm = &entry.verify_material;
    let max_dials = if vm.len() >= 36 {
        u32::from_le_bytes(vm[32..36].try_into().ok()?).max(1)
    } else {
        1
    };
    // Claim the next nullifier index. Once the budget is spent, stop minting —
    // dialing without a credential under require=true then fails fast, the
    // correct signal that the epoch's budget is exhausted (rotate / raise it).
    let nf_index = ring.next_nf.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if nf_index >= max_dials {
        tracing::warn!(
            nf_index,
            max_dials,
            epoch_id = ring.epoch_id,
            "ring credential: per-epoch dial budget exhausted — no 0x04 minted for this dial"
        );
        return None;
    }
    let transcript = ring.epoch_id.to_le_bytes();
    let token = mirage_ringmember::prove_membership(
        &ring.member_sk,
        &ring.path,
        ring.epoch_id,
        &transcript,
        nf_index,
    )
    .encode();
    Some(mirage_protocol::driver::ClientCredential { scheme: 0x04, token })
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── TokenPool basic ops ───────────────────────────────────────────────────

    #[test]
    fn pool_starts_empty() {
        let p = TokenPool::default();
        assert!(p.is_empty());
        assert_eq!(p.len(), 0);
    }

    #[test]
    fn pool_push_and_len() {
        let mut p = TokenPool::default();
        let tok = StoredToken {
            epoch_id: 1,
            msg: [0u8; 32],
            randomizer: [0u8; bt::RANDOMIZER_LEN],
            sig: [0u8; bt::SIG_LEN],
        };
        p.push(tok.clone());
        assert_eq!(p.len(), 1);
        assert!(!p.is_empty());
        p.push(tok);
        assert_eq!(p.len(), 2);
    }

    #[test]
    fn pool_extend() {
        let mut p = TokenPool::default();
        let tok = StoredToken {
            epoch_id: 2,
            msg: [0xAAu8; 32],
            randomizer: [0u8; bt::RANDOMIZER_LEN],
            sig: [0u8; bt::SIG_LEN],
        };
        p.extend(vec![tok.clone(), tok]);
        assert_eq!(p.len(), 2);
    }

    // ── StoredToken::to_wire ─────────────────────────────────────────────────

    /// to_wire must produce exactly 324 bytes in the correct layout.
    #[test]
    fn to_wire_layout_and_length() {
        let tok = StoredToken {
            epoch_id: 0x0102_0304u32,
            msg: [0xAAu8; 32],
            randomizer: [0xBBu8; bt::RANDOMIZER_LEN],
            sig: [0xCCu8; bt::SIG_LEN],
        };
        let wire = tok.to_wire();
        assert_eq!(wire.len(), 324, "wire token must be 324 bytes");
        // epoch_id BE
        assert_eq!(&wire[0..4], &[0x01, 0x02, 0x03, 0x04]);
        // msg
        assert_eq!(&wire[4..36], &[0xAAu8; 32]);
        // randomizer
        assert_eq!(&wire[36..68], &[0xBBu8; 32]);
        // sig
        assert_eq!(&wire[68..324], &[0xCCu8; 256]);
    }

    // ── pop_one ───────────────────────────────────────────────────────────────

    fn make_stored_token(epoch_id: u32, fill: u8) -> StoredToken {
        StoredToken {
            epoch_id,
            msg: [fill; 32],
            randomizer: [fill; bt::RANDOMIZER_LEN],
            sig: [fill; bt::SIG_LEN],
        }
    }

    /// Two distinct tokens are returned in pop order; third pop returns None.
    #[test]
    fn pop_one_single_use_semantics() {
        use arc_swap::ArcSwap;

        let mut p = TokenPool::default();
        p.push(make_stored_token(1, 0x11));
        p.push(make_stored_token(2, 0x22));

        let pool = ArcSwap::from_pointee(p);

        let t1 = super::pop_one(&pool).expect("first pop must succeed");
        let t2 = super::pop_one(&pool).expect("second pop must succeed");
        // Tokens are distinct (epoch_ids are different).
        assert_ne!(t1.epoch_id, t2.epoch_id);
        // Pool is now empty.
        assert!(super::pop_one(&pool).is_none(), "third pop must return None");
    }

    /// pop_one on an empty pool returns None immediately.
    #[test]
    fn pop_one_empty_returns_none() {
        use arc_swap::ArcSwap;

        let pool = ArcSwap::from_pointee(TokenPool::default());
        assert!(super::pop_one(&pool).is_none());
    }

    /// A popped token assembled via to_wire has scheme-0x03 wire byte pattern:
    /// first 4 bytes encode epoch_id big-endian.
    #[test]
    fn pop_one_produces_valid_wire_token() {
        use arc_swap::ArcSwap;

        let mut p = TokenPool::default();
        p.push(make_stored_token(0xDEAD_BEEFu32, 0x42));
        let pool = ArcSwap::from_pointee(p);

        let tok = super::pop_one(&pool).expect("pop must succeed");
        let wire = tok.to_wire();
        assert_eq!(wire.len(), 324);
        // epoch_id = 0xDEADBEEF → bytes [DE AD BE EF]
        assert_eq!(&wire[0..4], &[0xDE, 0xAD, 0xBE, 0xEF]);
    }

    // ── absorb_issue_result roundtrip ─────────────────────────────────────────

    /// End-to-end test: keygen → blind 3 msgs → sign (issuer side) → absorb_issue_result
    /// → assert 3 StoredTokens pass bt::verify.
    #[test]
    fn absorb_issue_result_roundtrip() {
        let (sk, pk) = bt::issuer_keygen().expect("keygen");

        // Client: blind 3 messages.
        let msgs: Vec<[u8; bt::MSG_LEN]> = (0u8..3).map(|i| [i; bt::MSG_LEN]).collect();
        let mut blinded_hexes = Vec::new();
        let mut retained: Vec<([u8; bt::MSG_LEN], bt::ClientBlind)> = Vec::new();

        for &msg in &msgs {
            let (blinded_bytes, client_blind) = bt::blind(&pk, &msg).expect("blind");
            blinded_hexes.push(hex::encode(&blinded_bytes));
            retained.push((msg, client_blind));
        }

        // Issuer: sign each blinded message.
        let blind_sigs_hex: Vec<String> = blinded_hexes
            .iter()
            .map(|bh| {
                let blinded = hex::decode(bh).unwrap();
                let sig = bt::blind_sign(&sk, &blinded).expect("blind_sign");
                hex::encode(&sig)
            })
            .collect();

        // Client: absorb.
        let epoch_id = 7u32;
        let tokens = absorb_issue_result(&pk, retained, &blind_sigs_hex, epoch_id);
        assert_eq!(tokens.len(), 3, "all 3 tokens must succeed");

        // Verify each token.
        for (i, tok) in tokens.iter().enumerate() {
            assert_eq!(tok.epoch_id, epoch_id);
            assert_eq!(tok.msg, msgs[i]);
            bt::verify(&pk, &tok.msg, &tok.randomizer, &tok.sig)
                .unwrap_or_else(|e| panic!("token {i} verify failed: {e}"));
        }
    }

    /// Bad hex in one sig → that entry is skipped, others succeed.
    #[test]
    fn absorb_issue_result_bad_hex_skipped() {
        let (sk, pk) = bt::issuer_keygen().expect("keygen");

        let msgs: Vec<[u8; bt::MSG_LEN]> = (0u8..2).map(|i| [i; bt::MSG_LEN]).collect();
        let mut retained = Vec::new();
        let mut blinded_bytes_list = Vec::new();

        for &msg in &msgs {
            let (blinded, client_blind) = bt::blind(&pk, &msg).expect("blind");
            blinded_bytes_list.push(blinded);
            retained.push((msg, client_blind));
        }

        let mut blind_sigs_hex: Vec<String> = blinded_bytes_list
            .iter()
            .map(|b| hex::encode(bt::blind_sign(&sk, b).unwrap()))
            .collect();
        // Corrupt the first sig hex.
        blind_sigs_hex[0] = "not_valid_hex!!!".to_string();

        let tokens = absorb_issue_result(&pk, retained, &blind_sigs_hex, 1);
        assert_eq!(tokens.len(), 1, "only the valid token must be returned");
        assert_eq!(tokens[0].msg, msgs[1]);
    }
}
