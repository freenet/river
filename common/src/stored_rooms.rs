//! The rooms the web UI keeps in the chat delegate, for clients other than the
//! web UI (the native app): reading them, and adding a room to them.
//!
//! The web UI stores each room under its own delegate key,
//! `room:<base58(owner_vk)>`, as a CBOR [`StoredRoomSlot`], and the room list's
//! view preferences under `rooms_meta` as a CBOR [`StoredRoomsMeta`]. The
//! writer's types are `RoomSlot`, `RoomData` and `RoomsMeta` in
//! `ui/src/room_data.rs`. The read types decode the same bytes but keep only the
//! fields another client needs to show the rooms, so they can live here without
//! the web UI's dependencies.
//!
//! The read types derive no `Serialize` on purpose: writing a slot built from
//! that subset would drop every field the web UI stores and they do not name.
//! The write side is narrower than the web UI's on purpose too. It can only ADD
//! a room: [`reconcile_new_room`] stores a [`NewRoom`] under a key that holds
//! nothing (or a tombstone, for an explicit rejoin), and never replaces a room
//! the store already holds. And it can only point `rooms_meta`'s open room at it
//! ([`meta_with_current_room`]), editing that one field of the stored CBOR and
//! keeping every other field, known or not, as it was. Both are meant to run
//! inside the web UI's own generation compare-and-swap loop
//! (`GetVersionedRequest`, then `CasStoreRequest`, reading again on a
//! conflict), which is what makes them safe beside a web UI writing at the same
//! time.
//!
//! Field names and `#[serde(default)]` attributes must match the writer's.
//! Unknown fields are ignored, so the web UI can add fields freely. Renaming or
//! removing one of the fields below breaks this reader, and a [`NewRoom`] the
//! web UI could not decode would lose the room: the drift tests in the `ui`
//! crate (`stored_rooms_drift` in `ui/src/room_data.rs`) catch both.
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
use serde::{Deserialize, Serialize};
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

/// A room this client adds to the store: the web UI's `RoomData` fields a new
/// room has, by the web UI's names. The fields it leaves out
/// (`previous_contract_key`, `rekey_merge`, `own_purges`) are
/// `#[serde(default)]` in `RoomData`, and their defaults are what a room the web
/// UI just created or joined holds.
///
/// No `Debug` on purpose: the value holds the user's signing key and any room
/// secrets.
#[derive(Clone, Serialize)]
pub struct NewRoom {
    pub owner_vk: VerifyingKey,
    pub room_state: ChatRoomStateV1,
    pub self_sk: Option<SigningKey>,
    /// Set by this writer. The web UI fills it in at its save chokepoint
    /// (`RoomData::backfill_self_vk`), so a room it saved holds it too.
    pub self_vk: Option<VerifyingKey>,
    pub contract_key: ContractKey,
    pub last_read_message_id: Option<MessageId>,
    pub self_authorized_member: Option<AuthorizedMember>,
    pub invite_chain: Vec<AuthorizedMember>,
    pub self_member_info: Option<AuthorizedMemberInfo>,
    pub self_nickname: Option<String>,
    pub invitation_secrets: BTreeMap<u32, [u8; 32]>,
}

impl NewRoom {
    /// The identity this room is stored under.
    pub fn self_verifying_key(&self) -> Option<VerifyingKey> {
        match &self.self_sk {
            Some(sk) => Some(sk.verifying_key()),
            None => self.self_vk,
        }
    }
}

/// The web UI's `RoomSlot::Present`, for a [`NewRoom`]. Serde names the
/// variant, so these are bytes `RoomSlot` decodes.
#[derive(Serialize)]
enum NewRoomSlot<'a> {
    Present(&'a NewRoom),
}

/// The CBOR of `room` as a present slot.
pub fn encode_new_room(room: &NewRoom) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(&NewRoomSlot::Present(room), &mut out)
        .map_err(|e| format!("room slot: {e}"))?;
    Ok(out)
}

/// What to do with a room's key, given what it holds now.
#[derive(Debug, PartialEq, Eq)]
pub enum NewRoomWrite {
    /// Store these bytes at the generation just read.
    Store(Vec<u8>),
    /// The key already holds this room under this identity: an earlier store of
    /// it landed (one whose answer was lost, say). Nothing to write.
    AlreadyStored,
    /// The key holds something this writer must not replace. Says why.
    Refuse(String),
}

/// The write that adds `room` to the store, given what its key holds now
/// (`current`, as a `GetVersionedResponse` gave it).
///
/// It mirrors the web UI's `reconcile_room_present`
/// (`ui/src/components/app/chat_delegate.rs`) for the cases a new room meets:
/// an absent key is stored fresh, and a tombstone is overwritten only for an
/// explicit rejoin (`over_tombstone`), as accepting an invitation is in the web
/// UI (`mark_room_rejoined`). Where the web UI would MERGE into a present room,
/// this refuses instead: merging means rewriting the web UI's whole `RoomData`,
/// and this writer knows only some of its fields. A present room under the same
/// identity is this room, already stored, so it is left as it is.
pub fn reconcile_new_room(
    current: Option<&[u8]>,
    room: &NewRoom,
    over_tombstone: bool,
) -> NewRoomWrite {
    let store = || match encode_new_room(room) {
        Ok(bytes) => NewRoomWrite::Store(bytes),
        Err(e) => NewRoomWrite::Refuse(e),
    };
    let Some(bytes) = current.filter(|b| !b.is_empty()) else {
        return store();
    };
    match decode_room_slot(bytes) {
        Ok(StoredRoomSlot::Tombstone) if over_tombstone => store(),
        Ok(StoredRoomSlot::Tombstone) => {
            NewRoomWrite::Refuse("River's storage marks this room as one you left.".into())
        }
        Ok(StoredRoomSlot::Present(stored)) => {
            let mine = room.self_verifying_key();
            if mine.is_some() && stored.self_verifying_key() == mine {
                NewRoomWrite::AlreadyStored
            } else {
                NewRoomWrite::Refuse(
                    "This room is already in River's storage on this computer.".into(),
                )
            }
        }
        // Never overwrite what can't be read: it may be a room a newer web UI
        // wrote.
        Err(e) => NewRoomWrite::Refuse(format!(
            "River's storage holds something for this room that this app can't read ({e})."
        )),
    }
}

/// The field of `rooms_meta` naming the room the web UI opens.
const CURRENT_ROOM_FIELD: &str = "current_room_key";

/// `rooms_meta` (`current`, as a `GetVersionedResponse` gave it) with the open
/// room set to `room`, as the web UI's save writes it after creating or joining
/// a room (both set `CURRENT_ROOM`, which the save copies into the meta).
/// `Ok(None)` when it already names `room`.
///
/// Only that field changes. The rest of the stored value is kept as CBOR,
/// unknown fields included, so nothing another web UI build stored is lost.
/// When the key holds nothing, the value is a map holding only the open room;
/// every other `RoomsMeta` field is `#[serde(default)]`. A value that is not a
/// CBOR map is an error, never overwritten.
pub fn meta_with_current_room(
    current: Option<&[u8]>,
    room: &VerifyingKey,
) -> Result<Option<Vec<u8>>, String> {
    use ciborium::Value;
    let wanted = Value::serialized(room).map_err(|e| format!("rooms_meta: {e}"))?;
    let mut entries = match current.filter(|b| !b.is_empty()) {
        None => Vec::new(),
        Some(bytes) => match ciborium::de::from_reader::<Value, _>(bytes) {
            Ok(Value::Map(entries)) => entries,
            Ok(_) => return Err("rooms_meta: not a map".into()),
            Err(e) => return Err(format!("rooms_meta: {e}")),
        },
    };
    match entries
        .iter_mut()
        .find(|(k, _)| k.as_text() == Some(CURRENT_ROOM_FIELD))
    {
        Some((_, v)) if *v == wanted => return Ok(None),
        Some((_, v)) => *v = wanted,
        None => entries.push((Value::Text(CURRENT_ROOM_FIELD.into()), wanted)),
    }
    let mut out = Vec::new();
    ciborium::ser::into_writer(&Value::Map(entries), &mut out)
        .map_err(|e| format!("rooms_meta: {e}"))?;
    Ok(Some(out))
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

    fn new_room(self_seed: u8) -> NewRoom {
        use freenet_stdlib::prelude::{ContractCode, Parameters};
        let me = SigningKey::from_bytes(&[self_seed; 32]);
        NewRoom {
            owner_vk: vk(3),
            room_state: ChatRoomStateV1::default(),
            self_vk: Some(me.verifying_key()),
            self_sk: Some(me),
            contract_key: ContractKey::from_params_and_code(
                Parameters::from(vec![1]),
                ContractCode::from(vec![2]),
            ),
            last_read_message_id: None,
            self_authorized_member: None,
            invite_chain: Vec::new(),
            self_member_info: None,
            self_nickname: Some("Ada".into()),
            invitation_secrets: BTreeMap::from([(0, [9; 32])]),
        }
    }

    fn tombstone() -> Vec<u8> {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&"Tombstone", &mut buf).unwrap();
        buf
    }

    #[test]
    fn a_new_room_is_stored_only_where_nothing_else_is() {
        let room = new_room(4);
        let bytes = encode_new_room(&room).unwrap();
        let StoredRoomSlot::Present(read) = decode_room_slot(&bytes).unwrap() else {
            panic!("a present slot");
        };
        assert_eq!(read.self_verifying_key(), room.self_verifying_key());
        assert_eq!(read.self_nickname.as_deref(), Some("Ada"));
        assert_eq!(read.invitation_secrets, room.invitation_secrets);

        // Absent (and the empty value a deleted key reads as): stored.
        assert_eq!(
            reconcile_new_room(None, &room, false),
            NewRoomWrite::Store(bytes.clone())
        );
        assert_eq!(
            reconcile_new_room(Some(&[]), &room, false),
            NewRoomWrite::Store(bytes.clone())
        );
        // A tombstone only for an explicit rejoin.
        assert_eq!(
            reconcile_new_room(Some(&tombstone()), &room, true),
            NewRoomWrite::Store(bytes.clone())
        );
        assert!(matches!(
            reconcile_new_room(Some(&tombstone()), &room, false),
            NewRoomWrite::Refuse(_)
        ));
        // This room, already there: nothing to write, whatever else changed.
        let mut later = room.clone();
        later.self_nickname = None;
        assert_eq!(
            reconcile_new_room(Some(&bytes), &later, true),
            NewRoomWrite::AlreadyStored
        );
        // Another identity's room, or something unreadable: never replaced.
        let other = encode_new_room(&new_room(5)).unwrap();
        assert!(matches!(
            reconcile_new_room(Some(&other), &room, true),
            NewRoomWrite::Refuse(_)
        ));
        assert!(matches!(
            reconcile_new_room(Some(&[0xff, 0x00]), &room, true),
            NewRoomWrite::Refuse(_)
        ));
        // A stored room with no identity is not this one.
        let mut anonymous = new_room(4);
        anonymous.self_sk = None;
        anonymous.self_vk = None;
        assert!(matches!(
            reconcile_new_room(
                Some(&encode_new_room(&anonymous).unwrap()),
                &anonymous,
                true
            ),
            NewRoomWrite::Refuse(_)
        ));
    }

    #[test]
    fn the_open_room_is_set_and_every_other_meta_field_kept() {
        use ciborium::Value;
        #[derive(serde::Serialize)]
        struct Meta {
            current_room_key: Option<VerifyingKey>,
            room_order: Vec<VerifyingKey>,
            something_new: u32,
        }
        let mut stored = Vec::new();
        ciborium::ser::into_writer(
            &Meta {
                current_room_key: Some(vk(1)),
                room_order: vec![vk(2), vk(1)],
                something_new: 9,
            },
            &mut stored,
        )
        .unwrap();
        let out = meta_with_current_room(Some(&stored), &vk(7))
            .unwrap()
            .unwrap();
        let read = decode_rooms_meta(&out).unwrap();
        assert_eq!(read.current_room_key, Some(vk(7)));
        assert_eq!(read.room_order, vec![vk(2), vk(1)]);
        let Value::Map(entries) = ciborium::de::from_reader::<Value, _>(&out[..]).unwrap() else {
            panic!("a map");
        };
        assert!(entries
            .iter()
            .any(|(k, v)| k.as_text() == Some("something_new") && *v == Value::from(9)));
        assert_eq!(entries.len(), 3);
        // Already open: nothing to write.
        assert_eq!(meta_with_current_room(Some(&out), &vk(7)).unwrap(), None);
        // Nothing stored: a map naming only the open room.
        let fresh = meta_with_current_room(None, &vk(7)).unwrap().unwrap();
        assert_eq!(
            decode_rooms_meta(&fresh).unwrap().current_room_key,
            Some(vk(7))
        );
        // A `None` open room is replaced too.
        let mut none = Vec::new();
        ciborium::ser::into_writer(
            &Meta {
                current_room_key: None,
                room_order: vec![],
                something_new: 1,
            },
            &mut none,
        )
        .unwrap();
        let set = meta_with_current_room(Some(&none), &vk(7))
            .unwrap()
            .unwrap();
        assert_eq!(
            decode_rooms_meta(&set).unwrap().current_room_key,
            Some(vk(7))
        );
        // Something that is not a map is never overwritten.
        let mut list = Vec::new();
        ciborium::ser::into_writer(&vec![1u8, 2], &mut list).unwrap();
        assert!(meta_with_current_room(Some(&list), &vk(7)).is_err());
        assert!(meta_with_current_room(Some(&[0xff]), &vk(7)).is_err());
    }
}
