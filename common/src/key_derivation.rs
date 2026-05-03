//! Deterministic derivation of room secrets from the owner's signing key.

use crate::room_state::privacy::SecretVersion;
use ed25519_dalek::VerifyingKey;

/// Derive a 32-byte room secret deterministically from the owner's signing
/// key seed, the room owner's verifying key, and the secret version.
///
/// Construction: blake3 keyed-hash with the signing key seed as the key.
/// blake3's keyed-hash mode is a secure PRF (HMAC-equivalent) and acceptable
/// as a KDF when the output size matches blake3's native 32 bytes.
///
/// Domain separation:
/// - `b"river-rotate-v1"` identifies the application protocol and version.
///   Must be a hard-coded constant per blake3 KDF guidance.
/// - `owner_vk` binds the secret to a specific room.
/// - `version` binds the secret to a specific rotation.
///
/// Determinism: identical inputs always produce identical 32-byte outputs.
/// Multiple replicas of the same delegate (e.g. running on different devices
/// for the same owner) compute byte-identical secrets without coordination.
pub fn derive_room_secret(
    signing_key_seed: &[u8; 32],
    owner_vk: &VerifyingKey,
    version: SecretVersion,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_keyed(signing_key_seed);
    hasher.update(b"river-rotate-v1");
    hasher.update(owner_vk.as_bytes());
    hasher.update(&version.to_le_bytes());
    *hasher.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn vk_from_seed(seed: [u8; 32]) -> VerifyingKey {
        SigningKey::from_bytes(&seed).verifying_key()
    }

    #[test]
    fn derive_is_deterministic() {
        let seed = [7u8; 32];
        let vk = vk_from_seed([1u8; 32]);
        let a = derive_room_secret(&seed, &vk, 0);
        let b = derive_room_secret(&seed, &vk, 0);
        assert_eq!(a, b, "identical inputs must produce identical outputs");
    }

    #[test]
    fn derive_separates_versions() {
        let seed = [7u8; 32];
        let vk = vk_from_seed([1u8; 32]);
        let v0 = derive_room_secret(&seed, &vk, 0);
        let v1 = derive_room_secret(&seed, &vk, 1);
        assert_ne!(v0, v1, "different versions must produce different outputs");
    }

    #[test]
    fn derive_separates_owners() {
        let seed = [7u8; 32];
        let vk_a = vk_from_seed([1u8; 32]);
        let vk_b = vk_from_seed([2u8; 32]);
        let a = derive_room_secret(&seed, &vk_a, 0);
        let b = derive_room_secret(&seed, &vk_b, 0);
        assert_ne!(a, b, "different owners must produce different outputs");
    }

    #[test]
    fn derive_separates_keys() {
        let seed_a = [7u8; 32];
        let seed_b = [8u8; 32];
        let vk = vk_from_seed([1u8; 32]);
        let a = derive_room_secret(&seed_a, &vk, 0);
        let b = derive_room_secret(&seed_b, &vk, 0);
        assert_ne!(
            a, b,
            "different signing key seeds must produce different outputs"
        );
    }

    /// Known-answer test that locks the construction in place. If this test
    /// fails, the derivation algorithm has changed and any deployed clients
    /// will compute incompatible secrets. Update the expected bytes ONLY when
    /// intentionally changing the construction (and treat that as a breaking
    /// protocol change requiring a new domain-separation tag).
    #[test]
    fn derive_known_answer_v1() {
        let seed = [0u8; 32];
        let vk = SigningKey::from_bytes(&[1u8; 32]).verifying_key();
        let actual = derive_room_secret(&seed, &vk, 0);
        let expected: [u8; 32] = [
            0x64, 0x1f, 0xfc, 0x73, 0x82, 0x69, 0x6a, 0x1b, 0xab, 0xd9, 0xeb, 0xa1, 0x7e, 0x48,
            0x4c, 0x06, 0x26, 0x55, 0x46, 0xc3, 0x5e, 0xf9, 0xed, 0x06, 0xa6, 0x89, 0xc4, 0x8e,
            0x20, 0x5b, 0x32, 0x6f,
        ];
        assert_eq!(
            actual, expected,
            "construction changed; KAT mismatch. Actual = {:02x?}",
            actual
        );
    }
}
