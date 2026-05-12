//! ECIES helpers for room secret distribution.
//!
//! These helpers are shared between the UI (`river-ui`) and the chat delegate
//! (`chat-delegate`) so secret rotation, sealing, and unsealing all use byte-identical
//! constructions on both sides. The previous home of this code was
//! `ui/src/util/ecies.rs`; this module is the single source of truth and the UI
//! file is now a thin re-export.
//!
//! Feature gate: enabled via the `ecies` Cargo feature on `river-core`.
//! `room-contract` does NOT enable this feature so the room contract WASM stays
//! small.

use crate::room_state::privacy::SealedBytes;
use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use curve25519_dalek::edwards::CompressedEdwardsY;
use ed25519_dalek::{SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use sha2::{Digest, Sha256, Sha512};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret as X25519EphemeralSecret};

/// Encrypts a plaintext message using ECIES (Elliptic Curve Integrated Encryption Scheme).
///
/// Uses ed25519_dalek SigningKey and VerifyingKey for compatibility with the rest
/// of the River codebase (the room owner / member identities are ed25519 keys).
///
/// Returns `(ciphertext, nonce, sender_ephemeral_x25519_public_key)`.
pub fn encrypt(
    recipient_public_key: &VerifyingKey,
    plaintext: &[u8],
) -> (Vec<u8>, [u8; 12], X25519PublicKey) {
    let sender_private_key = X25519EphemeralSecret::random_from_rng(OsRng);
    let sender_public_key = X25519PublicKey::from(&sender_private_key);

    let recipient_x25519_public_key = ed25519_to_x25519_public_key(recipient_public_key);
    let shared_secret = sender_private_key.diffie_hellman(&recipient_x25519_public_key);
    let symmetric_key = Sha256::digest(shared_secret.as_bytes());

    let nonce = rand::random::<[u8; 12]>();
    let cipher = Aes256Gcm::new_from_slice(&symmetric_key).expect("Failed to create cipher");
    let ciphertext = cipher
        .encrypt(&Nonce::from(nonce), plaintext)
        .expect("encryption failure!");

    (ciphertext, nonce, sender_public_key)
}

fn ed25519_to_x25519_public_key(ed25519_pk: &VerifyingKey) -> X25519PublicKey {
    let ed_y = CompressedEdwardsY(ed25519_pk.to_bytes())
        .decompress()
        .expect("Invalid Edwards point");
    let mont_u = ed_y.to_montgomery().to_bytes();
    X25519PublicKey::from(mont_u)
}

/// Decrypts a ciphertext produced by [`encrypt`].
pub fn decrypt(
    recipient_private_key: &SigningKey,
    sender_public_key: &X25519PublicKey,
    ciphertext: &[u8],
    nonce: &[u8; 12],
) -> Vec<u8> {
    let recipient_x25519_private_key = ed25519_to_x25519_private_key(recipient_private_key);
    let shared_secret = recipient_x25519_private_key.diffie_hellman(sender_public_key);
    let symmetric_key = Sha256::digest(shared_secret.as_bytes());

    let cipher = Aes256Gcm::new_from_slice(&symmetric_key).expect("Failed to create cipher");
    cipher
        .decrypt(&Nonce::from(*nonce), ciphertext.as_ref())
        .expect("decryption failure!")
}

fn ed25519_to_x25519_private_key(ed25519_sk: &SigningKey) -> X25519EphemeralSecret {
    let h = Sha512::digest(ed25519_sk.to_bytes());
    let mut key = [0u8; 32];
    key.copy_from_slice(&h[..32]);
    key[0] &= 248;
    key[31] &= 127;
    key[31] |= 64;
    X25519EphemeralSecret::from(key)
}

// ============================================================================
// Symmetric encryption utilities for room secrets
// ============================================================================

/// Encrypts data using a symmetric key (typically a room secret).
pub fn encrypt_with_symmetric_key(key: &[u8; 32], plaintext: &[u8]) -> (Vec<u8>, [u8; 12]) {
    let cipher = Aes256Gcm::new_from_slice(key).expect("Failed to create cipher");
    let nonce_bytes = rand::random::<[u8; 12]>();
    let nonce = Nonce::from(nonce_bytes);

    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .expect("Symmetric encryption failure");

    (ciphertext, nonce_bytes)
}

/// Decrypts data using a symmetric key.
pub fn decrypt_with_symmetric_key(
    key: &[u8; 32],
    ciphertext: &[u8],
    nonce: &[u8; 12],
) -> Result<Vec<u8>, String> {
    let cipher =
        Aes256Gcm::new_from_slice(key).map_err(|e| format!("Failed to create cipher: {}", e))?;
    let nonce_obj = Nonce::from(*nonce);

    cipher
        .decrypt(&nonce_obj, ciphertext)
        .map_err(|e| format!("Decryption failed: {}", e))
}

/// Generates a new random 32-byte room secret.
///
/// Prefer [`crate::key_derivation::derive_room_secret`] for new code where the
/// caller has access to the owner signing-key seed — that derivation is
/// deterministic and lets multi-device replicas converge without coordination.
/// This random helper is kept for callers that don't have access to the seed.
pub fn generate_room_secret() -> [u8; 32] {
    rand::random::<[u8; 32]>()
}

/// Encrypts a room secret for a specific member using ECIES.
///
/// Returns `(ciphertext, nonce, sender_ephemeral_x25519_public_key)`.
pub fn encrypt_secret_for_member(
    secret: &[u8; 32],
    member_public_key: &VerifyingKey,
) -> (Vec<u8>, [u8; 12], X25519PublicKey) {
    encrypt(member_public_key, secret)
}

/// Decrypts a room secret from an [`crate::room_state::secret::EncryptedSecretForMemberV1`] blob.
pub fn decrypt_secret_from_member_blob(
    ciphertext: &[u8],
    nonce: &[u8; 12],
    ephemeral_sender_key: &X25519PublicKey,
    member_private_key: &SigningKey,
) -> Result<[u8; 32], String> {
    let decrypted = decrypt(member_private_key, ephemeral_sender_key, ciphertext, nonce);

    if decrypted.len() != 32 {
        return Err(format!(
            "Decrypted secret has invalid length: {} (expected 32)",
            decrypted.len()
        ));
    }

    let mut secret = [0u8; 32];
    secret.copy_from_slice(&decrypted);
    Ok(secret)
}

// ============================================================================
// SealedBytes helpers
// ============================================================================

/// Creates a [`SealedBytes::Private`] variant by encrypting plaintext with a
/// room secret.
pub fn seal_bytes(plaintext: &[u8], secret_key: &[u8; 32], secret_version: u32) -> SealedBytes {
    let (ciphertext, nonce) = encrypt_with_symmetric_key(secret_key, plaintext);
    let declared_len_bytes = plaintext.len() as u32;

    SealedBytes::Private {
        ciphertext,
        nonce,
        secret_version,
        declared_len_bytes,
    }
}

/// Unseals a [`SealedBytes`] value, returning the plaintext.
pub fn unseal_bytes(
    sealed: &SealedBytes,
    secret_key: Option<&[u8; 32]>,
) -> Result<Vec<u8>, String> {
    match sealed {
        SealedBytes::Public { value } => Ok(value.clone()),
        SealedBytes::Private {
            ciphertext, nonce, ..
        } => {
            let key = secret_key.ok_or("Secret key required to unseal private data")?;
            decrypt_with_symmetric_key(key, ciphertext, nonce)
        }
    }
}

/// Unseal private data using a map of secrets by version.
pub fn unseal_bytes_with_secrets(
    sealed: &SealedBytes,
    secrets: &std::collections::HashMap<u32, [u8; 32]>,
) -> Result<Vec<u8>, String> {
    match sealed {
        SealedBytes::Public { value } => Ok(value.clone()),
        SealedBytes::Private {
            ciphertext,
            nonce,
            secret_version,
            ..
        } => {
            let key = secrets.get(secret_version).ok_or_else(|| {
                format!(
                    "Secret version {} not available (have versions: {:?})",
                    secret_version,
                    secrets.keys().collect::<Vec<_>>()
                )
            })?;
            decrypt_with_symmetric_key(key, ciphertext, nonce)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{SigningKey, VerifyingKey};

    #[test]
    fn ecies_round_trip() {
        let mut rng = OsRng;
        let recipient_sk = SigningKey::generate(&mut rng);
        let recipient_vk: VerifyingKey = VerifyingKey::from(&recipient_sk);

        let plaintext = b"Secret message";
        let (ciphertext, nonce, sender_pk) = encrypt(&recipient_vk, plaintext);
        let decrypted = decrypt(&recipient_sk, &sender_pk, &ciphertext, &nonce);

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn symmetric_round_trip() {
        let key = generate_room_secret();
        let plaintext = b"Room secret message";

        let (ciphertext, nonce) = encrypt_with_symmetric_key(&key, plaintext);
        let decrypted = decrypt_with_symmetric_key(&key, &ciphertext, &nonce).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn encrypt_decrypt_secret_for_member_round_trip() {
        let mut rng = OsRng;
        let member_sk = SigningKey::generate(&mut rng);
        let member_vk = VerifyingKey::from(&member_sk);

        let original_secret = generate_room_secret();
        let (ciphertext, nonce, ephemeral_key) =
            encrypt_secret_for_member(&original_secret, &member_vk);

        let decrypted_secret =
            decrypt_secret_from_member_blob(&ciphertext, &nonce, &ephemeral_key, &member_sk)
                .unwrap();

        assert_eq!(decrypted_secret, original_secret);
    }

    #[test]
    fn seal_unseal_private_round_trip() {
        let secret_key = generate_room_secret();
        let plaintext = b"Private nickname";
        let secret_version = 5;

        let sealed = seal_bytes(plaintext, &secret_key, secret_version);
        let unsealed = unseal_bytes(&sealed, Some(&secret_key)).unwrap();
        assert_eq!(unsealed, plaintext);
    }
}
