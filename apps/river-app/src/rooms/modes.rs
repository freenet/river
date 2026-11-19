//! Per-room notification modes, shared with the web UI.
//!
//! The web UI keeps each room's mode (`NotificationMode` in `ui/src/room_data.rs`) in the
//! `notification_modes` field of `RoomsMeta`, the CBOR value stored under the chat delegate's
//! `rooms_meta` key beside `room_order`, the open room and the legacy identity sources: a map
//! from the room owner's verifying key to the mode, where a room with no entry is
//! [`NotificationMode::All`]. This app reads and writes that same field, so a room muted in the
//! browser is muted here and the other way round.
//!
//! The types are defined here, not in `river-core`: `NotificationMode` and `RoomsMeta` live in
//! the web UI crate, and moving them into `river-core` could change the bytes of the contract
//! and delegate WASM, which re-keys them. [`NotificationMode`] carries the web UI's serde
//! derive, so it encodes to the same CBOR (a text string holding the variant name); the tests
//! rebuild the web UI's `RoomsMeta` with the same derives and compare bytes both ways.
//!
//! Writing never rebuilds the value from a subset: [`meta_with_mode`] edits the stored CBOR in
//! place, as `river_core::stored_rooms::meta_with_current_room` does for the open room, so
//! `room_order`, `current_room_key`, `identity_sources` and any field a newer web UI adds are
//! kept exactly as they were, and so are the other rooms' modes (even a mode this build cannot
//! name). It runs inside the web UI's generation compare-and-swap loop ([`super::work`]).
//!
//! **Limitation: the web UI is local-wins for modes.** Its `reconcile_meta`
//! (`ui/src/components/app/chat_delegate.rs`) merges a stored `rooms_meta` into the tab's own
//! with the tab's modes first, and takes the stored mode only for a room the tab has no entry
//! for. It also never re-reads `rooms_meta` while it runs. So (1) a web tab that holds an
//! explicit entry for a room puts its own mode back the next time it saves its meta, undoing a
//! mode set here; and (2) a mode changed in the browser reaches this app only on its next load
//! of the stored rooms. This app's side of that is that it stores exactly the one entry it was
//! given and shows the store's word again after each load. There is no web change in this
//! work to close either.

use std::collections::HashMap;

use ciborium::Value;
use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};

/// The field of the web UI's `RoomsMeta` that holds the modes.
pub const MODES_FIELD: &str = "notification_modes";

/// When a room notifies. The web UI's `NotificationMode`, with its serde derive and variant
/// names, which are what is stored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NotificationMode {
    /// Every message from another member (the default).
    #[default]
    All,
    /// Only messages that mention you or reply to one of yours.
    MentionsAndReplies,
    /// Never.
    Muted,
}

impl NotificationMode {
    /// The three modes, in the order the web UI's dialog lists them.
    pub const ALL: [NotificationMode; 3] = [
        NotificationMode::All,
        NotificationMode::MentionsAndReplies,
        NotificationMode::Muted,
    ];

    /// The web UI's label for the mode (`MODES` in `notification_modal.rs`).
    pub fn label(self) -> &'static str {
        match self {
            NotificationMode::All => "All messages",
            NotificationMode::MentionsAndReplies => "Mentions & replies only",
            NotificationMode::Muted => "Muted",
        }
    }

    /// The web UI's help text for the mode.
    pub fn help(self) -> &'static str {
        match self {
            NotificationMode::All => "Notify for every message in this room.",
            NotificationMode::MentionsAndReplies => {
                "Notify only when someone @mentions you or replies to your message."
            }
            NotificationMode::Muted => "Never notify for this room.",
        }
    }
}

/// The modes a stored `rooms_meta` value holds, by room. A value that can't be read, a field
/// that is not a map, or an entry that is not a key with a mode this build knows gives nothing
/// for that part: a room with no readable entry is [`NotificationMode::All`], and the stored
/// value is never rewritten from what is returned here ([`meta_with_mode`] edits it in place).
pub fn decode_modes(bytes: &[u8]) -> HashMap<VerifyingKey, NotificationMode> {
    let Ok(Value::Map(entries)) = ciborium::de::from_reader::<Value, _>(bytes) else {
        return HashMap::new();
    };
    let Some((_, Value::Map(modes))) = entries
        .into_iter()
        .find(|(k, _)| k.as_text() == Some(MODES_FIELD))
    else {
        return HashMap::new();
    };
    modes
        .into_iter()
        .filter_map(|(k, v)| Some((k.deserialized().ok()?, v.deserialized().ok()?)))
        .collect()
}

/// `rooms_meta` (`current`, as a `GetVersionedResponse` gave it) with `room`'s mode set to
/// `mode`, as the web UI's save writes it after the user picks one (it stores the pick, the
/// default included). `Ok(None)` when it already holds that mode, so nothing is written.
///
/// Only that one entry changes. Everything else in the stored value is kept as CBOR: the other
/// fields, known or not, and the other rooms' modes. When the key holds nothing, the value is a
/// map holding only the modes (every other `RoomsMeta` field is `#[serde(default)]`). A value
/// that is not a CBOR map, or a modes field that is not a map, is an error, never overwritten.
pub fn meta_with_mode(
    current: Option<&[u8]>,
    room: &VerifyingKey,
    mode: NotificationMode,
) -> Result<Option<Vec<u8>>, String> {
    let wanted_mode = Value::serialized(&mode).map_err(|e| format!("rooms_meta: {e}"))?;
    let wanted_key = Value::serialized(room).map_err(|e| format!("rooms_meta: {e}"))?;
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
        .position(|(k, _)| k.as_text() == Some(MODES_FIELD));
    let modes = match at {
        Some(i) => match &mut entries[i].1 {
            Value::Map(modes) => modes,
            // The web UI decodes a `null` field as an error for a map, so it never wrote one;
            // anything but a map is something this app does not understand.
            _ => return Err("rooms_meta: notification_modes is not a map".into()),
        },
        None => {
            entries.push((Value::Text(MODES_FIELD.into()), Value::Map(Vec::new())));
            match &mut entries.last_mut().expect("just pushed").1 {
                Value::Map(modes) => modes,
                _ => unreachable!("just pushed a map"),
            }
        }
    };
    let same_key = |k: &Value| {
        *k == wanted_key || k.deserialized::<VerifyingKey>().is_ok_and(|vk| vk == *room)
    };
    match modes.iter_mut().find(|(k, _)| same_key(k)) {
        Some((_, v)) if *v == wanted_mode => return Ok(None),
        Some((_, v)) => *v = wanted_mode,
        None => modes.push((wanted_key, wanted_mode)),
    }
    let mut out = Vec::new();
    ciborium::ser::into_writer(&Value::Map(entries), &mut out)
        .map_err(|e| format!("rooms_meta: {e}"))?;
    Ok(Some(out))
}

#[cfg(test)]
#[path = "modes_tests.rs"]
mod tests;
