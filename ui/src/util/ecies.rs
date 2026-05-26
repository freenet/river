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
/// the publish entirely when sealing would leak. **The caller MUST treat
/// `None` as "skip publish"** — never substitute a public seal.
///
/// The decision is keyed primarily on `is_private`, not on the cached secret:
///
/// - Public room → `Some(SealedBytes::public(plaintext))`, regardless of any
///   cached secret. A stale or stray secret cached for a public room must not
///   silently encrypt content other viewers cannot read.
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
        (false, _) => Some(SealedBytes::public(plaintext)),
        (true, Some((secret, version))) => Some(seal_bytes(&plaintext, secret, version)),
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
    fn public_room_ignores_stray_secret_and_returns_public() {
        // A public room has no secret by definition. If one is somehow
        // cached (legacy state, bug elsewhere) the helper must NOT seal with
        // it — sealing would produce content unreadable to public-room
        // viewers, breaking the room. Public-room edits always publish as
        // public.
        let secret = [7u8; 32];
        let out = seal_for_room(false, Some((&secret, 3)), b"hi".to_vec())
            .expect("public rooms always return Some");
        assert!(
            !out.is_private(),
            "public room with a stray cached secret MUST still seal as public"
        );
        assert_eq!(out.to_string_lossy(), "hi");
    }

    #[test]
    fn private_room_with_secret_seals() {
        let secret = [9u8; 32];
        let out = seal_for_room(true, Some((&secret, 1)), b"alice".to_vec())
            .expect("private+secret must seal, not defer");
        assert!(out.is_private(), "private+secret → encrypted SealedBytes");
        // Belt-and-braces: the sealed bytes' on-wire representation must
        // not contain the plaintext as a substring — even on the to-be-
        // displayed path. Defends against a future refactor that lets the
        // plaintext leak through `to_string_lossy` while still returning a
        // structurally `Private` variant.
        assert!(
            !out.to_string_lossy().contains("alice"),
            "sealed bytes must not surface plaintext via to_string_lossy"
        );
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

    /// Source-grep pin: every UI edit-delta site that produces a
    /// `SealedBytes` destined for the network MUST go through
    /// `seal_for_room`. A future refactor that re-inlines the
    /// `SealedBytes::public(...)` fallback would resurrect the
    /// freenet/river#299 plaintext leak — this test fails first.
    ///
    /// The check is two-fold: (a) `seal_for_room(` is referenced in each
    /// known edit-delta file, and (b) `SealedBytes::public(` does NOT
    /// appear directly in those files (the helper is the only legal path
    /// to a public-sealed edit delta).
    #[test]
    fn seal_for_room_call_sites_pinned() {
        let nickname_src =
            include_str!("../components/members/member_info_modal/nickname_field.rs");
        let room_name_src = include_str!("../components/room_list/room_name_field.rs");
        let edit_room_src = include_str!("../components/room_list/edit_room_modal.rs");

        for (name, src) in [
            ("nickname_field.rs", nickname_src),
            ("room_name_field.rs", room_name_src),
            ("edit_room_modal.rs", edit_room_src),
        ] {
            assert!(
                src.contains("seal_for_room("),
                "{name}: expected at least one `seal_for_room(...)` call — \
                 the helper is the single privacy gate for sealed-for-network \
                 writes; if you removed it, restore the call",
            );
            assert!(
                !src.contains("SealedBytes::public("),
                "{name}: must NOT call `SealedBytes::public(...)` directly — \
                 route through `seal_for_room` instead (freenet/river#299)",
            );
        }
    }
}
