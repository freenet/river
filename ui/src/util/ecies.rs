use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use curve25519_dalek::edwards::CompressedEdwardsY;
use ed25519_dalek::{SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use river_core::room_state::privacy::SealedBytes;
use sha2::{Digest, Sha256, Sha512};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret as X25519EphemeralSecret};

/// Encrypts a plaintext message using ECIES (Elliptic Curve Integrated Encryption Scheme).
/// Uses ed25519_dalek SigningKey and VerifyingKey because they're used elsewhere in the codebase.
///
/// # Arguments
///
/// * `recipient_public_key` - The public key of the message recipient.
/// * `plaintext` - The message to be encrypted.
///
/// # Returns
///
/// A tuple containing:
/// * The encrypted ciphertext.
/// * A 12-byte nonce used for encryption.
/// * The ephemeral public key of the sender.
#[allow(dead_code)]
pub fn encrypt(
    recipient_public_key: &VerifyingKey,
    plaintext: &[u8],
) -> (Vec<u8>, [u8; 12], X25519PublicKey) {
    // Generate an ephemeral keypair
    let sender_private_key = X25519EphemeralSecret::random_from_rng(OsRng);
    let sender_public_key = X25519PublicKey::from(&sender_private_key);

    // Convert Ed25519 verifying key to X25519 public key
    let recipient_x25519_public_key = ed25519_to_x25519_public_key(recipient_public_key);

    // Derive shared secret using sender's private key and recipient's public key
    let shared_secret = sender_private_key.diffie_hellman(&recipient_x25519_public_key);

    // Use the shared secret to derive a symmetric key
    let symmetric_key = Sha256::digest(shared_secret.as_bytes());

    // Generate a random nonce
    let nonce = rand::random::<[u8; 12]>();

    // Encrypt the plaintext using AES-GCM
    let cipher = Aes256Gcm::new_from_slice(&symmetric_key).expect("Failed to create cipher");
    let ciphertext = cipher
        .encrypt(&Nonce::from(nonce), plaintext)
        .expect("encryption failure!");

    (ciphertext, nonce, sender_public_key)
}

#[allow(dead_code)]
fn ed25519_to_x25519_public_key(ed25519_pk: &VerifyingKey) -> X25519PublicKey {
    let ed_y = CompressedEdwardsY(ed25519_pk.to_bytes())
        .decompress()
        .expect("Invalid Edwards point");
    let mont_u = ed_y.to_montgomery().to_bytes();
    X25519PublicKey::from(mont_u)
}

/// Decrypts a ciphertext message using ECIES (Elliptic Curve Integrated Encryption Scheme).
///
/// # Arguments
///
/// * `recipient_private_key` - The private key of the message recipient.
/// * `sender_public_key` - The ephemeral public key of the sender.
/// * `ciphertext` - The encrypted message to be decrypted.
/// * `nonce` - The 12-byte nonce used for encryption.
///
/// # Returns
///
/// The decrypted plaintext message as a vector of bytes.
#[allow(dead_code)]
pub fn decrypt(
    recipient_private_key: &SigningKey,
    sender_public_key: &X25519PublicKey,
    ciphertext: &[u8],
    nonce: &[u8; 12],
) -> Vec<u8> {
    // Convert Ed25519 signing key to X25519 private key
    let recipient_x25519_private_key = ed25519_to_x25519_private_key(recipient_private_key);

    // Derive shared secret using recipient's private key and sender's public key
    let shared_secret = recipient_x25519_private_key.diffie_hellman(sender_public_key);

    // Use the shared secret to derive the symmetric key
    let symmetric_key = Sha256::digest(shared_secret.as_bytes());

    // Decrypt the ciphertext using AES-GCM
    let cipher = Aes256Gcm::new_from_slice(&symmetric_key).expect("Failed to create cipher");
    let decrypted_message = cipher
        .decrypt(&Nonce::from(*nonce), ciphertext.as_ref())
        .expect("decryption failure!");

    decrypted_message
}

#[allow(dead_code)]
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

/// Encrypts data using a symmetric key (for room secrets).
///
/// # Arguments
///
/// * `key` - A 32-byte symmetric key (the room secret).
/// * `plaintext` - The data to encrypt.
///
/// # Returns
///
/// A tuple containing the ciphertext and a 12-byte nonce.
#[allow(dead_code)]
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
///
/// # Arguments
///
/// * `key` - A 32-byte symmetric key (the room secret).
/// * `ciphertext` - The encrypted data.
/// * `nonce` - The 12-byte nonce used for encryption.
///
/// # Returns
///
/// The decrypted plaintext, or an error if decryption fails.
#[allow(dead_code)]
pub fn decrypt_with_symmetric_key(
    key: &[u8; 32],
    ciphertext: &[u8],
    nonce: &[u8; 12],
) -> Result<Vec<u8>, String> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| format!("Failed to create cipher: {}", e))?;
    let nonce_obj = Nonce::from(*nonce);

    cipher
        .decrypt(&nonce_obj, ciphertext)
        .map_err(|e| format!("Decryption failed: {}", e))
}

/// Generates a new random 32-byte room secret.
#[allow(dead_code)]
pub fn generate_room_secret() -> [u8; 32] {
    rand::random::<[u8; 32]>()
}

/// Encrypts a room secret for a specific member using ECIES.
///
/// This creates the ciphertext that goes into an EncryptedSecretForMember blob.
///
/// # Arguments
///
/// * `secret` - The 32-byte room secret to encrypt.
/// * `member_public_key` - The member's Ed25519 public key.
///
/// # Returns
///
/// A tuple containing:
/// * The encrypted secret (ciphertext).
/// * A 12-byte nonce.
/// * The ephemeral X25519 public key.
#[allow(dead_code)]
pub fn encrypt_secret_for_member(
    secret: &[u8; 32],
    member_public_key: &VerifyingKey,
) -> (Vec<u8>, [u8; 12], X25519PublicKey) {
    encrypt(member_public_key, secret)
}

/// Decrypts a room secret from an EncryptedSecretForMember blob.
///
/// # Arguments
///
/// * `ciphertext` - The encrypted secret.
/// * `nonce` - The 12-byte nonce.
/// * `ephemeral_sender_key` - The ephemeral X25519 public key from the blob.
/// * `member_private_key` - The member's Ed25519 private key.
///
/// # Returns
///
/// The decrypted 32-byte room secret, or an error if decryption fails.
#[allow(dead_code)]
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

/// Creates a SealedBytes::Private variant by encrypting plaintext with a room secret.
///
/// # Arguments
///
/// * `plaintext` - The data to encrypt.
/// * `secret_key` - The 32-byte room secret.
/// * `secret_version` - The version number of the room secret.
///
/// # Returns
///
/// A SealedBytes::Private variant containing the encrypted data.
#[allow(dead_code)]
pub fn seal_bytes(
    plaintext: &[u8],
    secret_key: &[u8; 32],
    secret_version: u32,
) -> SealedBytes {
    let (ciphertext, nonce) = encrypt_with_symmetric_key(secret_key, plaintext);
    let declared_len_bytes = plaintext.len() as u32;

    SealedBytes::Private {
        ciphertext,
        nonce,
        secret_version,
        declared_len_bytes,
    }
}

/// Unseals a SealedBytes value, returning the plaintext.
///
/// For Public variants, returns the value directly.
/// For Private variants, decrypts using the provided secret key.
///
/// # Arguments
///
/// * `sealed` - The SealedBytes to unseal.
/// * `secret_key` - The room secret (required for Private variants, ignored for Public).
///
/// # Returns
///
/// The plaintext data, or an error if decryption fails.
#[allow(dead_code)]
pub fn unseal_bytes(
    sealed: &SealedBytes,
    secret_key: Option<&[u8; 32]>,
) -> Result<Vec<u8>, String> {
    match sealed {
        SealedBytes::Public { value } => Ok(value.clone()),
        SealedBytes::Private {
            ciphertext,
            nonce,
            ..
        } => {
            let key = secret_key.ok_or("Secret key required to unseal private data")?;
            decrypt_with_symmetric_key(key, ciphertext, nonce)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{SigningKey, VerifyingKey};

    #[test]
    fn test_ecies_encryption_decryption() {
        let mut rng = OsRng;

        // Generate recipient's Ed25519 keypair
        let recipient_private_key = SigningKey::generate(&mut rng);
        let recipient_public_key: VerifyingKey = VerifyingKey::from(&recipient_private_key);

        // Encrypt the message
        let plaintext = b"Secret message";
        let (ciphertext, nonce, sender_public_key) = encrypt(&recipient_public_key, plaintext);

        // Decrypt the message
        let decrypted_message = decrypt(
            &recipient_private_key,
            &sender_public_key,
            &ciphertext,
            &nonce,
        );

        // Ensure the decrypted message matches the original
        assert_eq!(decrypted_message, plaintext);
    }

    #[test]
    fn test_symmetric_encryption_decryption() {
        let key = generate_room_secret();
        let plaintext = b"Room secret message";

        let (ciphertext, nonce) = encrypt_with_symmetric_key(&key, plaintext);
        let decrypted = decrypt_with_symmetric_key(&key, &ciphertext, &nonce)
            .expect("Decryption should succeed");

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_symmetric_decryption_wrong_key() {
        let key1 = generate_room_secret();
        let key2 = generate_room_secret();
        let plaintext = b"Room secret message";

        let (ciphertext, nonce) = encrypt_with_symmetric_key(&key1, plaintext);
        let result = decrypt_with_symmetric_key(&key2, &ciphertext, &nonce);

        assert!(result.is_err());
    }

    #[test]
    fn test_encrypt_decrypt_secret_for_member() {
        let mut rng = OsRng;
        let member_private_key = SigningKey::generate(&mut rng);
        let member_public_key = VerifyingKey::from(&member_private_key);

        let original_secret = generate_room_secret();

        // Encrypt secret for member
        let (ciphertext, nonce, ephemeral_key) =
            encrypt_secret_for_member(&original_secret, &member_public_key);

        // Decrypt secret
        let decrypted_secret = decrypt_secret_from_member_blob(
            &ciphertext,
            &nonce,
            &ephemeral_key,
            &member_private_key,
        )
        .expect("Decryption should succeed");

        assert_eq!(decrypted_secret, original_secret);
    }

    #[test]
    fn test_seal_unseal_bytes_private() {
        let secret_key = generate_room_secret();
        let plaintext = b"Private nickname";
        let secret_version = 5;

        // Seal the bytes
        let sealed = seal_bytes(plaintext, &secret_key, secret_version);

        // Verify it's the Private variant
        match &sealed {
            SealedBytes::Private {
                secret_version: v,
                declared_len_bytes,
                ..
            } => {
                assert_eq!(*v, secret_version);
                assert_eq!(*declared_len_bytes, plaintext.len() as u32);
            }
            _ => panic!("Expected Private variant"),
        }

        // Unseal the bytes
        let unsealed = unseal_bytes(&sealed, Some(&secret_key)).expect("Unseal should succeed");
        assert_eq!(unsealed, plaintext);
    }

    #[test]
    fn test_unseal_bytes_public() {
        let plaintext = b"Public nickname";
        let sealed = SealedBytes::Public {
            value: plaintext.to_vec(),
        };

        let unsealed = unseal_bytes(&sealed, None).expect("Unseal should succeed");
        assert_eq!(unsealed, plaintext);
    }

    #[test]
    fn test_unseal_private_without_key() {
        let secret_key = generate_room_secret();
        let plaintext = b"Private nickname";
        let sealed = seal_bytes(plaintext, &secret_key, 1);

        let result = unseal_bytes(&sealed, None);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("Secret key required to unseal private data"));
    }

    #[test]
    fn test_unseal_private_with_wrong_key() {
        let key1 = generate_room_secret();
        let key2 = generate_room_secret();
        let plaintext = b"Private nickname";
        let sealed = seal_bytes(plaintext, &key1, 1);

        let result = unseal_bytes(&sealed, Some(&key2));
        assert!(result.is_err());
    }
}
