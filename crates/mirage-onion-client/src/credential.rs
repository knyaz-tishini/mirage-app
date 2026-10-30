//! The credential the chain leg presents.
//!
//! The scheme is `CredentialScheme::RoleSigned`: an operator-signed identity
//! excerpt, plus a per-dial signature bound to the exit's own node id. The
//! exit verifies the operator's signature against the operator key it is
//! configured to trust (`MIRAGE_OPERATOR_PUBKEY_HEX` on a standalone box) and
//! the per-dial signature against the excerpt's own signing key.
//!
//! In this mode the client IS the edge and the operator of its own chain, so
//! the chain's operator keypair plays both parts: it signs the excerpt, and it
//! signs the dial. The chain's trust anchor and the client's signing key are
//! therefore the same key - which is exactly what the artefact says when it
//! carries one operator keypair.

use ed25519_dalek::SigningKey;
use mirage_core::capsule::RoleSlice;
use mirage_core::crypto::credential::role_signed::{
    mint_role_signed_token, sign_role_signed_excerpt, RoleSignedError, RoleSignedIdentityExcerpt,
};
use mirage_core::crypto::KeyPair;
use mirage_core::identity::ServiceRole;
use mirage_core::mirage::wire::credential_scheme::CredentialScheme;
use mirage_protocol::driver::ClientCredential;
use thiserror::Error;

use crate::spec::OnionChainSpec;

/// Every way the chain leg's credential is refused, by name.
#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("onion credential: the operator secret is not valid hex: {reason}")]
    MalformedSecret { reason: String },

    #[error("onion credential: the operator secret is not {expected_bytes} bytes, got {got_bytes}")]
    WrongSecretLength { expected_bytes: usize, got_bytes: usize },

    #[error(
        "onion credential: the operator secret does not match the trust anchor - the artefact names two different keys"
    )]
    SecretDoesNotMatchAnchor,

    #[error("onion credential: {0}")]
    RoleSigned(#[from] RoleSignedError),

    #[error("onion credential: the token does not encode: {0}")]
    MalformedToken(String),
}

/// Mint the credential one chain leg presents.
///
/// `now_ms` is the dial's timestamp - the exit refuses a token whose
/// timestamp is more than a minute away from its own clock.
pub fn mint_chain_leg_credential(
    spec: &OnionChainSpec,
    now_ms: u64,
) -> Result<ClientCredential, CredentialError> {
    let secret_bytes = hex::decode(&spec.operator_secret_hex).map_err(|e| CredentialError::MalformedSecret {
        reason: e.to_string(),
    })?;
    if secret_bytes.len() != 32 {
        return Err(CredentialError::WrongSecretLength {
            expected_bytes: 32,
            got_bytes: secret_bytes.len(),
        });
    }
    let mut secret = [0u8; 32];
    secret.copy_from_slice(&secret_bytes);
    let operator_key = SigningKey::from_bytes(&secret);

    // The artefact names the trust anchor and the secret as a pair. If they
    // are two different keys the chain would never verify anything, so this
    // is caught here, by name, rather than at the exit as a bare refusal.
    let derived_pub = hex::encode(operator_key.verifying_key().to_bytes());
    if derived_pub != spec.operator_pubkey_hex {
        return Err(CredentialError::SecretDoesNotMatchAnchor);
    }

    // The edge's own slice: the wire key is the client's persistent static
    // key, the signing key is the operator's - the client signs as the edge.
    let wire = KeyPair::from_secret(spec.client_static_priv);
    let slice = RoleSlice {
        role: ServiceRole::Edge,
        wire_privkey: spec.client_static_priv,
        signing_privkey: Some(secret),
        wire_pubkey_hex: hex::encode(wire.public.to_bytes()),
        signing_pubkey_hex: Some(spec.operator_pubkey_hex.clone()),
    };

    let excerpt: RoleSignedIdentityExcerpt = sign_role_signed_excerpt(
        &operator_key,
        spec.self_id,
        &spec.mesh_id,
        ServiceRole::Edge,
        &spec.operator_pubkey_hex,
        spec.identity_not_after_ms,
    )?;

    let token = mint_role_signed_token(&slice, &excerpt, spec.exit_node_id(), now_ms)?;
    let bytes = token.encode().map_err(|e| CredentialError::MalformedToken(e.to_string()))?;

    Ok(ClientCredential {
        scheme: CredentialScheme::RoleSigned.as_u8(),
        token: bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::OnionHopSpec;

    fn spec() -> OnionChainSpec {
        // A fixed seed keeps the test deterministic - the scheme's freshness
        // comes from the per-dial nonce, not from the operator key.
        let key = SigningKey::from_bytes(&[0x42u8; 32]);
        OnionChainSpec {
            chain_id: "stand".to_owned(),
            mesh_id: "mirage-vpn".to_owned(),
            self_id: uuid::Uuid::new_v4(),
            operator_pubkey_hex: hex::encode(key.verifying_key().to_bytes()),
            operator_secret_hex: hex::encode(key.to_bytes()),
            identity_not_after_ms: 1_726_000_000_000,
            client_static_priv: [0x11; 32],
            hops: vec![
                OnionHopSpec {
                    node_id: "node-0".to_owned(),
                    addr: "203.0.113.1:41443".to_owned(),
                    server_pubkey_hex: "22".repeat(32),
                    camouflage: None,
                },
                OnionHopSpec {
                    node_id: "node-1".to_owned(),
                    addr: "203.0.113.2:41443".to_owned(),
                    server_pubkey_hex: "33".repeat(32),
                    camouflage: None,
                },
            ],
        }
    }

    #[test]
    fn the_credential_is_the_role_signed_scheme() {
        let credential = mint_chain_leg_credential(&spec(), 1_726_000_000_000).expect("credential");
        assert_eq!(credential.scheme, CredentialScheme::RoleSigned.as_u8());
        assert!(!credential.token.is_empty());
    }

    #[test]
    fn a_secret_that_does_not_match_the_anchor_is_refused() {
        let mut spec = spec();
        spec.operator_pubkey_hex = "44".repeat(32);
        assert!(matches!(
            mint_chain_leg_credential(&spec, 1_726_000_000_000),
            Err(CredentialError::SecretDoesNotMatchAnchor)
        ));
    }

    #[test]
    fn every_dial_carries_a_fresh_token() {
        let spec = spec();
        let first = mint_chain_leg_credential(&spec, 1_726_000_000_000).expect("first");
        let second = mint_chain_leg_credential(&spec, 1_726_000_000_001).expect("second");
        assert_ne!(first.token, second.token, "the per-dial nonce must change");
    }
}
