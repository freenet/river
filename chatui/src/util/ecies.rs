use ed25519_dalek::{SigningKey, VerifyingKey};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret as X25519PrivateKey};
use aes_gcm::{Aes256Gcm, Key, Nonce}; // AES-GCM encryption
use aes_gcm::aead::{Aead, NewAead};
use rand::rngs::OsRng;
use sha2::{Sha256, Digest};
use rand::Rng;

pub fn encrypt(recipient_public_key: &VerifyingKey, plaintext: &[u8]) -> (Vec<u8>, [u8; 12], VerifyingKey) {
    // Generate an ephemeral keypair
    let sender_private_key = X25519PrivateKey::new(&mut OsRng);
    let sender_public_key = X25519PublicKey::from(&sender_private_key);

    // Convert Ed25519 verifying key to X25519 public key
    let recipient_public_key_bytes = recipient_public_key.to_bytes();
    let recipient_public_key = X25519PublicKey::from(&recipient_public_key_bytes[..32]);

    // Derive shared secret using sender's private key and recipient's public key
    let shared_secret = sender_private_key.diffie_hellman(&recipient_public_key);

    // Derive a symmetric key from the shared secret (using SHA-256 for KDF)
    let symmetric_key = Sha256::digest(shared_secret.as_bytes());

    // Encrypt the message using AES-GCM
    let cipher = Aes256Gcm::new(Key::from_slice(&symmetric_key));
    let nonce: [u8; 12] = OsRng.gen(); // Generate a random nonce (12 bytes)

    let ciphertext = cipher.encrypt(Nonce::from_slice(&nonce), plaintext)
        .expect("encryption failure!");

    (ciphertext, nonce, recipient_public_key.clone())
}

pub fn decrypt(recipient_private_key: &SigningKey, sender_public_key: &VerifyingKey, ciphertext: &[u8], nonce: &[u8; 12]) -> Vec<u8> {
    // Convert Ed25519 signing key to X25519 private key
    let recipient_private_key_bytes = recipient_private_key.to_bytes();
    let recipient_private_key = X25519PrivateKey::from(&recipient_private_key_bytes[..32]);

    // Convert Ed25519 verifying key to X25519 public key
    let sender_public_key_bytes = sender_public_key.to_bytes();
    let sender_public_key = X25519PublicKey::from(&sender_public_key_bytes[..32]);

    // Derive shared secret using recipient's private key and sender's public key
    let shared_secret = recipient_private_key.diffie_hellman(&sender_public_key);

    // Derive the symmetric key from the shared secret
    let symmetric_key = Sha256::digest(shared_secret.as_bytes());

    // Decrypt the message using AES-GCM
    let cipher = Aes256Gcm::new(Key::from_slice(&symmetric_key));
    let decrypted_message = cipher.decrypt(Nonce::from_slice(nonce), ciphertext)
        .expect("decryption failure!");

    decrypted_message
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