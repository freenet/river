//! Leaving a room, as the web UI does it, in the chat delegate's storage.
//!
//! The web UI's Leave (`edit_room_modal.rs`, then `do_save_rooms_to_delegate` in
//! `ui/src/components/app/chat_delegate.rs`) changes two stored values:
//!
//! 1. The room's own key, `room:<base58(owner_vk)>`, is overwritten with a tombstone, the CBOR
//!    of the unit variant `RoomSlot::Tombstone` (a text string, `"Tombstone"`), unless it
//!    holds a tombstone already (`reconcile_room_tombstone`). Whatever the key held before is
//!    replaced, a room this app can read or one it cannot: this is what removes the user's
//!    signing key for the room from the store, and what keeps the room from coming back when
//!    the stored rooms are read again. Both clients skip a tombstone.
//! 2. `rooms_meta` drops the room from the room order, from the identity sources and, when it
//!    is the open room, from `current_room_key`. The room's notification mode stays: the web
//!    UI keeps it on purpose, so rejoining the same room later inherits it.
//!
//! The web UI publishes nothing to the room's contract when leaving: the member stays in the
//! room's member list until the room prunes them, and it never deletes the chat delegate's own
//! copy of the room's signing key (the delegate has no request for that). This module does the
//! same.
//!
//! Both edits run inside the generation compare-and-swap loop of [`super::work`], which re-reads
//! and works them out again on a conflict. [`meta_without_room`] edits the stored CBOR in place
//! and keeps every field it does not own, known or not, as [`super::modes::meta_with_mode`]
//! does.

use ciborium::Value;
use ed25519_dalek::VerifyingKey;
use river_core::stored_rooms::{StoredRoomSlot, decode_room_slot};
use serde::Serialize;

/// The web UI's `RoomSlot`, with only the variant a leave writes. Serde names the variant, so
/// the bytes are the ones the web UI's `RoomSlot::Tombstone` writes.
#[derive(Serialize)]
enum LeftSlot {
    Tombstone,
}

/// What to store at the room's key, given what it holds now (`current`, as a
/// `GetVersionedResponse` gave it): the tombstone, or `None` when the key holds one already.
/// Anything else, including bytes this app can't read, is replaced, as the web UI's
/// `reconcile_room_tombstone` does.
pub fn tombstone_write(current: Option<&[u8]>) -> Result<Option<Vec<u8>>, String> {
    if let Some(bytes) = current
        && matches!(decode_room_slot(bytes), Ok(StoredRoomSlot::Tombstone))
    {
        return Ok(None);
    }
    let mut out = Vec::new();
    ciborium::ser::into_writer(&LeftSlot::Tombstone, &mut out)
        .map_err(|e| format!("tombstone: {e}"))?;
    Ok(Some(out))
}

/// `rooms_meta` (`current`, as a `GetVersionedResponse` gave it) with `room` taken out of the
/// room order, the identity sources and, if it is the open room, `current_room_key` (set to
/// null, as the web UI's save writes it once its open room is cleared). `Ok(None)` when none of
/// them names the room, so nothing is written; the same when the key holds nothing.
///
/// Everything else is kept as CBOR, unknown fields and the notification modes included. A value
/// that is not a CBOR map, or one of these fields in a shape the web UI never wrote, is an
/// error, never overwritten.
pub fn meta_without_room(
    current: Option<&[u8]>,
    room: &VerifyingKey,
) -> Result<Option<Vec<u8>>, String> {
    let Some(bytes) = current.filter(|b| !b.is_empty()) else {
        return Ok(None);
    };
    let mut entries = match ciborium::de::from_reader::<Value, _>(bytes) {
        Ok(Value::Map(entries)) => entries,
        Ok(_) => return Err("rooms_meta: not a map".into()),
        Err(e) => return Err(format!("rooms_meta: {e}")),
    };
    let wanted = Value::serialized(room).map_err(|e| format!("rooms_meta: {e}"))?;
    let is_room =
        |v: &Value| *v == wanted || v.deserialized::<VerifyingKey>().is_ok_and(|vk| vk == *room);
    let mut changed = false;
    let mut drop_identity_sources = false;
    for (name, value) in &mut entries {
        match (name.as_text(), value) {
            (Some("current_room_key"), v) => {
                if is_room(v) {
                    *v = Value::Null;
                    changed = true;
                }
            }
            (Some("room_order"), Value::Array(order)) => {
                let before = order.len();
                order.retain(|v| !is_room(v));
                changed |= order.len() != before;
            }
            (Some("room_order"), _) => return Err("rooms_meta: room_order is not a list".into()),
            // Each entry is `(room, delegate key)`.
            (Some("identity_sources"), Value::Array(sources)) => {
                let before = sources.len();
                sources.retain(
                    |s| !matches!(s, Value::Array(pair) if pair.first().is_some_and(is_room)),
                );
                if sources.len() != before {
                    changed = true;
                    // The web UI leaves the field out when it is empty.
                    drop_identity_sources = sources.is_empty();
                }
            }
            (Some("identity_sources"), _) => {
                return Err("rooms_meta: identity_sources is not a list".into());
            }
            _ => {}
        }
    }
    if !changed {
        return Ok(None);
    }
    if drop_identity_sources {
        entries.retain(|(k, _)| k.as_text() != Some("identity_sources"));
    }
    let mut out = Vec::new();
    ciborium::ser::into_writer(&Value::Map(entries), &mut out)
        .map_err(|e| format!("rooms_meta: {e}"))?;
    Ok(Some(out))
}

#[cfg(test)]
#[path = "leave_tests.rs"]
mod tests;
