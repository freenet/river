//! ECIES helpers for River.
//!
//! The implementation lives in `river_core::ecies` so that the chat delegate
//! and the UI use byte-identical constructions for room-secret distribution.
//! This module re-exports the helpers under their historical names so existing
//! UI imports keep working.
#![allow(unused_imports)]

pub use river_core::ecies::{
    decrypt, decrypt_secret_from_member_blob, decrypt_secret_from_member_blob_raw,
    decrypt_with_symmetric_key, encrypt_secret_for_member, encrypt_with_symmetric_key,
    generate_room_secret, seal_bytes, unseal_bytes, unseal_bytes_with_secrets,
};

use river_core::room_state::privacy::SealedBytes;

/// Seal `plaintext` for publication into a room's encrypted state, or defer
/// the publish entirely when sealing would leak.
///
/// - Public room (any secret state) → `Some(SealedBytes::public(plaintext))`.
/// - Private room with a secret available → `Some(seal_bytes(...))`.
/// - **Private room with NO secret available → `None`.** The caller must skip
///   the publish rather than emit a plaintext `SealedBytes::public` into a
///   nominally encrypted room — that's the privacy leak from
///   freenet/river#299.
///
/// Three UI surfaces previously inlined the same `match` and all three had
/// the bug: `NicknameField::save_changes`, `RoomNameField::update_room_name`,
/// and `EditRoomModal::update_description`. Centralising the decision here
/// is the single source of truth so a future fourth surface inherits the
/// guard automatically.
pub fn seal_for_room(
    is_private: bool,
    current_secret_opt: Option<(&[u8; 32], u32)>,
    plaintext: Vec<u8>,
) -> Option<SealedBytes> {
    match (is_private, current_secret_opt) {
        (_, Some((secret, version))) => Some(seal_bytes(&plaintext, secret, version)),
        (false, None) => Some(SealedBytes::public(plaintext)),
        (true, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_room_with_no_secret_returns_public_sealed() {
        let out = seal_for_room(false, None, b"hello".to_vec())
            .expect("public rooms always seal as public, never defer");
        assert!(!out.is_private());
        assert_eq!(out.to_string_lossy(), "hello");
    }

    #[test]
    fn public_room_with_secret_uses_secret_for_consistency() {
        // A public room nominally has no secret, but if one were ever supplied
        // we still seal with it rather than emitting plaintext — defensive,
        // since a misconfigured caller passing both should never produce a
        // weaker output than the explicit "no secret" branch.
        let secret = [7u8; 32];
        let out = seal_for_room(false, Some((&secret, 3)), b"hi".to_vec())
            .expect("a secret-bearing case always returns Some");
        assert!(out.is_private(), "secret available → sealed");
    }

    #[test]
    fn private_room_with_secret_seals() {
        let secret = [9u8; 32];
        let out = seal_for_room(true, Some((&secret, 1)), b"alice".to_vec())
            .expect("private+secret must seal, not defer");
        assert!(out.is_private(), "private+secret → encrypted SealedBytes");
        // Verify it can be decrypted back.
        let plaintext =
            unseal_bytes(&out, Some(&secret)).expect("freshly-sealed bytes must round-trip");
        assert_eq!(plaintext, b"alice");
    }

    #[test]
    fn private_room_without_secret_defers() {
        // The freenet/river#299 regression case: must return None so the
        // caller skips the publish, NEVER fall through to
        // SealedBytes::public.
        assert!(
            seal_for_room(true, None, b"top secret".to_vec()).is_none(),
            "private room with no secret MUST defer (return None), \
             never emit plaintext SealedBytes::public"
        );
    }
}
