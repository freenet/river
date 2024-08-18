use ed25519_dalek::Signature;
use serde::{Deserialize, Serialize};
use std::time::SystemTime;
use crate::member::MemberId;
use crate::util::fast_hash;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AuthorizedMessage {
    pub time: SystemTime,
    pub content: String,
    pub author: MemberId,
    pub signature: Signature,
}

#[derive(Eq, PartialEq, Hash, Serialize, Deserialize, Clone, Debug, Ord, PartialOrd)]
pub struct MessageId(pub i32);

impl AuthorizedMessage {
    pub fn id(&self) -> MessageId {
        MessageId(fast_hash(&self.signature.to_bytes()))
    }
}
