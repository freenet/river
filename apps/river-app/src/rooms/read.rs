//! Where the user has read a room up to, shared with the web UI.
//!
//! The web UI stores it as `last_read_message_id` in the room's own slot, the CBOR value under
//! `room:<base58(owner_vk)>` ([`river_core::stored_rooms::StoredRoom`]), and counts a room's
//! unread messages from it (`unread_candidate_messages` in
//! `ui/src/components/app/document_title.rs`). The web UI moves the marker to the newest message
//! the reader has had on screen in the open room, forward only (`read_marker_update`), and saves
//! the room a little later. This app does the same ([`crate::chat`] decides when, [`super::work`]
//! paces the writes).
//!
//! [`slot_with_marker`] edits the stored slot in place, as [`super::identity::slot_with_identity`]
//! does: only `last_read_message_id` changes, and every other field of the slot, known or not,
//! stays as the web UI wrote it. It runs inside the compare-and-swap loop of [`super::work`].
//!
//! **Never backwards.** The web UI merges two copies' markers with `furthest_read_marker`: the one
//! further along in the room's messages wins, and one that is not in the messages (evicted) loses to
//! one that is. Here the same rule is applied against what the store holds, so a marker the
//! web UI stored after this app read the room is not moved back: messages are compared by their
//! place in the room's order, `(time, id)` ([`MessageOrderKey`]), found in the messages this app
//! holds and in the slot's own copy of the room.

use ciborium::Value;
use river_core::room_state::message::{MessageId, MessageOrderKey};
use river_core::stored_rooms::{StoredRoomSlot, decode_room_slot};

/// The field of the slot that holds the marker.
pub const MARKER_FIELD: &str = "last_read_message_id";

/// A message in the order of its room, as the writer knows it.
pub type Known = Vec<MessageOrderKey>;

/// Where `id` stands in the room: from the messages this app holds, else from the slot's copy.
fn place(
    id: &MessageId,
    known: &[MessageOrderKey],
    slot: &[MessageOrderKey],
) -> Option<MessageOrderKey> {
    known.iter().chain(slot).find(|k| &k.id == id).cloned()
}

/// Whether `candidate` is strictly further along than `held`, as the web UI's
/// `furthest_read_marker` decides it: further in the room's order wins; one that is not found
/// loses to one that is; with neither found nothing is further (the held one stays).
pub fn is_further(
    candidate: &MessageId,
    held: Option<&MessageId>,
    known: &[MessageOrderKey],
    slot: &[MessageOrderKey],
) -> bool {
    let Some(held) = held else {
        return true;
    };
    match (place(candidate, known, slot), place(held, known, slot)) {
        (Some(c), Some(h)) => c > h,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// `current` (a stored `Present` slot, as a `GetVersionedResponse` gave it) with its read marker
/// moved to `mark`, whose place in the room and the places of the messages around it are in
/// `known`. `Ok(None)` when the slot already holds that marker or one further along, so nothing
/// is written; the same when the room is not stored (gone, left, or a tombstone: there is nothing
/// to mark). A slot this app can't read is an error, and left alone.
pub fn slot_with_marker(
    current: Option<&[u8]>,
    mark: &MessageOrderKey,
    known: &[MessageOrderKey],
) -> Result<Option<Vec<u8>>, String> {
    let Some(bytes) = current.filter(|b| !b.is_empty()) else {
        return Ok(None);
    };
    let stored = match decode_room_slot(bytes) {
        Ok(StoredRoomSlot::Present(r)) => r,
        Ok(StoredRoomSlot::Tombstone) => return Ok(None),
        Err(e) => return Err(format!("room slot: {e}")),
    };
    let own: Vec<MessageOrderKey> = stored
        .room_state
        .recent_messages
        .messages
        .iter()
        .map(|m| m.order_key())
        .collect();
    if stored.last_read_message_id.as_ref() == Some(&mark.id)
        || !is_further(&mark.id, stored.last_read_message_id.as_ref(), known, &own)
    {
        return Ok(None);
    }
    let Ok(Value::Map(mut outer)) = ciborium::de::from_reader::<Value, _>(bytes) else {
        return Err("room slot: not a map".into());
    };
    let Some(Value::Map(fields)) = outer
        .iter_mut()
        .find(|(k, _)| k.as_text() == Some("Present"))
        .map(|(_, v)| v)
    else {
        return Err("room slot: not a present room".into());
    };
    let wanted = Value::serialized(&Some(&mark.id)).map_err(|e| format!("room slot: {e}"))?;
    match fields
        .iter_mut()
        .find(|(k, _)| k.as_text() == Some(MARKER_FIELD))
    {
        Some((_, v)) => *v = wanted,
        None => fields.push((Value::Text(MARKER_FIELD.into()), wanted)),
    }
    let mut out = Vec::new();
    ciborium::ser::into_writer(&Value::Map(outer), &mut out)
        .map_err(|e| format!("room slot: {e}"))?;
    Ok(Some(out))
}

#[cfg(test)]
#[path = "read_tests.rs"]
mod tests;
