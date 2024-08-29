use crate::state::member::MemberId;
use crate::util::{truncated_base64, verify_struct};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::SystemTime;
use freenet_scaffold::ComposableState;
use freenet_scaffold::util::{fast_hash, FastHash};
use crate::{ChatRoomState};
use crate::state::ChatRoomParameters;
use crate::util::sign_struct;

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Messages {
    pub messages: Vec<AuthorizedMessage>,
}

impl ComposableState for Messages {
    type ParentState = ChatRoomState;
    type Summary = Vec<MessageId>;
    type Delta = Vec<AuthorizedMessage>;
    type Parameters = ChatRoomParameters;

    fn verify(&self, parent_state: &Self::ParentState, _parameters: &Self::Parameters) -> Result<(), String> {
        let members_by_id = parent_state.members.members_by_member_id();
        
        for message in &self.messages {
            if let Some(member) = members_by_id.get(&message.message.author) {
                if message.validate(&member.member_vk).is_err() {
                    return Err(format!("Invalid message signature: id:{:?} content:{:?}", message.id(), message.message.content));
                }
            } else {
                return Err(format!("Message author not found: {:?}", message.message.author));
            }
        }
        
        Ok(())
    }

    fn summarize(&self, _parent_state: &Self::ParentState, _parameters: &Self::Parameters) -> Self::Summary {
        self.messages.iter().map (|m| m.id()).collect()
    }

    fn delta(&self, _parent_state: &Self::ParentState, _parameters: &Self::Parameters, old_state_summary: &Self::Summary) -> Self::Delta {
        self.messages.iter().filter(|m| !old_state_summary.contains(&m.id())).cloned().collect()
    }

    fn apply_delta(&self, parent_state: &Self::ParentState, _parameters: &Self::Parameters, delta: &Self::Delta) -> Self {
        let max_recent_messages = parent_state.configuration.configuration.max_recent_messages;
        let max_message_size = parent_state.configuration.configuration.max_message_size;
        let mut messages = self.messages.clone();
        messages.extend(delta.iter().cloned());
        
        // Ensure there are no messages over the size limit
        messages.retain(|m| m.message.content.len() <= max_message_size);
        
        // Sort messages by time
        messages.sort_by(|a, b| a.message.time.cmp(&b.message.time));
        
        // Ensure all messages are signed by a valid member, remove if not
        let members_by_id = parent_state.members.members_by_member_id();
        messages.retain(|m| members_by_id.contains_key(&m.message.author));
        
        // Remove oldest messages if there are too many
        while messages.len() > max_recent_messages {
            messages.remove(0);
        }
        Messages { messages }
    }
}

impl Default for Messages {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Message {
    pub owner_member_id : MemberId,
    pub author: MemberId,
    pub time: SystemTime,
    pub content: String,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthorizedMessage {
    pub message: Message,
    pub signature: Signature,
}


impl fmt::Debug for AuthorizedMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizedMessage")
            .field("message", &self.message)
            .field("signature", &format_args!("{}", truncated_base64(self.signature.to_bytes())))
            .finish()
    }
}

#[derive(Eq, PartialEq, Hash, Serialize, Deserialize, Clone, Debug, Ord, PartialOrd)]
pub struct MessageId(pub FastHash);

impl AuthorizedMessage {
    pub fn new(message: Message, signing_key: &SigningKey) -> Self {
        Self {
            message: message.clone(),
            signature : sign_struct(&message, signing_key),
        }
    }

    pub fn validate(&self, verifying_key: &VerifyingKey) -> Result<(), ed25519_dalek::SignatureError> {
        verify_struct(&self.message, &self.signature, &verifying_key)
    }

    pub fn id(&self) -> MessageId {
        MessageId(fast_hash(&self.signature.to_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;
    use std::collections::HashMap;

    fn create_test_message(owner_id: MemberId, author_id: MemberId) -> Message {
        Message {
            owner_member_id: owner_id,
            author: author_id,
            time: SystemTime::now(),
            content: "Test message".to_string(),
        }
    }

    #[test]
    fn test_authorized_message_new_and_validate() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        let owner_id = MemberId(FastHash(0));
        let author_id = MemberId(FastHash(1));

        let message = create_test_message(owner_id, author_id);
        let authorized_message = AuthorizedMessage::new(message.clone(), &signing_key);

        assert_eq!(authorized_message.message, message);
        assert!(authorized_message.validate(&verifying_key).is_ok());

        // Test with wrong key
        let wrong_key = SigningKey::generate(&mut OsRng).verifying_key();
        assert!(authorized_message.validate(&wrong_key).is_err());
    }

    #[test]
    fn test_message_id() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let owner_id = MemberId(FastHash(0));
        let author_id = MemberId(FastHash(1));

        let message = create_test_message(owner_id, author_id);
        let authorized_message = AuthorizedMessage::new(message, &signing_key);

        let id1 = authorized_message.id();
        let id2 = authorized_message.id();

        assert_eq!(id1, id2);
    }

    #[test]
    fn test_messages_verify() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        let owner_id = MemberId(FastHash(0));
        let author_id = MemberId(FastHash(1));

        let message = create_test_message(owner_id, author_id);
        let authorized_message = AuthorizedMessage::new(message, &signing_key);

        let messages = Messages {
            messages: vec![authorized_message],
        };

        let mut parent_state = ChatRoomState::default();
        parent_state.members.members = vec![
            crate::state::member::AuthorizedMember {
                member: crate::state::member::Member {
                    owner_member_id: owner_id,
                    invited_by: owner_id,
                    member_vk: verifying_key,
                    nickname: "Test User".to_string(),
                },
                signature: Signature::from_bytes(&[0; 64]),
            },
            crate::state::member::AuthorizedMember {
                member: crate::state::member::Member {
                    owner_member_id: owner_id,
                    invited_by: owner_id,
                    member_vk: signing_key.verifying_key(),
                    nickname: "Author User".to_string(),
                },
                signature: Signature::from_bytes(&[0; 64]),
            }
        ];

        let parameters = ChatRoomParameters {
            owner: verifying_key,
        };

        assert!(messages.verify(&parent_state, &parameters).is_ok(), "Valid messages should pass verification");

        // Test with invalid signature
        let mut invalid_messages = messages.clone();
        invalid_messages.messages[0].signature = Signature::from_bytes(&[0; 64]);
        assert!(invalid_messages.verify(&parent_state, &parameters).is_err(), "Messages with invalid signature should fail verification");
    }

    #[test]
    fn test_messages_summarize() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let owner_id = MemberId(FastHash(0));
        let author_id = MemberId(FastHash(1));

        let message1 = create_test_message(owner_id, author_id);
        let message2 = create_test_message(owner_id, author_id);

        let authorized_message1 = AuthorizedMessage::new(message1, &signing_key);
        let authorized_message2 = AuthorizedMessage::new(message2, &signing_key);

        let messages = Messages {
            messages: vec![authorized_message1.clone(), authorized_message2.clone()],
        };

        let parent_state = ChatRoomState::default();
        let parameters = ChatRoomParameters {
            owner: signing_key.verifying_key(),
        };

        let summary = messages.summarize(&parent_state, &parameters);
        assert_eq!(summary.len(), 2);
        assert_eq!(summary[0], authorized_message1.id());
        assert_eq!(summary[1], authorized_message2.id());
    }

    #[test]
    fn test_messages_delta() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let owner_id = MemberId(FastHash(0));
        let author_id = MemberId(FastHash(1));

        let message1 = create_test_message(owner_id, author_id);
        let message2 = create_test_message(owner_id, author_id);
        let message3 = create_test_message(owner_id, author_id);

        let authorized_message1 = AuthorizedMessage::new(message1, &signing_key);
        let authorized_message2 = AuthorizedMessage::new(message2, &signing_key);
        let authorized_message3 = AuthorizedMessage::new(message3, &signing_key);

        let messages = Messages {
            messages: vec![authorized_message1.clone(), authorized_message2.clone(), authorized_message3.clone()],
        };

        let parent_state = ChatRoomState::default();
        let parameters = ChatRoomParameters {
            owner: signing_key.verifying_key(),
        };

        let old_summary = vec![authorized_message1.id(), authorized_message2.id()];
        let delta = messages.delta(&parent_state, &parameters, &old_summary);

        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0], authorized_message3);
    }

    #[test]
    fn test_messages_apply_delta() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let owner_id = MemberId(FastHash(0));
        let author_id = MemberId(FastHash(1));

        let message1 = create_test_message(owner_id, author_id);
        let message2 = create_test_message(owner_id, author_id);
        let message3 = create_test_message(owner_id, author_id);

        let authorized_message1 = AuthorizedMessage::new(message1, &signing_key);
        let authorized_message2 = AuthorizedMessage::new(message2, &signing_key);
        let authorized_message3 = AuthorizedMessage::new(message3, &signing_key);

        let messages = Messages {
            messages: vec![authorized_message1.clone(), authorized_message2.clone()],
        };

        let mut parent_state = ChatRoomState::default();
        parent_state.configuration.configuration.max_recent_messages = 3;
        parent_state.configuration.configuration.max_message_size = 100;
        parent_state.members.members = vec![crate::state::member::AuthorizedMember {
            member: crate::state::member::Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: signing_key.verifying_key(),
                nickname: "Test User".to_string(),
            },
            signature: Signature::from_bytes(&[0; 64]),
        }];

        let parameters = ChatRoomParameters {
            owner: signing_key.verifying_key(),
        };

        let delta = vec![authorized_message3.clone()];
        let new_messages = messages.apply_delta(&parent_state, &parameters, &delta);

        assert_eq!(new_messages.messages.len(), 3, "Expected 3 messages after applying delta");
        assert_eq!(new_messages.messages[0], authorized_message1, "First message should be authorized_message1");
        assert_eq!(new_messages.messages[1], authorized_message2, "Second message should be authorized_message2");
        assert_eq!(new_messages.messages[2], authorized_message3, "Third message should be authorized_message3");

        // Test max_recent_messages limit
        let message4 = create_test_message(owner_id, author_id);
        let authorized_message4 = AuthorizedMessage::new(message4, &signing_key);
        let delta = vec![authorized_message4.clone()];
        let new_messages = new_messages.apply_delta(&parent_state, &parameters, &delta);

        assert_eq!(new_messages.messages.len(), 3, "Expected 3 messages after applying delta with max_recent_messages limit");
        assert_eq!(new_messages.messages[0], authorized_message2, "First message should be authorized_message2 after applying max_recent_messages limit");
        assert_eq!(new_messages.messages[1], authorized_message3, "Second message should be authorized_message3 after applying max_recent_messages limit");
        assert_eq!(new_messages.messages[2], authorized_message4, "Third message should be authorized_message4 after applying max_recent_messages limit");
    }
}
