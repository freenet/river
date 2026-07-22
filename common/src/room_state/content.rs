//! Message content types for client-side interpretation.
//!
//! The contract treats message content as opaque bytes with type/version tags.
//! This module defines the client-side content types that are encoded into those bytes.
//!
//! # Extensibility
//!
//! - New content types: Add new `CONTENT_TYPE_*` constant, no contract change needed
//! - New action types: Add new `ACTION_TYPE_*` constant, no contract change needed
//! - New fields on existing types: Just add them (old clients ignore unknown fields)
//! - Breaking format changes: Bump the version constant for that type

use crate::room_state::message::MessageId;
use serde::{Deserialize, Serialize};

/// Content type constants
pub const CONTENT_TYPE_TEXT: u32 = 1;
pub const CONTENT_TYPE_ACTION: u32 = 2;
pub const CONTENT_TYPE_REPLY: u32 = 3;
pub const CONTENT_TYPE_EVENT: u32 = 4;
// Future: CONTENT_TYPE_BLOB = 5, CONTENT_TYPE_POLL = 6, etc.

/// Current version for text content
pub const TEXT_CONTENT_VERSION: u32 = 1;

/// Current version for action content
pub const ACTION_CONTENT_VERSION: u32 = 1;

/// Current version for reply content
pub const REPLY_CONTENT_VERSION: u32 = 1;

/// Current version for event content
pub const EVENT_CONTENT_VERSION: u32 = 1;

/// Event type constants
pub const EVENT_TYPE_JOIN: u32 = 1;
// Future: EVENT_TYPE_LEAVE = 2, etc.

/// Action type constants
pub const ACTION_TYPE_EDIT: u32 = 1;
pub const ACTION_TYPE_DELETE: u32 = 2;
pub const ACTION_TYPE_REACTION: u32 = 3;
pub const ACTION_TYPE_REMOVE_REACTION: u32 = 4;
// Future: ACTION_TYPE_PIN = 5, ACTION_TYPE_REPLY = 6, etc.

/// Text message content (content_type = 1)
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct TextContentV1 {
    pub text: String,
}

impl TextContentV1 {
    pub fn new(text: String) -> Self {
        Self { text }
    }

    /// Encode to CBOR bytes
    pub fn encode(&self) -> Vec<u8> {
        encode_cbor(self)
    }

    /// Decode from CBOR bytes
    pub fn decode(data: &[u8]) -> Result<Self, String> {
        decode_cbor(data, "TextContentV1")
    }
}

/// Encode a value to CBOR bytes
fn encode_cbor<T: Serialize>(value: &T) -> Vec<u8> {
    let mut data = Vec::new();
    ciborium::into_writer(value, &mut data).expect("CBOR serialization should not fail");
    data
}

/// Decode a value from CBOR bytes
fn decode_cbor<T: serde::de::DeserializeOwned>(data: &[u8], type_name: &str) -> Result<T, String> {
    ciborium::from_reader(data).map_err(|e| format!("Failed to decode {}: {}", type_name, e))
}

/// Serde helper: encode [`ActionContentV1::payload`] as a CBOR **byte string**
/// while still decoding the legacy **array-of-integers** form.
///
/// serde has no distinct byte-string type in the derive path, so a bare
/// `Vec<u8>` goes through `serialize_seq` and ciborium writes a CBOR array of
/// integers. Every byte >= 0x18 then costs 2 bytes on the wire, and all
/// printable ASCII is >= 0x20 — so an edit cost ~2.1 bytes per character while
/// a plain `TextContentV1 { text: String }` message cost ~1.01 (a CBOR text
/// string). Against the default `max_message_size` of 1000 that capped edits at
/// ~467 characters while sends allowed ~985, i.e. a message could be sent and
/// then never edited (freenet/river#443).
///
/// Serializing as a byte string brings edits to ~1.05 bytes per character.
///
/// `deserialize` accepts BOTH encodings, which is required rather than
/// cosmetic: rooms created before this change hold action payloads in the
/// array form, and contract migration re-PUTs that existing state into the new
/// contract. Without the legacy arm every pre-existing edit and reaction would
/// silently stop rendering (`ActionContentV1::decode` -> `Err` -> the action is
/// skipped by `rebuild_actions_state_with_decrypted`).
///
/// `deserialize_any` is sound here because this type is only ever serialized
/// with ciborium (see `encode_cbor` / `decode_cbor` above) and CBOR is
/// self-describing. Do NOT reuse this helper for a type that may be handled by
/// a non-self-describing format such as bincode.
mod payload_bytes {
    use serde::de::{Error as _, SeqAccess, Visitor};
    use serde::{Deserializer, Serializer};
    use std::fmt;

    pub fn serialize<S: Serializer>(payload: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(payload)
    }

    struct BytesOrLegacySeq;

    impl<'de> Visitor<'de> for BytesOrLegacySeq {
        type Value = Vec<u8>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a CBOR byte string, or a legacy array of byte values")
        }

        fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
            Ok(v.to_vec())
        }

        fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<Self::Value, E> {
            Ok(v)
        }

        /// Legacy form: a CBOR array of integers, one per byte.
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            // Read as u16 so an out-of-range element is a decode error rather
            // than a silent truncation to u8.
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0));
            while let Some(byte) = seq.next_element::<u16>()? {
                if byte > u8::MAX as u16 {
                    return Err(A::Error::custom(format!(
                        "action payload element {byte} is not a byte"
                    )));
                }
                out.push(byte as u8);
            }
            Ok(out)
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        deserializer.deserialize_any(BytesOrLegacySeq)
    }
}

/// Action message content (content_type = 2)
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct ActionContentV1 {
    /// Type of action (ACTION_TYPE_* constants)
    pub action_type: u32,
    /// Target message ID for the action
    pub target: MessageId,
    /// Action-specific payload (CBOR-encoded).
    ///
    /// Encoded on the wire as a CBOR byte string; the legacy array-of-integers
    /// form is still accepted on decode. See [`payload_bytes`] and
    /// freenet/river#443 — do NOT drop the `serde(with = ...)` attribute, it is
    /// what keeps an edit from costing ~2 bytes per character.
    #[serde(with = "payload_bytes")]
    pub payload: Vec<u8>,
}

impl ActionContentV1 {
    /// Create an edit action
    pub fn edit(target: MessageId, new_text: String) -> Self {
        Self {
            action_type: ACTION_TYPE_EDIT,
            target,
            payload: encode_cbor(&EditPayload { new_text }),
        }
    }

    /// Create a delete action
    pub fn delete(target: MessageId) -> Self {
        Self {
            action_type: ACTION_TYPE_DELETE,
            target,
            payload: Vec::new(),
        }
    }

    /// Create a reaction action
    pub fn reaction(target: MessageId, emoji: String) -> Self {
        Self {
            action_type: ACTION_TYPE_REACTION,
            target,
            payload: encode_cbor(&ReactionPayload { emoji }),
        }
    }

    /// Create a remove reaction action
    pub fn remove_reaction(target: MessageId, emoji: String) -> Self {
        Self {
            action_type: ACTION_TYPE_REMOVE_REACTION,
            target,
            payload: encode_cbor(&ReactionPayload { emoji }),
        }
    }

    /// Encode to CBOR bytes
    pub fn encode(&self) -> Vec<u8> {
        encode_cbor(self)
    }

    /// Decode from CBOR bytes
    pub fn decode(data: &[u8]) -> Result<Self, String> {
        decode_cbor(data, "ActionContentV1")
    }

    /// Get the edit payload if this is an edit action
    pub fn edit_payload(&self) -> Option<EditPayload> {
        if self.action_type == ACTION_TYPE_EDIT {
            ciborium::from_reader(&self.payload[..]).ok()
        } else {
            None
        }
    }

    /// Get the reaction payload if this is a reaction or remove_reaction action
    pub fn reaction_payload(&self) -> Option<ReactionPayload> {
        if self.action_type == ACTION_TYPE_REACTION
            || self.action_type == ACTION_TYPE_REMOVE_REACTION
        {
            ciborium::from_reader(&self.payload[..]).ok()
        } else {
            None
        }
    }
}

/// Payload for edit actions
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct EditPayload {
    pub new_text: String,
}

/// Payload for reaction actions
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct ReactionPayload {
    pub emoji: String,
}

/// Reply message content (content_type = 3)
///
/// A reply references a target message and includes a snapshot of the target's
/// author name and content preview so that the reply remains meaningful even if
/// the target message is later deleted or scrolled out of the recent window.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct ReplyContentV1 {
    pub text: String,
    pub target_message_id: MessageId,
    pub target_author_name: String,
    /// Snapshot of the target message content (~100 chars)
    pub target_content_preview: String,
}

impl ReplyContentV1 {
    pub fn new(
        text: String,
        target_message_id: MessageId,
        target_author_name: String,
        target_content_preview: String,
    ) -> Self {
        Self {
            text,
            target_message_id,
            target_author_name,
            target_content_preview,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        encode_cbor(self)
    }

    pub fn decode(data: &[u8]) -> Result<Self, String> {
        decode_cbor(data, "ReplyContentV1")
    }
}

/// Event message content (content_type = 4)
///
/// Represents room events like joins and leaves. These are authored by the
/// member performing the action and count as messages for pruning purposes.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct EventContentV1 {
    pub event_type: u32,
}

impl EventContentV1 {
    pub fn join() -> Self {
        Self {
            event_type: EVENT_TYPE_JOIN,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        encode_cbor(self)
    }

    pub fn decode(data: &[u8]) -> Result<Self, String> {
        decode_cbor(data, "EventContentV1")
    }
}

/// Decoded message content for client-side processing
#[derive(Clone, PartialEq, Debug)]
pub enum DecodedContent {
    /// Text message
    Text(TextContentV1),
    /// Action on another message
    Action(ActionContentV1),
    /// Reply to another message
    Reply(ReplyContentV1),
    /// Room event (join, leave, etc.)
    Event(EventContentV1),
    /// Unknown content type - preserved for round-tripping but displayed as placeholder
    Unknown {
        content_type: u32,
        content_version: u32,
    },
}

impl DecodedContent {
    /// Check if this is an action
    pub fn is_action(&self) -> bool {
        matches!(self, Self::Action(_))
    }

    /// Check if this is an event
    pub fn is_event(&self) -> bool {
        matches!(self, Self::Event(_))
    }

    /// Get the target message ID if this is an action
    pub fn target_id(&self) -> Option<&MessageId> {
        match self {
            Self::Action(action) => Some(&action.target),
            _ => None,
        }
    }

    /// Get the text content if this is a text or reply message
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(&text.text),
            Self::Reply(reply) => Some(&reply.text),
            _ => None,
        }
    }

    /// Get a display string for this content
    pub fn to_display_string(&self) -> String {
        match self {
            Self::Text(text) => text.text.clone(),
            Self::Reply(reply) => reply.text.clone(),
            Self::Action(action) => match action.action_type {
                ACTION_TYPE_EDIT => format!("[Edit of message {}]", action.target),
                ACTION_TYPE_DELETE => format!("[Delete of message {}]", action.target),
                ACTION_TYPE_REACTION => {
                    let emoji = action
                        .reaction_payload()
                        .map(|p| p.emoji)
                        .unwrap_or_else(|| "?".to_string());
                    format!("[Reaction {} to {}]", emoji, action.target)
                }
                ACTION_TYPE_REMOVE_REACTION => {
                    let emoji = action
                        .reaction_payload()
                        .map(|p| p.emoji)
                        .unwrap_or_else(|| "?".to_string());
                    format!("[Remove reaction {} from {}]", emoji, action.target)
                }
                _ => format!(
                    "[Unknown action type {} on {}]",
                    action.action_type, action.target
                ),
            },
            Self::Event(event) => match event.event_type {
                EVENT_TYPE_JOIN => "joined the room".to_string(),
                _ => format!("[Unknown event type {}]", event.event_type),
            },
            Self::Unknown {
                content_type,
                content_version,
            } => format!(
                "[Unsupported message type {}.{} - please upgrade]",
                content_type, content_version
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use freenet_scaffold::util::fast_hash;

    fn test_message_id() -> MessageId {
        MessageId(fast_hash(&[1, 2, 3, 4]))
    }

    #[test]
    fn test_text_content_roundtrip() {
        let content = TextContentV1::new("Hello, world!".to_string());
        let encoded = content.encode();
        let decoded = TextContentV1::decode(&encoded).unwrap();
        assert_eq!(content, decoded);
    }

    /// Mirror of [`ActionContentV1`] as it was encoded BEFORE freenet/river#443
    /// (bare `Vec<u8>` -> CBOR array of integers). Stands in for both an
    /// existing room's stored actions and a pre-#443 client on the wire.
    #[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
    struct LegacyActionContentV1 {
        action_type: u32,
        target: MessageId,
        payload: Vec<u8>,
    }

    /// The migration-critical direction: rooms created before #443 hold action
    /// payloads as a CBOR array of integers, and contract migration re-PUTs that
    /// state into the new contract. If this breaks, every pre-existing edit and
    /// reaction silently stops rendering.
    #[test]
    fn legacy_array_payload_still_decodes() {
        let action = ActionContentV1::edit(test_message_id(), "Edited text".to_string());
        let legacy = LegacyActionContentV1 {
            action_type: action.action_type,
            target: action.target.clone(),
            payload: action.payload.clone(),
        };
        let legacy_bytes = encode_cbor(&legacy);

        let decoded = ActionContentV1::decode(&legacy_bytes)
            .expect("legacy array-encoded payload must still decode");
        assert_eq!(decoded, action, "legacy decode must be lossless");
        assert_eq!(
            decoded.edit_payload().expect("edit payload").new_text,
            "Edited text",
            "the edited text must survive a legacy-format decode"
        );
    }

    /// The rollout direction: a pre-#443 reader must still understand the new
    /// byte-string encoding (ciborium's `deserialize_seq` accepts a byte
    /// string), so a stale riverctl/UI does not lose newly-authored edits.
    #[test]
    fn new_byte_string_payload_decodes_with_legacy_reader() {
        let action = ActionContentV1::edit(test_message_id(), "Edited text".to_string());
        let new_bytes = action.encode();

        let legacy: LegacyActionContentV1 = ciborium::from_reader(&new_bytes[..])
            .expect("a pre-#443 reader must still decode the new encoding");
        assert_eq!(legacy.payload, action.payload);
        assert_eq!(legacy.action_type, action.action_type);
        assert_eq!(legacy.target, action.target);
    }

    /// Regression pin for freenet/river#443. Before the fix an edit cost ~2.1
    /// bytes per ASCII character (CBOR array of integers), so against the
    /// default `max_message_size` of 1000 edits were capped at ~467 characters
    /// while sends allowed ~985 — a message could be sent and never edited.
    #[test]
    fn edit_action_does_not_cost_two_bytes_per_character() {
        let text = "a".repeat(900);
        let encoded_len = ActionContentV1::edit(test_message_id(), text.clone())
            .encode()
            .len();

        // Legacy encoding of the very same action, for contrast.
        let legacy_len = {
            let action = ActionContentV1::edit(test_message_id(), text.clone());
            encode_cbor(&LegacyActionContentV1 {
                action_type: action.action_type,
                target: action.target.clone(),
                payload: action.payload,
            })
            .len()
        };

        assert!(
            legacy_len > 1800,
            "sanity: the legacy encoding should be ~2x the text ({legacy_len} bytes)"
        );
        assert!(
            encoded_len < 1000,
            "a 900-char edit must fit the default 1000-byte limit, got {encoded_len} bytes"
        );
        assert!(
            encoded_len < text.len() + 100,
            "edit overhead must be roughly constant, not proportional: \
             {encoded_len} bytes for {} chars",
            text.len()
        );
    }

    #[test]
    fn test_edit_action_roundtrip() {
        let action = ActionContentV1::edit(test_message_id(), "New text".to_string());
        let encoded = action.encode();
        let decoded = ActionContentV1::decode(&encoded).unwrap();
        assert_eq!(action, decoded);

        let payload = decoded.edit_payload().unwrap();
        assert_eq!(payload.new_text, "New text");
    }

    #[test]
    fn test_delete_action_roundtrip() {
        let action = ActionContentV1::delete(test_message_id());
        let encoded = action.encode();
        let decoded = ActionContentV1::decode(&encoded).unwrap();
        assert_eq!(action, decoded);
        assert_eq!(decoded.action_type, ACTION_TYPE_DELETE);
    }

    #[test]
    fn test_reaction_action_roundtrip() {
        let action = ActionContentV1::reaction(test_message_id(), "👍".to_string());
        let encoded = action.encode();
        let decoded = ActionContentV1::decode(&encoded).unwrap();
        assert_eq!(action, decoded);

        let payload = decoded.reaction_payload().unwrap();
        assert_eq!(payload.emoji, "👍");
    }

    #[test]
    fn test_remove_reaction_action_roundtrip() {
        let action = ActionContentV1::remove_reaction(test_message_id(), "❤️".to_string());
        let encoded = action.encode();
        let decoded = ActionContentV1::decode(&encoded).unwrap();
        assert_eq!(action, decoded);

        let payload = decoded.reaction_payload().unwrap();
        assert_eq!(payload.emoji, "❤️");
    }

    #[test]
    fn test_reply_content_roundtrip() {
        let reply = ReplyContentV1::new(
            "I agree!".to_string(),
            test_message_id(),
            "Alice".to_string(),
            "The original message text here...".to_string(),
        );
        let encoded = reply.encode();
        let decoded = ReplyContentV1::decode(&encoded).unwrap();
        assert_eq!(reply, decoded);

        // Verify DecodedContent::Reply returns text via as_text()
        let dc = DecodedContent::Reply(reply.clone());
        assert_eq!(dc.as_text(), Some("I agree!"));
        assert_eq!(dc.to_display_string(), "I agree!");
        assert!(!dc.is_action());
    }

    #[test]
    fn test_decoded_content_display() {
        let text = DecodedContent::Text(TextContentV1::new("Hello".to_string()));
        assert_eq!(text.to_display_string(), "Hello");

        let unknown = DecodedContent::Unknown {
            content_type: 99,
            content_version: 1,
        };
        assert!(unknown.to_display_string().contains("Unsupported"));
    }

    #[test]
    fn test_event_content_roundtrip() {
        let event = EventContentV1::join();
        let encoded = event.encode();
        let decoded = EventContentV1::decode(&encoded).unwrap();
        assert_eq!(event, decoded);
        assert_eq!(decoded.event_type, EVENT_TYPE_JOIN);

        let dc = DecodedContent::Event(event);
        assert!(dc.is_event());
        assert!(!dc.is_action());
        assert_eq!(dc.to_display_string(), "joined the room");
    }

    #[test]
    fn test_join_event_message_body() {
        let body = crate::room_state::message::RoomMessageBody::join_event();
        assert!(body.is_event());
        assert!(!body.is_action());
        let decoded = body.decode_content().unwrap();
        assert!(matches!(decoded, DecodedContent::Event(_)));
    }
}
