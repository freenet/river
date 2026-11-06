//! Building a message body from what the user wrote, sealed under a room
//! secret in a private room. Kept with each outbox message so a message
//! sealed under a secret the room has since rotated away from can be sealed
//! again under the current one and re-signed (`Outbox::reseal`), rather than
//! refused or sent to a member the rotation excluded.

use river_core::room_state::content::{
    ActionContentV1, ReplyContentV1, TextContentV1, CONTENT_TYPE_REPLY, CONTENT_TYPE_TEXT,
    REPLY_CONTENT_VERSION, TEXT_CONTENT_VERSION,
};
use river_core::room_state::message::{MessageId, RoomMessageBody};

/// What the user wrote, before it is sealed.
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum SealInput {
    Text(String),
    Reply {
        text: String,
        message_id: MessageId,
        author_name: String,
        content_preview: String,
    },
    Action(ActionContentV1),
}

impl SealInput {
    /// The private body, sealed under `secret` at `version`.
    pub fn seal(&self, secret: &[u8; 32], version: u32) -> RoomMessageBody {
        use crate::util::ecies::encrypt_with_symmetric_key;
        match self {
            SealInput::Text(text) => {
                let bytes = TextContentV1::new(text.clone()).encode();
                let (ciphertext, nonce) = encrypt_with_symmetric_key(secret, &bytes);
                RoomMessageBody::private(
                    CONTENT_TYPE_TEXT,
                    TEXT_CONTENT_VERSION,
                    ciphertext,
                    nonce,
                    version,
                )
            }
            SealInput::Reply {
                text,
                message_id,
                author_name,
                content_preview,
            } => {
                let bytes = ReplyContentV1::new(
                    text.clone(),
                    message_id.clone(),
                    author_name.clone(),
                    content_preview.clone(),
                )
                .encode();
                let (ciphertext, nonce) = encrypt_with_symmetric_key(secret, &bytes);
                RoomMessageBody::private(
                    CONTENT_TYPE_REPLY,
                    REPLY_CONTENT_VERSION,
                    ciphertext,
                    nonce,
                    version,
                )
            }
            SealInput::Action(action) => {
                let (ciphertext, nonce) = encrypt_with_symmetric_key(secret, &action.encode());
                RoomMessageBody::private_action(ciphertext, nonce, version)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::ecies::decrypt_with_symmetric_key;
    use freenet_scaffold::util::FastHash;

    fn open(body: &RoomMessageBody, secret: &[u8; 32]) -> (u32, u32, Vec<u8>) {
        match body {
            RoomMessageBody::Private {
                content_type,
                ciphertext,
                nonce,
                secret_version,
                ..
            } => (
                *content_type,
                *secret_version,
                decrypt_with_symmetric_key(secret, ciphertext, nonce).expect("opens"),
            ),
            RoomMessageBody::Public { .. } => panic!("expected a sealed body"),
        }
    }

    /// Each kind seals under the given secret and version, and opens back to
    /// what the user wrote: the reply keeps its context, the action its target.
    #[test]
    fn every_kind_seals_and_opens_to_what_was_written() {
        let secret = [7u8; 32];
        let (ty, version, bytes) = open(&SealInput::Text("hi".into()).seal(&secret, 4), &secret);
        assert_eq!((ty, version), (CONTENT_TYPE_TEXT, 4));
        assert_eq!(TextContentV1::decode(&bytes).unwrap().text, "hi");

        let target = MessageId(FastHash(9));
        let reply = SealInput::Reply {
            text: "re".into(),
            message_id: target.clone(),
            author_name: "Ann".into(),
            content_preview: "orig".into(),
        };
        let (ty, version, bytes) = open(&reply.seal(&secret, 5), &secret);
        assert_eq!((ty, version), (CONTENT_TYPE_REPLY, 5));
        assert_eq!(
            ReplyContentV1::decode(&bytes).unwrap(),
            ReplyContentV1::new("re".into(), target.clone(), "Ann".into(), "orig".into())
        );

        let action = ActionContentV1::edit(target, "new".into());
        let (_, version, bytes) =
            open(&SealInput::Action(action.clone()).seal(&secret, 6), &secret);
        assert_eq!(version, 6);
        assert_eq!(ActionContentV1::decode(&bytes).unwrap(), action);
    }
}
