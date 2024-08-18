use ed25519_dalek::Signature;
use serde::{Deserialize, Serialize};
use std::time::SystemTime;
use std::fmt;
use crate::state::member::MemberId;
use crate::util::{fast_hash, truncated_base64};

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct AuthorizedMessage {
    pub time: SystemTime,
    pub content: String,
    pub author: MemberId,
    pub signature: Signature,
}

impl fmt::Debug for AuthorizedMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizedMessage")
            .field("time", &self.time)
            .field("content", &self.content)
            .field("author", &self.author)
            .field("signature", &format_args!("{}", truncated_base64(self.signature.to_bytes())))
            .finish()
    }
}

#[derive(Eq, PartialEq, Hash, Serialize, Deserialize, Clone, Debug, Ord, PartialOrd)]
pub struct MessageId(pub i32);

impl AuthorizedMessage {
    pub fn id(&self) -> MessageId {
        MessageId(fast_hash(&self.signature.to_bytes()))
    }
}
