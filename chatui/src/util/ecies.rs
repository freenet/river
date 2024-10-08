use ed25519_dalek::{SigningKey, VerifyingKey};
use x25519_dalek::{PublicKey as X25519PublicKey, EphemeralSecret as X25519PrivateKey};
use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use rand::rngs::OsRng;
use sha2::{Sha256, Sha512, Digest};
use curve25519_dalek::edwards::CompressedEdwardsY;

/// Encrypts a plaintext message using ECIES (Elliptic Curve Integrated Encryption Scheme).
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
pub fn encrypt(recipient_public_key: &VerifyingKey, plaintext: &[u8]) -> (Vec<u8>, [u8; 12], VerifyingKey) {
    // Generate an ephemeral keypair
    let sender_private_key = X25519PrivateKey::new(OsRng);
    let sender_public_key = sender_private_key.public_key();

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
    let ciphertext = cipher.encrypt(&Nonce::from(nonce), plaintext)
        .expect("encryption failure!");

    (ciphertext, nonce, VerifyingKey::from_bytes(&sender_public_key.to_bytes()).expect("Failed to convert to VerifyingKey"))
}

fn ed25519_to_x25519_public_key(ed25519_pk: &VerifyingKey) -> X25519PublicKey {
    let ed_y = CompressedEdwardsY(ed25519_pk.to_bytes()).decompress().expect("Invalid Edwards point");
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
pub fn decrypt(recipient_private_key: &SigningKey, sender_public_key: &VerifyingKey, ciphertext: &[u8], nonce: &[u8; 12]) -> Vec<u8> {
    // Convert Ed25519 signing key to X25519 private key
    let recipient_x25519_private_key = ed25519_to_x25519_private_key(recipient_private_key);

    // Convert Ed25519 verifying key to X25519 public key
    let sender_x25519_public_key = ed25519_to_x25519_public_key(sender_public_key);

    // Derive shared secret using recipient's private key and sender's public key
    let shared_secret = recipient_x25519_private_key.diffie_hellman(&sender_x25519_public_key);

    // Use the shared secret to derive the symmetric key
    let symmetric_key = Sha256::digest(shared_secret.as_bytes());

    // Decrypt the ciphertext using AES-GCM
    let cipher = Aes256Gcm::new_from_slice(&symmetric_key).expect("Failed to create cipher");
    let decrypted_message = cipher.decrypt(&Nonce::from(*nonce), ciphertext.as_ref())
        .expect("decryption failure!");

    decrypted_message
}

fn ed25519_to_x25519_private_key(ed25519_sk: &SigningKey) -> X25519PrivateKey {
    let h = Sha512::digest(ed25519_sk.to_bytes());
    let mut key = [0u8; 32];
    key.copy_from_slice(&h[..32]);
    key[0] &= 248;
    key[31] &= 127;
    key[31] |= 64;
    X25519PrivateKey::from(key)
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
        let decrypted_message = decrypt(&recipient_private_key, &sender_public_key, &ciphertext, &nonce);

        // Ensure the decrypted message matches the original
        assert_eq!(decrypted_message, plaintext);
    }
}
