//! The order of the room list, shared with the web UI.
//!
//! The web UI keeps the order the user gave their rooms in the `room_order` field of
//! `RoomsMeta`, the CBOR value stored under the chat delegate's `rooms_meta` key (see
//! [`super::modes`] for the rest of that value). Rooms not in it follow, by key bytes. The web
//! UI changes it by drag and drop (`Rooms::move_room_before` in `ui/src/room_data.rs`), which
//! stores the whole visible order through `set_visible_order`; this app does it from a
//! Move up / Move down menu, with the same write.
//!
//! [`meta_with_order`] edits the stored CBOR in place, as [`super::modes::meta_with_mode`] does,
//! so the other fields (notification modes, the open room, identity sources, anything a newer web
//! UI adds) are kept exactly as they were. It runs inside the web UI's generation
//! compare-and-swap loop ([`super::work`]), so a write that lost to another writer is worked out
//! again from what that writer stored.
//!
//! The new order is the order shown, and entries of the stored order for rooms that are not in
//! it (rooms the web UI has that this app has not read yet) are kept next to their old
//! neighbours, so they do not drop to the bottom ([`merge_room_order`], the web UI's function
//! of the same name; freenet/river#802).

use ciborium::Value;
use ed25519_dalek::VerifyingKey;

/// The field of the web UI's `RoomsMeta` that holds the order.
pub const ORDER_FIELD: &str = "room_order";

/// `base` with the keys of `other` it lacks inserted next to their neighbours in `other`: right
/// after the nearest preceding key of `other` that `base` has, else right before the nearest
/// following one, else at the end. The web UI's `merge_room_order`, key for key.
pub fn merge_room_order(base: &mut Vec<VerifyingKey>, other: &[VerifyingKey]) {
    for (i, vk) in other.iter().enumerate() {
        if base.contains(vk) {
            continue;
        }
        let after = other[..i]
            .iter()
            .rev()
            .find_map(|p| base.iter().position(|b| b == p).map(|at| at + 1));
        let at = after.unwrap_or_else(|| {
            other[i + 1..]
                .iter()
                .find_map(|n| base.iter().position(|b| b == n))
                .unwrap_or(base.len())
        });
        base.insert(at, *vk);
    }
}

/// `shown` with `room` moved to sit immediately before `before` (`None`: to the end). Unchanged
/// when `room` is not in it, when `before` is not in it, or when the move changes nothing.
pub fn move_before(
    shown: &[VerifyingKey],
    room: &VerifyingKey,
    before: Option<&VerifyingKey>,
) -> Vec<VerifyingKey> {
    let mut out = shown.to_vec();
    let Some(from) = out.iter().position(|k| k == room) else {
        return out;
    };
    if before.is_some_and(|b| b == room || !out.contains(b)) {
        return out;
    }
    out.remove(from);
    let at = before
        .and_then(|b| out.iter().position(|k| k == b))
        .unwrap_or(out.len());
    out.insert(at, *room);
    out
}

/// The room order a stored `rooms_meta` value holds: empty when it can't be read, or the field
/// is missing or not a list. Entries that are not keys are left out.
pub fn decode_order(bytes: &[u8]) -> Vec<VerifyingKey> {
    let Ok(Value::Map(entries)) = ciborium::de::from_reader::<Value, _>(bytes) else {
        return Vec::new();
    };
    let Some((_, Value::Array(order))) = entries
        .into_iter()
        .find(|(k, _)| k.as_text() == Some(ORDER_FIELD))
    else {
        return Vec::new();
    };
    order
        .into_iter()
        .filter_map(|v| v.deserialized().ok())
        .collect()
}

/// `rooms_meta` (`current`, as a `GetVersionedResponse` gave it) with the room order set to
/// `shown`, the order on the screen, plus the stored entries `shown` lacks, next to their old
/// neighbours. `Ok(None)` when that is the order already stored, so nothing is written.
///
/// Only the order changes; everything else is kept as CBOR. When the key holds nothing the value
/// is a map holding only the order (every other `RoomsMeta` field is `#[serde(default)]`). A value
/// that is not a CBOR map, or an order that is not a list, is an error, never overwritten.
pub fn meta_with_order(
    current: Option<&[u8]>,
    shown: &[VerifyingKey],
) -> Result<Option<Vec<u8>>, String> {
    let mut entries = match current.filter(|b| !b.is_empty()) {
        None => Vec::new(),
        Some(bytes) => match ciborium::de::from_reader::<Value, _>(bytes) {
            Ok(Value::Map(entries)) => entries,
            Ok(_) => return Err("rooms_meta: not a map".into()),
            Err(e) => return Err(format!("rooms_meta: {e}")),
        },
    };
    let at = entries
        .iter()
        .position(|(k, _)| k.as_text() == Some(ORDER_FIELD));
    let stored: Vec<VerifyingKey> = match at {
        Some(i) => match &entries[i].1 {
            Value::Array(items) => items.iter().filter_map(|v| v.deserialized().ok()).collect(),
            _ => return Err("rooms_meta: room_order is not a list".into()),
        },
        None => Vec::new(),
    };
    let mut order = shown.to_vec();
    order.dedup();
    merge_room_order(&mut order, &stored);
    if at.is_some() && order == stored {
        return Ok(None);
    }
    let wanted = Value::Array(
        order
            .iter()
            .map(|k| Value::serialized(k).map_err(|e| format!("rooms_meta: {e}")))
            .collect::<Result<_, _>>()?,
    );
    match at {
        Some(i) => entries[i].1 = wanted,
        None => entries.push((Value::Text(ORDER_FIELD.into()), wanted)),
    }
    let mut out = Vec::new();
    ciborium::ser::into_writer(&Value::Map(entries), &mut out)
        .map_err(|e| format!("rooms_meta: {e}"))?;
    Ok(Some(out))
}

#[cfg(test)]
#[path = "order_tests.rs"]
mod tests;
