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
    type Summary = Vec<FastHash>;
    type Delta = Vec<AuthorizedMessage>;
    type Parameters = ChatRoomParameters;

    fn verify(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters) -> Result<(), String> {
        
        let members_by_id = parent_state.members.members_by_member_id();
        
        for message in &self.messages {
            if message.validate(&members_by_id.get(&message.message.author).unwrap().member_vk).is_err() {
                return Err("Invalid message signature".to_string());
            }
        }
        
        todo!()
    }

    fn summarize(&self, _parent_state: &Self::ParentState, _parameters: &Self::Parameters) -> Self::Summary {
        todo!()
    }

    fn delta(&self, _parent_state: &Self::ParentState, _parameters: &Self::Parameters, _old_state_summary: &Self::Summary) -> Self::Delta {
        todo!()
    }

    fn apply_delta(&self, _parent_state: &Self::ParentState, _parameters: &Self::Parameters, _delta: &Self::Delta) -> Self {
        todo!()
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

    #[test]
    fn test_message_creation_and_validation() {
        let mut csprng = OsRng;
        let signing_key = SigningKey::generate(&mut csprng);
        let verifying_key = signing_key.verifying_key();

        let message = Message {
            time: SystemTime::UNIX_EPOCH,
            content: "Test message".to_string(),
        };

        let author = MemberId(FastHash(1));

        let authorized_message = AuthorizedMessage::new(0, message.clone(), author, &signing_key);

        // Test that the message was correctly stored
        assert_eq!(authorized_message.message, message);
        assert_eq!(authorized_message.author, author);

        // Test that the signature is valid
        assert!(authorized_message.validate(&verifying_key).is_ok());

        // Test with an incorrect verifying key
        let wrong_signing_key = SigningKey::generate(&mut csprng);
        let wrong_verifying_key = wrong_signing_key.verifying_key();
        assert!(authorized_message.validate(&wrong_verifying_key).is_err());

        // Test message ID generation
        let id1 = authorized_message.id();
        let id2 = authorized_message.id();
        assert_eq!(id1, id2);
    }
}
