use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};
use crate::util::fast_hash;

#[derive(Serialize, Deserialize, Eq, PartialEq, Clone, Debug)]
pub struct AuthorizedMember {
    pub member: Member,
    pub invited_by: VerifyingKey,
    pub signature: Signature,
}

impl Hash for AuthorizedMember {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.member.hash(state);
    }
}

#[derive(Serialize, Deserialize, Eq, PartialEq, Hash, Clone, Debug)]
pub struct Member {
    pub public_key: VerifyingKey,
    pub nickname: String,
}

#[derive(Eq, PartialEq, Hash, Serialize, Deserialize, Clone, Debug, Ord, PartialOrd, Copy)]
pub struct MemberId(pub i32);

impl Member {
    pub fn id(&self) -> MemberId {
        MemberId(fast_hash(&self.public_key.to_bytes()))
    }
}
