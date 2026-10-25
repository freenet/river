//! Per-viewer avatar colour salt.
//!
//! Each message author's avatar is coloured per VIEWER: the hue is
//! [`avatar_hue`]`(salt, member_vk)`, a BLAKE3 keyed hash under a salt only
//! this viewer knows. An impersonator therefore cannot pick a key whose avatar
//! colour matches someone else's, because they cannot compute anyone's colours.
//!
//! The salt comes from the chat delegate (`ChatDelegateRequestMsg::GetAvatarSalt`),
//! which derives it from this viewer's member signing key for the room, so the
//! display code never touches the private key. There is deliberately no
//! fallback to `RoomData::self_sk`: a room whose salt is absent (the delegate
//! did not answer, or holds no key for it) renders neutral avatars.

use crate::components::app::chat_delegate::{generate_request_id, send_delegate_request};
use dioxus::logger::tracing::warn;
use dioxus::prelude::*;
use river_core::chat_delegate::{ChatDelegateRequestMsg, ChatDelegateResponseMsg, RoomKey};
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};

/// This viewer's avatar colour salt per room (keyed by the room owner's
/// verifying-key bytes), filled by [`ensure_avatar_salt`]. A room with no
/// entry renders neutral avatars.
///
/// Read it with `try_read()`, and in a memo or effect with the
/// `util::signal_guard` anchor and nudge (dioxus-signal-safety.md).
pub static AVATAR_SALTS: GlobalSignal<HashMap<RoomKey, [u8; 32]>> = Global::new(HashMap::new);

/// Rooms whose salt has been requested this session. A `Mutex`, not a signal,
/// so [`ensure_avatar_salt`] can claim a room synchronously, from render,
/// without writing a signal.
static AVATAR_SALT_REQUESTED: LazyLock<Mutex<HashSet<RoomKey>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// `true` the first time it is called for `room_key` this session.
fn claim_avatar_salt_fetch(room_key: RoomKey) -> bool {
    AVATAR_SALT_REQUESTED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(room_key)
}

/// Fetch this viewer's avatar salt for `room_key` from the delegate, at most
/// once per room per session, and store it in [`AVATAR_SALTS`].
///
/// Safe to call from render: it writes no signal synchronously; the store is
/// deferred. A failed or unanswered fetch is not retried this session and
/// leaves the room's salt absent.
pub fn ensure_avatar_salt(room_key: RoomKey) {
    if !claim_avatar_salt_fetch(room_key) {
        return;
    }
    crate::util::safe_spawn_local(async move {
        match fetch_avatar_salt(room_key).await {
            Ok(salt) => crate::util::defer(move || {
                AVATAR_SALTS.write().insert(room_key, salt);
            }),
            Err(e) => warn!("No avatar salt for room {:?}: {}", room_key, e),
        }
    });
}

/// Ask the chat delegate for this viewer's avatar salt for `room_key`.
pub async fn fetch_avatar_salt(room_key: RoomKey) -> Result<[u8; 32], String> {
    let request = ChatDelegateRequestMsg::GetAvatarSalt {
        room_key,
        request_id: generate_request_id(),
    };
    extract_avatar_salt(send_delegate_request(request).await)
}

/// Extract the salt from a delegate response.
fn extract_avatar_salt(
    response: Result<ChatDelegateResponseMsg, String>,
) -> Result<[u8; 32], String> {
    match response {
        Ok(ChatDelegateResponseMsg::AvatarSaltResponse { salt, .. }) => salt,
        Ok(other) => Err(format!("Unexpected response: {:?}", other)),
        Err(e) => Err(e),
    }
}

/// The avatar hue (`0..360`) this viewer gives the member with verifying-key
/// bytes `member_vk_bytes`: a BLAKE3 keyed hash of those bytes under `salt`,
/// its first 8 bytes read little-endian, reduced mod 360.
pub fn avatar_hue(salt: &[u8; 32], member_vk_bytes: &[u8]) -> u16 {
    let hash = blake3::keyed_hash(salt, member_vk_bytes);
    let mut head = [0u8; 8];
    head.copy_from_slice(&hash.as_bytes()[..8]);
    (u64::from_le_bytes(head) % 360) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the construction (keyed hash, byte order, reduction). Expected
    /// value computed independently: `b3sum --keyed` over 32 bytes of 0x02
    /// with a key of 32 bytes of 0x01, first 8 bytes little-endian, mod 360.
    #[test]
    fn avatar_hue_golden_vector() {
        assert_eq!(avatar_hue(&[1u8; 32], &[2u8; 32]), 262);
    }

    #[test]
    fn avatar_hue_is_in_range_and_deterministic() {
        let salt = [7u8; 32];
        for i in 0..=255u8 {
            let vk = [i; 32];
            let hue = avatar_hue(&salt, &vk);
            assert!(hue < 360, "hue {hue} out of range");
            assert_eq!(hue, avatar_hue(&salt, &vk));
        }
    }

    /// The point of the salt: another viewer's salt gives a different
    /// colour mapping. Checked over many members, since any one hue can
    /// collide by chance.
    #[test]
    fn avatar_hue_depends_on_salt_and_member() {
        let members: Vec<[u8; 32]> = (0..64u8).map(|i| [i; 32]).collect();
        let a: Vec<u16> = members.iter().map(|m| avatar_hue(&[1u8; 32], m)).collect();
        let b: Vec<u16> = members.iter().map(|m| avatar_hue(&[2u8; 32], m)).collect();
        assert_ne!(a, b, "a different salt must change the mapping");
        let distinct: HashSet<u16> = a.iter().copied().collect();
        assert!(
            distinct.len() > 32,
            "64 members should spread over many hues, got {}",
            distinct.len()
        );
    }

    #[test]
    fn extract_avatar_salt_passes_the_delegate_answer_through() {
        let ok = ChatDelegateResponseMsg::AvatarSaltResponse {
            room_key: [3u8; 32],
            request_id: 1,
            salt: Ok([9u8; 32]),
        };
        assert_eq!(extract_avatar_salt(Ok(ok)), Ok([9u8; 32]));

        let refused = ChatDelegateResponseMsg::AvatarSaltResponse {
            room_key: [3u8; 32],
            request_id: 1,
            salt: Err("Signing key not found for this room".into()),
        };
        assert!(extract_avatar_salt(Ok(refused)).is_err());

        let other = ChatDelegateResponseMsg::GetPublicKeyResponse {
            room_key: [3u8; 32],
            public_key: None,
        };
        assert!(extract_avatar_salt(Ok(other)).is_err());
        assert!(extract_avatar_salt(Err("timeout".into())).is_err());
    }

    #[test]
    fn a_room_is_claimed_for_fetching_once_per_session() {
        let room = [0xA7u8; 32];
        let other = [0xA8u8; 32];
        assert!(claim_avatar_salt_fetch(room));
        assert!(!claim_avatar_salt_fetch(room));
        assert!(claim_avatar_salt_fetch(other));
    }
}
