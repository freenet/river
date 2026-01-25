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
// Future: CONTENT_TYPE_BLOB = 3, CONTENT_TYPE_POLL = 4, etc.

/// Current version for text content
pub const TEXT_CONTENT_VERSION: u32 = 1;

/// Current version for action content
pub const ACTION_CONTENT_VERSION: u32 = 1;

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

/// Action message content (content_type = 2)
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct ActionContentV1 {
    /// Type of action (ACTION_TYPE_* constants)
    pub action_type: u32,
    /// Target message ID for the action
    pub target: MessageId,
    /// Action-specific payload (CBOR-encoded)
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

/// Decoded message content for client-side processing
#[derive(Clone, PartialEq, Debug)]
pub enum DecodedContent {
    /// Text message
    Text(TextContentV1),
    /// Action on another message
    Action(ActionContentV1),
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

    /// Get the target message ID if this is an action
    pub fn target_id(&self) -> Option<&MessageId> {
        match self {
            Self::Action(action) => Some(&action.target),
            _ => None,
        }
    }

    /// Get the text content if this is a text message
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(&text.text),
            _ => None,
        }
    }

    /// Get a display string for this content
    pub fn to_display_string(&self) -> String {
        match self {
            Self::Text(text) => text.text.clone(),
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
    fn test_decoded_content_display() {
        let text = DecodedContent::Text(TextContentV1::new("Hello".to_string()));
        assert_eq!(text.to_display_string(), "Hello");

        let unknown = DecodedContent::Unknown {
            content_type: 99,
            content_version: 1,
        };
        assert!(unknown.to_display_string().contains("Unsupported"));
    }
}
