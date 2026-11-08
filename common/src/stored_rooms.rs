//! Read-side types for the rooms the web UI keeps in the chat delegate.
//!
//! The web UI stores each room under its own delegate key,
//! `room:<base58(owner_vk)>`, as a CBOR [`StoredRoomSlot`], and the room list's
//! view preferences under `rooms_meta` as a CBOR [`StoredRoomsMeta`]. The
//! writer's types are `RoomSlot`, `RoomData` and `RoomsMeta` in
//! `ui/src/room_data.rs`. These types decode the same bytes but keep only the
//! fields another client (the native app) needs to show the rooms, so they can
//! live here without the web UI's dependencies.
//!
//! They are for READING only. They derive no `Serialize` on purpose: writing a
//! slot built from this subset would drop every field the web UI stores and
//! this type does not name.
//!
//! Field names and `#[serde(default)]` attributes must match the writer's.
//! Unknown fields are ignored, so the web UI can add fields freely. Renaming or
//! removing one of the fields below breaks this reader, which is what the drift
//! test in the `ui` crate (`stored_rooms_drift` in `ui/src/room_data.rs`)
//! catches.
//!
//! Gated on the `stored-rooms` feature, a client-only feature like `migration`
//! and `mentions`: the room-contract and chat-delegate WASM builds do not
//! enable it, so their bytes and keys are unchanged.

use crate::room_state::member::AuthorizedMember;
use crate::room_state::member_info::AuthorizedMemberInfo;
use crate::room_state::message::MessageId;
use crate::ChatRoomStateV1;
use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_stdlib::prelude::ContractKey;
use serde::Deserialize;
use std::collections::BTreeMap;

/// The delegate key holding the room list's view preferences.
pub const ROOMS_META_KEY: &[u8] = b"rooms_meta";

/// The prefix of a per-room delegate key: `room:<base58(owner_vk)>`.
pub const ROOM_KEY_PREFIX: &str = "room:";

/// The per-room delegate key for the room owned by `owner_vk`.
pub fn room_storage_key(owner_vk: &VerifyingKey) -> Vec<u8> {
    format!(
        "{ROOM_KEY_PREFIX}{}",
        bs58::encode(owner_vk.to_bytes()).into_string()
    )
    .into_bytes()
}

/// The owner's verifying key in a per-room delegate key, or `None` if `key` is
/// not a well-formed `room:<base58(vk)>` key.
pub fn parse_room_storage_key(key: &[u8]) -> Option<VerifyingKey> {
    let s = std::str::from_utf8(key).ok()?;
    let b58 = s.strip_prefix(ROOM_KEY_PREFIX)?;
    let bytes = bs58::decode(b58).into_vec().ok()?;
    let arr: [u8; 32] = bytes.try_into().ok()?;
    VerifyingKey::from_bytes(&arr).ok()
}

/// What is stored under `room:<base58(owner_vk)>` (the web UI's `RoomSlot`).
#[derive(Clone, Deserialize)]
pub enum StoredRoomSlot {
    /// The user is in this room.
    Present(Box<StoredRoom>),
    /// The user left this room. Skip it.
    Tombstone,
}

/// One room as the web UI stores it (a subset of its `RoomData`).
#[derive(Clone, Deserialize)]
pub struct StoredRoom {
    pub owner_vk: VerifyingKey,
    pub room_state: ChatRoomStateV1,
    /// The local user's signing key in this room. `None` in a blob written by a
    /// future build that no longer stores it here.
    #[serde(default)]
    pub self_sk: Option<SigningKey>,
    /// The public half of `self_sk`. `None` in blobs written before the field
    /// existed.
    #[serde(default)]
    pub self_vk: Option<VerifyingKey>,
    /// The room contract the web UI last synced this room under.
    pub contract_key: ContractKey,
    /// The last message the user read in this room.
    #[serde(default)]
    pub last_read_message_id: Option<MessageId>,
    /// The nickname the local user chose for this room, in plaintext.
    #[serde(default)]
    pub self_nickname: Option<String>,
    /// Room secrets carried by the invitation, by version. Used for any
    /// version the room state holds no owner-signed blob for.
    #[serde(default)]
    pub invitation_secrets: BTreeMap<u32, [u8; 32]>,
    /// The owner-signed record that admitted the local user, kept so a member
    /// the room pruned for inactivity can re-add themselves when they next
    /// send (the web UI's `build_rejoin_delta`). `None` for the owner and in
    /// older blobs.
    #[serde(default)]
    pub self_authorized_member: Option<AuthorizedMember>,
    /// The local user's invite ancestry up to the owner, re-added with them
    /// when they rejoin.
    #[serde(default)]
    pub invite_chain: Vec<AuthorizedMember>,
    /// The local user's last published nickname record, republished when they
    /// rejoin.
    #[serde(default)]
    pub self_member_info: Option<AuthorizedMemberInfo>,
}

impl StoredRoom {
    /// The local user's verifying key, preferring the signing key when held,
    /// as the web UI's `RoomData::self_verifying_key` does.
    pub fn self_verifying_key(&self) -> Option<VerifyingKey> {
        match &self.self_sk {
            Some(sk) => Some(sk.verifying_key()),
            None => self.self_vk,
        }
    }
}

/// What is stored under `rooms_meta` (a subset of the web UI's `RoomsMeta`).
#[derive(Clone, Default, Deserialize)]
pub struct StoredRoomsMeta {
    /// The room the web UI had open.
    #[serde(default)]
    pub current_room_key: Option<VerifyingKey>,
    /// The order of the room list.
    #[serde(default)]
    pub room_order: Vec<VerifyingKey>,
}

/// Decode a per-room slot.
pub fn decode_room_slot(bytes: &[u8]) -> Result<StoredRoomSlot, String> {
    ciborium::de::from_reader(bytes).map_err(|e| format!("room slot: {e}"))
}

/// Decode the room list's view preferences.
pub fn decode_rooms_meta(bytes: &[u8]) -> Result<StoredRoomsMeta, String> {
    ciborium::de::from_reader(bytes).map_err(|e| format!("rooms_meta: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vk(seed: u8) -> VerifyingKey {
        SigningKey::from_bytes(&[seed; 32]).verifying_key()
    }

    #[test]
    fn room_storage_key_round_trips_and_rejects_malformed() {
        let owner = vk(7);
        let key = room_storage_key(&owner);
        assert!(key.starts_with(b"room:"));
        assert_eq!(parse_room_storage_key(&key), Some(owner));
        assert_eq!(parse_room_storage_key(ROOMS_META_KEY), None);
        assert_eq!(parse_room_storage_key(b"room:"), None);
        assert_eq!(parse_room_storage_key(b"room:0OIl"), None);
        assert_eq!(parse_room_storage_key(b"rooms_data"), None);
        // A base58 string of the wrong length.
        assert_eq!(parse_room_storage_key(b"room:2g"), None);
    }

    #[test]
    fn a_tombstone_decodes() {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&"Tombstone", &mut buf).unwrap();
        assert!(matches!(
            decode_room_slot(&buf),
            Ok(StoredRoomSlot::Tombstone)
        ));
    }

    #[test]
    fn meta_tolerates_unknown_and_missing_fields() {
        #[derive(serde::Serialize)]
        struct Future {
            room_order: Vec<VerifyingKey>,
            something_new: u32,
        }
        let mut buf = Vec::new();
        ciborium::ser::into_writer(
            &Future {
                room_order: vec![vk(1), vk(2)],
                something_new: 9,
            },
            &mut buf,
        )
        .unwrap();
        let meta = decode_rooms_meta(&buf).unwrap();
        assert_eq!(meta.room_order, vec![vk(1), vk(2)]);
        assert_eq!(meta.current_room_key, None);
    }
}
