use crate::state::member::MemberId;
use crate::util::{fast_hash, truncated_base64};
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::SystemTime;
use anyhow::{Result, anyhow};

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Message {
    pub time: SystemTime,
    pub content: String,
}

#[derive(Clone, PartialEq)]
pub struct AuthorizedMessage {
    pub message: Message,
    pub author: MemberId,
    pub signature: Signature,
}

impl fmt::Debug for AuthorizedMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizedMessage")
            .field("message", &self.message)
            .field("author", &self.author)
            .field("signature", &format_args!("{}", truncated_base64(self.signature.to_bytes())))
            .finish()
    }
}

#[derive(Eq, PartialEq, Hash, Serialize, Deserialize, Clone, Debug, Ord, PartialOrd)]
pub struct MessageId(pub i32);

impl AuthorizedMessage {
    pub fn new(message: Message, author: MemberId, signing_key: &SigningKey) -> Self {
        let serialized_message = cborium::to_vec(&message).expect("Serialization should not fail");
        let signature = signing_key.sign(&serialized_message);
        
        Self {
            message,
            author,
            signature,
        }
    }

    pub fn validate(&self, verifying_key: &VerifyingKey) -> Result<()> {
        let serialized_message = cborium::to_vec(&self.message).map_err(|e| anyhow!("Serialization failed: {}", e))?;
        verifying_key.verify(&serialized_message, &self.signature).map_err(|e| anyhow!("Signature validation failed: {}", e))
    }

    pub fn id(&self) -> MessageId {
        MessageId(fast_hash(&self.signature.to_bytes()))
    }
}
