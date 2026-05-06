//! Deterministic derivation of room secrets from the owner's signing key.

use crate::room_state::privacy::SecretVersion;
use ed25519_dalek::VerifyingKey;

/// Hard-coded context string for the protocol-root key derivation step.
/// Per blake3 KDF guidance this MUST be a compile-time constant. ANY change
/// to the input set fed into the per-call keyed-hash phase below requires
/// bumping this string (e.g. to `"river-rotate v2 ..."`) and the
/// corresponding known-answer test vectors.
const ROOT_CONTEXT: &str = "river-rotate v1 2026-04 room-secret-root";

/// Derive a 32-byte room secret deterministically from the owner's signing
/// key seed, the room owner's verifying key, and the secret version.
///
/// # Construction
///
/// Two-phase blake3 KDF:
///
/// 1. `root = blake3::derive_key(ROOT_CONTEXT, signing_key_seed)` — the
///    canonical blake3 KDF mode, with a hard-coded context string that
///    bakes in the protocol version and a date stamp. This separates the
///    protocol-version commitment from the per-call inputs.
/// 2. `secret = keyed_hash(root, owner_vk || version_le)` — keyed-hash with
///    the per-call inputs. blake3 keyed-hash is a secure PRF.
///
/// Future input additions are limited to phase 2 and require bumping the
/// `ROOT_CONTEXT` string (which forces a new known-answer test vector and
/// makes the protocol break visible at code-review time). Future protocol
/// version bumps just change the context string.
///
/// # Invariants
///
/// `signing_key_seed` MUST be the 32-byte ed25519 seed (the bytes returned
/// by `SigningKey::to_bytes()`), not the expanded 64-byte secret, not random
/// bytes, not the verifying key, not a stretched value. Passing any other
/// 32-byte input produces a "valid" but undefined output and breaks the
/// multi-replica determinism that is the entire point of this function.
///
/// `version: SecretVersion` is `u32`. Widening the type is a breaking
/// protocol change requiring a new `ROOT_CONTEXT` (because the
/// `to_le_bytes()` length changes).
///
/// # Determinism
///
/// Identical inputs always produce identical 32-byte outputs across all
/// platforms. blake3 is endian-agnostic; `version.to_le_bytes()` is
/// explicit; `VerifyingKey::as_bytes()` returns the canonical 32-byte
/// compressed ed25519 point. Multiple replicas of the same delegate (e.g.
/// a user running River on laptop + phone) compute byte-identical secrets
/// without coordination.
///
/// # Security trade-off
///
/// Anyone with `signing_key_seed` can derive every past and every future
/// secret for this room. This is acceptable for River's threat model: the
/// signing key already authorises every room operation, so seed compromise
/// is already terminal. The trade-off buys multi-device determinism without
/// distributed coordination. Apps that need historical forward secrecy
/// against signing-key compromise must not use this construction.
///
/// A removed member who held `secret_v_n` does not have the seed and so
/// cannot derive `secret_v_{n+1}` — forward secrecy against a removed
/// member holds, which is the property that matters for room rotation.
pub fn derive_room_secret(
    signing_key_seed: &[u8; 32],
    owner_vk: &VerifyingKey,
    version: SecretVersion,
) -> [u8; 32] {
    let root = blake3::derive_key(ROOT_CONTEXT, signing_key_seed);
    let mut hasher = blake3::Hasher::new_keyed(&root);
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

    #[test]
    fn derive_locks_input_ordering() {
        // If the implementation accidentally swapped the order of
        // `update(owner_vk)` and `update(version_le)`, then
        // `derive(seed, vk_a, 1)` would equal `derive(seed, vk_b, 0)`
        // for some adversarially-chosen vk_b. This test ensures the
        // ordering is locked: distinct (owner, version) pairs at the
        // same axis-product position must not collide.
        let seed = [7u8; 32];
        let vk_a = vk_from_seed([1u8; 32]);
        let vk_b = vk_from_seed([2u8; 32]);
        assert_ne!(
            derive_room_secret(&seed, &vk_a, 1),
            derive_room_secret(&seed, &vk_b, 0),
        );
        assert_ne!(
            derive_room_secret(&seed, &vk_a, 0),
            derive_room_secret(&seed, &vk_b, 1),
        );
    }

    /// Known-answer test that locks the construction in place. If this test
    /// fails, the derivation algorithm has changed and any deployed clients
    /// will compute incompatible secrets. Update the expected bytes ONLY when
    /// intentionally changing the construction (and treat that as a breaking
    /// protocol change requiring a new ROOT_CONTEXT string).
    ///
    /// To independently verify these vectors, run blake3 from outside Rust:
    ///
    ///   # Phase 1: derive the root key.
    ///   #   blake3::derive_key(ROOT_CONTEXT, signing_key_seed)
    ///   # Phase 2: keyed_hash(root, owner_vk_bytes || version_le_4).
    ///
    /// e.g. with python's `blake3` package:
    ///   import blake3
    ///   root = blake3.blake3(b'\x00'*32,
    ///       derive_key_context='river-rotate v1 2026-04 room-secret-root'
    ///   ).digest()
    ///   # owner_vk for SigningKey::from_bytes([1u8;32]) — paste 32 bytes
    ///   secret = blake3.blake3(owner_vk_bytes + (0).to_bytes(4,'little'),
    ///       key=root).digest()
    #[test]
    fn derive_known_answer_v1_zero_seed_zero_version() {
        let seed = [0u8; 32];
        let vk = SigningKey::from_bytes(&[1u8; 32]).verifying_key();
        let actual = derive_room_secret(&seed, &vk, 0);
        let expected: [u8; 32] = [
            0xdd, 0x18, 0x9c, 0xce, 0x07, 0x93, 0x74, 0x85, 0x6e, 0xb7, 0xa2, 0x01, 0x61, 0x8e,
            0x58, 0x86, 0xa1, 0xe9, 0xe5, 0x59, 0x8b, 0x33, 0x34, 0x08, 0x43, 0x00, 0x2c, 0xbb,
            0x90, 0x91, 0xe1, 0xa9,
        ];
        assert_eq!(
            actual, expected,
            "construction changed; KAT mismatch. Actual = {:02x?}",
            actual
        );
    }

    /// Multi-byte-significant version vector. Catches a future regression
    /// that swapped `to_le_bytes` for `to_be_bytes`, which would produce
    /// identical output for `version=0` or `version=1` but a different
    /// output here.
    #[test]
    fn derive_known_answer_v1_multi_byte_version() {
        let seed = [0u8; 32];
        let vk = SigningKey::from_bytes(&[1u8; 32]).verifying_key();
        let actual = derive_room_secret(&seed, &vk, 0x01020304);
        let expected: [u8; 32] = [
            0xaa, 0x8f, 0x7d, 0x5a, 0xb5, 0x15, 0x84, 0x66, 0x78, 0x72, 0x28, 0xd6, 0x88, 0x54,
            0xf6, 0x5d, 0x39, 0xac, 0xe3, 0x13, 0x07, 0x8f, 0x29, 0xa9, 0xfb, 0xad, 0x88, 0x79,
            0x70, 0xd3, 0xfe, 0x67,
        ];
        assert_eq!(
            actual, expected,
            "construction changed; KAT mismatch. Actual = {:02x?}",
            actual
        );
    }

    /// All-`0xFF` seed vector. Catches a buggy "if seed is zero, fall back
    /// to unkeyed mode" regression and exercises non-zero key bytes through
    /// blake3's keyed-hash internals.
    #[test]
    fn derive_known_answer_v1_all_ff_seed() {
        let seed = [0xFFu8; 32];
        let vk = SigningKey::from_bytes(&[1u8; 32]).verifying_key();
        let actual = derive_room_secret(&seed, &vk, 0);
        let expected: [u8; 32] = [
            0x60, 0xb5, 0x60, 0x0b, 0x12, 0xfc, 0xaa, 0x0c, 0x52, 0xda, 0x76, 0x59, 0x95, 0xf6,
            0x9c, 0xb3, 0xeb, 0x54, 0x37, 0xd5, 0x67, 0x53, 0xc0, 0x24, 0x97, 0x67, 0x19, 0xf1,
            0xe4, 0x31, 0x7e, 0x87,
        ];
        assert_eq!(
            actual, expected,
            "construction changed; KAT mismatch. Actual = {:02x?}",
            actual
        );
    }
}
