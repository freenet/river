//! ECIES helpers for room secret distribution.
//!
//! These helpers are shared between the UI (`river-ui`) and the chat delegate
//! (`chat-delegate`) so secret rotation, sealing, and unsealing all use byte-identical
//! constructions on both sides. The previous home of this code was
//! `ui/src/util/ecies.rs`; this module is the single source of truth and the UI
//! file is now a thin re-export.
//!
//! Feature gates:
//! * `ecies` — deterministic helpers usable in environments that have no
//!   randomness source (the freenet-core delegate runtime is one such
//!   environment: it has no `getrandom` backend on wasm32-unknown-unknown).
//!   This feature does NOT pull `rand`/`getrandom` into the dependency graph.
//! * `ecies-randomized` — adds the helpers that need a CSPRNG
//!   (`generate_room_secret`, `encrypt_with_symmetric_key`, `seal_bytes`).
//!   The UI enables this feature because it runs in a browser and has a
//!   working `getrandom` backend via `wasm-bindgen`.
//!
//! Why this split exists: issue freenet/river#241. Pulling `getrandom` into
//! the chat-delegate build via the workspace's `js` feature caused the
//! committed delegate WASM to contain unresolved `__wbindgen_placeholder__`
//! imports, which wasmtime cannot instantiate.

use crate::room_state::privacy::SealedBytes;
use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use curve25519_dalek::edwards::CompressedEdwardsY;
use ed25519_dalek::{SigningKey, VerifyingKey};
use sha2::{Digest, Sha256, Sha512};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret as X25519EphemeralSecret};

/// Domain-separation tag for the ephemeral-keypair derivation in
/// [`encrypt_secret_for_member`]. Any change to the derivation inputs MUST
/// bump this string AND add a new entry to `legacy_delegates.toml` so old
/// blobs remain decryptable (decryption does not use this tag — it works on
/// the wire bytes — but the new ciphertext bytes are different, which is
/// observable to anyone byte-comparing).
const ECIES_EPHEMERAL_DOMAIN: &str = "river-ecies-ephemeral-v1 2026-05";

fn ed25519_to_x25519_public_key(ed25519_pk: &VerifyingKey) -> X25519PublicKey {
    let ed_y = CompressedEdwardsY(ed25519_pk.to_bytes())
        .decompress()
        .expect("Invalid Edwards point");
    let mont_u = ed_y.to_montgomery().to_bytes();
    X25519PublicKey::from(mont_u)
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

/// Decrypts a ciphertext produced by ECIES.
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

// ============================================================================
// Symmetric decryption (always available — no randomness required)
// ============================================================================

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

// ============================================================================
// ECIES encrypt — deterministic, no randomness required
// ============================================================================

/// Encrypts a 32-byte room secret for a specific member using ECIES.
///
/// **Determinism:** the output is a deterministic function of the inputs
/// `(secret, member_public_key)` — calling this twice with the same inputs
/// produces byte-identical output. This is the property the chat-delegate
/// needs: the delegate runtime has no CSPRNG, and multi-device replicas of
/// the delegate must converge on byte-identical state without coordination.
///
/// **Why determinism is safe here:**
/// * The ephemeral X25519 private key is derived via blake3 from
///   `(secret, member_public_key)`. Each (secret, recipient) pair produces a
///   unique ephemeral key, so each call produces a unique symmetric key.
/// * The AES-GCM nonce is fixed at all-zeros, which is safe because (key,
///   nonce) is unique-per-call (uniqueness comes from the unique key, not
///   the nonce).
/// * `secret` is itself derived from the room-owner signing seed via
///   [`crate::key_derivation::derive_room_secret`] (or by the UI's
///   `generate_room_secret` for legacy callers), so the ephemeral derivation
///   is keyed by a high-entropy input.
///
/// **Forward secrecy against a removed member** is preserved by the
/// existing secret-version rotation scheme — a removed member who knows
/// `secret_v_n` cannot derive `secret_v_{n+1}` because the latter is keyed
/// by the owner's signing seed.
///
/// Returns `(ciphertext, nonce, sender_ephemeral_x25519_public_key)`.
pub fn encrypt_secret_for_member(
    secret: &[u8; 32],
    member_public_key: &VerifyingKey,
) -> (Vec<u8>, [u8; 12], X25519PublicKey) {
    let mut hasher = blake3::Hasher::new();
    hasher.update(ECIES_EPHEMERAL_DOMAIN.as_bytes());
    hasher.update(secret);
    hasher.update(member_public_key.as_bytes());
    let ephemeral_seed: [u8; 32] = *hasher.finalize().as_bytes();

    let sender_private_key = X25519EphemeralSecret::from(ephemeral_seed);
    let sender_public_key = X25519PublicKey::from(&sender_private_key);

    let recipient_x25519_public_key = ed25519_to_x25519_public_key(member_public_key);
    let shared_secret = sender_private_key.diffie_hellman(&recipient_x25519_public_key);
    let symmetric_key = Sha256::digest(shared_secret.as_bytes());

    let nonce: [u8; 12] = [0u8; 12]; // safe: each call has a unique symmetric key
    let cipher = Aes256Gcm::new_from_slice(&symmetric_key).expect("Failed to create cipher");
    let ciphertext = cipher
        .encrypt(&Nonce::from(nonce), secret.as_slice())
        .expect("encryption failure!");

    (ciphertext, nonce, sender_public_key)
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
// Sealing — `unseal_*` is always available; `seal_bytes` requires randomness
// ============================================================================

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

// ============================================================================
// Randomized helpers — gated behind `ecies-randomized` feature so the
// chat-delegate build does NOT pull `rand`/`getrandom` into its dep graph.
// ============================================================================

/// Generates a new random 32-byte room secret.
///
/// Prefer [`crate::key_derivation::derive_room_secret`] for new code where the
/// caller has access to the owner signing-key seed — that derivation is
/// deterministic and lets multi-device replicas converge without coordination.
/// This random helper is kept for callers that don't have access to the seed.
#[cfg(feature = "ecies-randomized")]
pub fn generate_room_secret() -> [u8; 32] {
    rand::random::<[u8; 32]>()
}

/// Encrypts data using a symmetric key (typically a room secret) with a
/// freshly-generated random nonce.
///
/// Available only with the `ecies-randomized` feature.
#[cfg(feature = "ecies-randomized")]
pub fn encrypt_with_symmetric_key(key: &[u8; 32], plaintext: &[u8]) -> (Vec<u8>, [u8; 12]) {
    let cipher = Aes256Gcm::new_from_slice(key).expect("Failed to create cipher");
    let nonce_bytes = rand::random::<[u8; 12]>();
    let nonce = Nonce::from(nonce_bytes);

    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .expect("Symmetric encryption failure");

    (ciphertext, nonce_bytes)
}

/// Creates a [`SealedBytes::Private`] variant by encrypting plaintext with a
/// room secret.
///
/// Available only with the `ecies-randomized` feature.
#[cfg(feature = "ecies-randomized")]
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

#[cfg(all(test, feature = "ecies-randomized"))]
mod tests {
    use super::*;
    use ed25519_dalek::{SigningKey, VerifyingKey};
    use rand::rngs::OsRng;

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
    fn encrypt_secret_for_member_is_deterministic() {
        // Same (secret, recipient) MUST produce byte-identical output across
        // calls — this is the property the chat-delegate relies on for
        // multi-device replica convergence.
        let mut rng = OsRng;
        let member_sk = SigningKey::generate(&mut rng);
        let member_vk = VerifyingKey::from(&member_sk);
        let secret = generate_room_secret();

        let (ct1, n1, ek1) = encrypt_secret_for_member(&secret, &member_vk);
        let (ct2, n2, ek2) = encrypt_secret_for_member(&secret, &member_vk);

        assert_eq!(ct1, ct2, "ciphertext must be deterministic");
        assert_eq!(n1, n2, "nonce must be deterministic");
        assert_eq!(
            ek1.as_bytes(),
            ek2.as_bytes(),
            "ephemeral pubkey must be deterministic"
        );

        // Sanity: still decrypts correctly.
        let decrypted = decrypt_secret_from_member_blob(&ct1, &n1, &ek1, &member_sk).unwrap();
        assert_eq!(decrypted, secret);
    }

    #[test]
    fn encrypt_secret_for_member_distinguishes_recipients() {
        let mut rng = OsRng;
        let member_sk_a = SigningKey::generate(&mut rng);
        let member_vk_a = VerifyingKey::from(&member_sk_a);
        let member_sk_b = SigningKey::generate(&mut rng);
        let member_vk_b = VerifyingKey::from(&member_sk_b);
        let secret = generate_room_secret();

        let (ct_a, _, ek_a) = encrypt_secret_for_member(&secret, &member_vk_a);
        let (ct_b, _, ek_b) = encrypt_secret_for_member(&secret, &member_vk_b);

        assert_ne!(
            ct_a, ct_b,
            "different recipients must produce different ciphertexts"
        );
        assert_ne!(
            ek_a.as_bytes(),
            ek_b.as_bytes(),
            "different recipients must produce different ephemeral pubkeys"
        );
    }

    #[test]
    fn encrypt_secret_for_member_distinguishes_secrets() {
        let mut rng = OsRng;
        let member_sk = SigningKey::generate(&mut rng);
        let member_vk = VerifyingKey::from(&member_sk);
        let secret_v0 = generate_room_secret();
        let secret_v1 = generate_room_secret();

        let (ct0, _, ek0) = encrypt_secret_for_member(&secret_v0, &member_vk);
        let (ct1, _, ek1) = encrypt_secret_for_member(&secret_v1, &member_vk);

        assert_ne!(
            ct0, ct1,
            "different secrets must produce different ciphertexts"
        );
        assert_ne!(
            ek0.as_bytes(),
            ek1.as_bytes(),
            "different secrets must produce different ephemeral pubkeys"
        );
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
