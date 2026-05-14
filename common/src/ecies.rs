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
/// **API-shape safety invariant — do not refactor.** The all-zero-nonce
/// argument above only holds because the plaintext fed to AES-GCM is
/// always `secret` itself — i.e. the same value that keys the ephemeral
/// derivation. Generalizing this function to take an arbitrary plaintext
/// would invite a caller to encrypt two different plaintexts under the
/// same (key, nonce) pair, which leaks `plaintext_A XOR plaintext_B` via
/// AES-GCM keystream reuse. Keep the signature as
/// `(secret, member_public_key)` — if you need to encrypt something other
/// than a room secret, write a new function with its own derivation.
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

// =============================================================================
// Tests
// =============================================================================
//
// The tests are split into two modules:
// * `tests` — deterministic tests that pin byte output. These run with the
//   default `ecies` feature (no `rand` needed) so `cargo test -p river-core
//   --features ecies` exercises them. They construct keys from fixed seeds
//   via `SigningKey::from_bytes(...)` instead of `SigningKey::generate(rng)`.
// * `tests_randomized` — round-trip tests for the helpers gated behind
//   `ecies-randomized` (e.g. `generate_room_secret`, `seal_bytes`).

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn fixed_signing_key(seed_byte: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed_byte; 32])
    }

    #[test]
    fn encrypt_secret_for_member_round_trip_deterministic_inputs() {
        let member_sk = fixed_signing_key(7);
        let member_vk = VerifyingKey::from(&member_sk);
        let secret = [13u8; 32];

        let (ciphertext, nonce, ephemeral_key) = encrypt_secret_for_member(&secret, &member_vk);
        let decrypted =
            decrypt_secret_from_member_blob(&ciphertext, &nonce, &ephemeral_key, &member_sk)
                .unwrap();
        assert_eq!(decrypted, secret);
    }

    #[test]
    fn encrypt_secret_for_member_is_deterministic() {
        // Same (secret, recipient) MUST produce byte-identical output across
        // calls — this is the property the chat-delegate relies on for
        // multi-device replica convergence.
        let member_sk = fixed_signing_key(7);
        let member_vk = VerifyingKey::from(&member_sk);
        let secret = [13u8; 32];

        let (ct1, n1, ek1) = encrypt_secret_for_member(&secret, &member_vk);
        let (ct2, n2, ek2) = encrypt_secret_for_member(&secret, &member_vk);

        assert_eq!(ct1, ct2, "ciphertext must be deterministic");
        assert_eq!(n1, n2, "nonce must be deterministic");
        assert_eq!(
            ek1.as_bytes(),
            ek2.as_bytes(),
            "ephemeral pubkey must be deterministic"
        );

        let decrypted = decrypt_secret_from_member_blob(&ct1, &n1, &ek1, &member_sk).unwrap();
        assert_eq!(decrypted, secret);
    }

    #[test]
    fn encrypt_secret_for_member_distinguishes_recipients() {
        let member_sk_a = fixed_signing_key(1);
        let member_vk_a = VerifyingKey::from(&member_sk_a);
        let member_sk_b = fixed_signing_key(2);
        let member_vk_b = VerifyingKey::from(&member_sk_b);
        let secret = [99u8; 32];

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
        let member_sk = fixed_signing_key(7);
        let member_vk = VerifyingKey::from(&member_sk);
        let secret_v0 = [0xA0u8; 32];
        let secret_v1 = [0xB0u8; 32];

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

    /// Known-answer test pinning the byte output of
    /// `encrypt_secret_for_member`. If this test fails after a refactor,
    /// you have changed the wire format produced by the function — bump
    /// `ECIES_EPHEMERAL_DOMAIN` AND add a new entry to
    /// `legacy_delegates.toml`, or revert the change. Determinism is only
    /// useful relative to a fixed encoding, and silent bytewise drift
    /// orphans every delegate state ever written.
    ///
    /// Vectors generated 2026-05-13 against PR #242. To regenerate (after
    /// an intentional, documented format bump): replace the four
    /// `assert_eq!` expected hex strings with the values printed by
    /// `cargo test -- --nocapture encrypt_secret_for_member_known_answer`
    /// after temporarily uncommenting the `eprintln!` lines.
    #[test]
    fn encrypt_secret_for_member_known_answer() {
        let member_sk = fixed_signing_key(3);
        let member_vk = VerifyingKey::from(&member_sk);
        let secret = [42u8; 32];

        let (ciphertext, nonce, ephemeral) = encrypt_secret_for_member(&secret, &member_vk);

        // To regenerate after an intentional, documented format bump:
        // uncomment the three eprintln! lines and run with `-- --nocapture`,
        // then paste the printed hex into the assertions below.
        // eprintln!("ciphertext: {}", hex::encode(&ciphertext));
        // eprintln!("nonce:      {}", hex::encode(nonce));
        // eprintln!("ephemeral:  {}", hex::encode(ephemeral.as_bytes()));

        assert_eq!(
            hex::encode(&ciphertext),
            "ae3a2f82fc8982c6014649b76c19ea0920d0eaf9bf8f2690ddf7dd70bda39bc54d829d924dc0afb8621639430515c78d",
            "ciphertext byte vector changed — see test docstring"
        );
        assert_eq!(nonce, [0u8; 12], "nonce must remain all-zero");
        assert_eq!(
            hex::encode(ephemeral.as_bytes()),
            "19f806d18ca5b14914ebd6831cf896369030b1e9c8c36ae60f7156317021aa12",
            "ephemeral pubkey byte vector changed — see test docstring"
        );

        // And it must decrypt back to the input secret.
        let decrypted =
            decrypt_secret_from_member_blob(&ciphertext, &nonce, &ephemeral, &member_sk).unwrap();
        assert_eq!(decrypted, secret);
    }

    /// Decrypts a blob whose nonce is non-zero — i.e. shaped like a blob
    /// produced by the pre-#242 random-nonce encrypt path. The wire
    /// format (ciphertext, nonce, ephemeral_pub) is unchanged across the
    /// PR, so existing on-disk encrypted-secret blobs in delegate state
    /// MUST still decrypt with the post-#242 code. If this test fails,
    /// every existing private room loses access on upgrade.
    #[test]
    fn decrypt_random_nonce_blob_backward_compat() {
        use aes_gcm::aead::{Aead, KeyInit};
        use x25519_dalek::StaticSecret;

        let member_sk = fixed_signing_key(7);
        let member_vk = VerifyingKey::from(&member_sk);
        let original_secret = [13u8; 32];

        // Build an "old-style" blob using a non-zero nonce and an
        // ephemeral keypair that is NOT derived from the secret — exactly
        // what the old random-nonce code path produced. The choice of
        // these bytes is arbitrary; they just must not match what the
        // current deterministic encoder would produce.
        let old_ephemeral_priv = StaticSecret::from([0x55u8; 32]);
        let old_ephemeral_pub = X25519PublicKey::from(&old_ephemeral_priv);
        let recipient_x25519 = ed25519_to_x25519_public_key(&member_vk);
        let shared = old_ephemeral_priv.diffie_hellman(&recipient_x25519);
        let sym_key = Sha256::digest(shared.as_bytes());
        let old_nonce: [u8; 12] = [0xAB; 12];
        let cipher = Aes256Gcm::new_from_slice(&sym_key).unwrap();
        let old_ct = cipher
            .encrypt(&Nonce::from(old_nonce), original_secret.as_slice())
            .unwrap();

        let decrypted =
            decrypt_secret_from_member_blob(&old_ct, &old_nonce, &old_ephemeral_pub, &member_sk)
                .unwrap();
        assert_eq!(decrypted, original_secret);
    }
}

#[cfg(all(test, feature = "ecies-randomized"))]
mod tests_randomized {
    use super::*;
    use ed25519_dalek::SigningKey;
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
    fn seal_unseal_private_round_trip() {
        let secret_key = generate_room_secret();
        let plaintext = b"Private nickname";
        let secret_version = 5;

        let sealed = seal_bytes(plaintext, &secret_key, secret_version);
        let unsealed = unseal_bytes(&sealed, Some(&secret_key)).unwrap();
        assert_eq!(unsealed, plaintext);
    }

    #[test]
    fn encrypt_decrypt_secret_for_member_round_trip_randomized_inputs() {
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
}
