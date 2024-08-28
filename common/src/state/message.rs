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
            if message.validate(&members_by_id.get(&message.message.author).unwrap().member_vk).is_err() {
                return Err(format!("Invalid message signature: id:{:?} content:{:?}", message.id(), message.message.content));
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

}
