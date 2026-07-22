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
/// ~467 characters while sends allowed ~991, i.e. a message could be sent and
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
///
/// One behavioural note: `deserialize_any` is the only ciborium entry point
/// that does NOT skip a `Header::Tag` — it routes tags to `visit_enum`,
/// whereas the derived `deserialize_seq` path transparently unwrapped them. So
/// this helper is marginally STRICTER than what it replaced. Nothing in River
/// emits CBOR tags, so no stored payload is affected.
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
            // NEVER pre-allocate from `size_hint()` unbounded. It is the
            // DECLARED length from the attacker-supplied CBOR array header,
            // which ciborium returns verbatim without checking it against the
            // remaining input. This decode runs inside the room contract on
            // untrusted peer data, so a ~50-byte message carrying a header
            // like `0x9B FF..FF` (2^64-1 elements) would otherwise become a
            // capacity-overflow panic or a multi-gigabyte allocation. serde's
            // derived `Vec<u8>` impl bounds this with `size_hint::cautious`;
            // this hand-rolled visitor must re-establish that bound. The Vec
            // still grows as needed, so a legitimately larger payload is
            // unaffected. Pinned by
            // `legacy_payload_with_lying_length_header_errors_not_panics`.
            const MAX_PREALLOC: usize = 4096;
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(MAX_PREALLOC));
            // Read as u16 purely for a clearer error message. `next_element::<u8>()`
            // would ALSO be correct — serde's u8 visitor range-checks and errors
            // on anything above 255, it does not truncate. Do not "harden" this
            // against a truncation bug that does not exist.
            while let Some(byte) = seq.next_element::<u16>()? {
                if byte > u8::MAX as u16 {
                    // A static message keeps `core::fmt` machinery out of the
                    // contract WASM.
                    return Err(A::Error::custom(
                        "action payload element is not a byte value",
                    ));
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
    /// form is still accepted on decode. See the `payload_bytes` module and
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

        // Pin EXPLICITLY that the fixture really is array-encoded. Without
        // this the test silently degenerates into a duplicate of
        // `test_edit_action_roundtrip` if the mirror ever stops producing the
        // legacy shape, and would no longer exercise `visit_seq` at all.
        let as_value: ciborium::value::Value =
            ciborium::from_reader(&legacy_bytes[..]).expect("decode as generic CBOR");
        let payload_field = as_value
            .as_map()
            .expect("a CBOR map")
            .iter()
            .find(|(k, _)| k.as_text() == Some("payload"))
            .map(|(_, v)| v)
            .expect("a payload field");
        assert!(
            payload_field.is_array(),
            "the legacy fixture must be a CBOR array of integers, got {payload_field:?}"
        );

        let decoded = ActionContentV1::decode(&legacy_bytes)
            .expect("legacy array-encoded payload must still decode");
        assert_eq!(decoded, action, "legacy decode must be lossless");
        assert_eq!(
            decoded.edit_payload().expect("edit payload").new_text,
            "Edited text",
            "the edited text must survive a legacy-format decode"
        );
    }

    /// Build legacy (array-encoded) bytes with `payload` replaced by a raw
    /// CBOR fragment. `payload` is the last declared field, so its encoding is
    /// last in the output and can be swapped wholesale.
    fn legacy_bytes_with_raw_payload(raw_payload: &[u8]) -> Vec<u8> {
        let mut bytes = encode_cbor(&LegacyActionContentV1 {
            action_type: ACTION_TYPE_EDIT,
            target: test_message_id(),
            payload: Vec::new(),
        });
        assert_eq!(
            bytes.pop(),
            Some(0x80),
            "expected a trailing empty CBOR array for the empty payload"
        );
        bytes.extend_from_slice(raw_payload);
        bytes
    }

    /// A crafted CBOR array header can declare far more elements than the
    /// input actually contains, and ciborium's `size_hint` returns that
    /// declared length verbatim without checking it against the remaining
    /// bytes. Pre-allocating from it unbounded lets ANY peer turn a ~50-byte
    /// message into a multi-gigabyte allocation or a capacity-overflow panic
    /// inside the room contract, the UI, and riverctl. serde's derived
    /// `Vec<u8>` impl caps this via `size_hint::cautious`; a hand-rolled
    /// visitor has to re-establish that bound.
    ///
    /// The three vectors below discriminate on DIFFERENT targets — keep all of
    /// them, none is redundant:
    /// - `0x9B FF..FF` (2^64-1): the vector that reproduces on x86_64, where
    ///   CI runs. It panicked with "capacity overflow" before the clamp. On
    ///   wasm32 `usize::try_from` rejects it first, so it proves nothing there.
    /// - `0x9A FF FF FF FF` (2^32-1): the vector that matters for the SHIPPING
    ///   target. On wasm32 (room contract + River UI) `usize` is 32-bit, so
    ///   this is a valid length and an unclamped `with_capacity` traps on
    ///   `memory.grow`.
    /// - `0x9A 00 0F 42 40` (1,000,000): a moderate lie that would reserve
    ///   1 MB. Errors either way; it pins that a truncated array is a clean
    ///   decode error rather than a partial read.
    #[test]
    fn legacy_payload_with_lying_length_header_errors_not_panics() {
        for (label, header) in [
            (
                "u64::MAX elements",
                &[0x9B, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF][..],
            ),
            ("u32::MAX elements", &[0x9A, 0xFF, 0xFF, 0xFF, 0xFF][..]),
            ("1,000,000 elements", &[0x9A, 0x00, 0x0F, 0x42, 0x40][..]),
        ] {
            let bytes = legacy_bytes_with_raw_payload(header);
            assert!(
                ActionContentV1::decode(&bytes).is_err(),
                "{label}: a lying array header must be a decode error, never a panic/abort"
            );
        }
    }

    /// The out-of-range guard in `visit_seq`. Without it a `byte as u8`
    /// truncation would silently corrupt the payload instead of rejecting it.
    #[test]
    fn legacy_payload_with_out_of_range_element_is_rejected() {
        // [300] — a single element that is not a byte.
        let bad = legacy_bytes_with_raw_payload(&[0x81, 0x19, 0x01, 0x2C]);
        assert!(
            ActionContentV1::decode(&bad).is_err(),
            "an element above 255 must be rejected, not truncated"
        );

        // [1, 300] — must not accept the valid prefix then truncate.
        let mixed = legacy_bytes_with_raw_payload(&[0x82, 0x01, 0x19, 0x01, 0x2C]);
        assert!(
            ActionContentV1::decode(&mixed).is_err(),
            "a trailing out-of-range element must reject the whole payload"
        );
    }

    /// Legacy decode must cover every action kind, not just `edit`:
    /// - `delete` carries an EMPTY payload (`0x80` legacy / `0x40` new), a
    ///   distinct branch from a populated one.
    /// - reaction emoji contain bytes >= 0x80, which the legacy form encodes
    ///   as TWO-byte CBOR integers rather than one — a different code path
    ///   through `visit_seq` than all-ASCII edit text.
    /// - multi-byte UTF-8 edit text likewise crosses the one/two-byte boundary.
    #[test]
    fn legacy_decode_covers_every_action_kind() {
        let cases = vec![
            ActionContentV1::edit(test_message_id(), "plain ascii".to_string()),
            ActionContentV1::edit(test_message_id(), "café 🎉 naïve".to_string()),
            ActionContentV1::delete(test_message_id()),
            ActionContentV1::reaction(test_message_id(), "👍".to_string()),
            ActionContentV1::remove_reaction(test_message_id(), "❤️".to_string()),
        ];

        for action in cases {
            let legacy_bytes = encode_cbor(&LegacyActionContentV1 {
                action_type: action.action_type,
                target: action.target.clone(),
                payload: action.payload.clone(),
            });
            let decoded = ActionContentV1::decode(&legacy_bytes)
                .unwrap_or_else(|e| panic!("legacy decode failed for {action:?}: {e}"));
            assert_eq!(decoded, action, "legacy decode must be lossless");

            // And the typed accessors must still yield the original content.
            if action.action_type == ACTION_TYPE_EDIT {
                assert_eq!(
                    decoded.edit_payload().expect("edit payload").new_text,
                    action.edit_payload().expect("edit payload").new_text
                );
            } else if action.action_type == ACTION_TYPE_REACTION
                || action.action_type == ACTION_TYPE_REMOVE_REACTION
            {
                assert_eq!(
                    decoded.reaction_payload().expect("reaction payload").emoji,
                    action.reaction_payload().expect("reaction payload").emoji
                );
            }
        }
    }

    /// FROZEN pre-#443 bytes for
    /// `ActionContentV1::edit(test_message_id(), "café 🎉 ok")`, captured from
    /// the legacy array-of-integers encoding.
    ///
    /// Do NOT regenerate this constant casually — it is the whole point. The
    /// mirror-struct tests above rebuild "legacy" bytes from TODAY's types, so
    /// they move with any change to `EditPayload`, `MessageId`, or ciborium
    /// and would keep passing against bytes that no longer resemble what is
    /// stored in real rooms. This literal cannot drift. If it starts failing,
    /// a change has broken the ability to read action payloads written by
    /// every already-deployed client — that is a migration problem, not a
    /// test problem.
    ///
    /// The payload deliberately contains bytes below 0x18 (single-byte CBOR
    /// ints) and above 0x80 (two-byte ints) so both integer widths of the
    /// legacy encoding are exercised.
    const LEGACY_EDIT_ACTION_PRE_443: &str = "a36b616374696f6e5f747970650166746172676574197c42677061796c6f6164981818a11868186e18651877185f1874186518781874186d18631861186618c318a9182018f0189f188e18891820186f186b";

    fn hex_to_bytes(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("valid hex"))
            .collect()
    }

    #[test]
    fn frozen_pre_443_bytes_still_decode() {
        let bytes = hex_to_bytes(LEGACY_EDIT_ACTION_PRE_443);
        let decoded = ActionContentV1::decode(&bytes)
            .expect("bytes written by already-deployed clients must still decode");

        assert_eq!(decoded.action_type, ACTION_TYPE_EDIT);
        assert_eq!(decoded.target, test_message_id());
        assert_eq!(
            decoded.edit_payload().expect("edit payload").new_text,
            "café 🎉 ok",
            "the edited text of a pre-#443 stored edit must survive verbatim"
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
