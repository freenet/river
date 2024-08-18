use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};
use crate::util::fast_hash;

#[derive(Serialize, Deserialize, Eq, PartialEq, Clone, Debug)]
pub struct AuthorizedMember {
    pub member: Member,
    pub invited_by: DebugVerifyingKey,
    pub signature: DebugSignature,
}

impl Hash for AuthorizedMember {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.member.hash(state);
    }
}

#[derive(Serialize, Deserialize, Eq, PartialEq, Hash, Clone, Debug)]
pub struct Member {
    pub public_key: DebugVerifyingKey,
    pub nickname: String,
}

#[derive(Eq, PartialEq, Hash, Serialize, Deserialize, Clone, Debug, Ord, PartialOrd, Copy)]
pub struct MemberId(pub i32);

impl Member {
    pub fn id(&self) -> MemberId {
        MemberId(fast_hash(&self.public_key.0.to_bytes()))
    }
}
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct DebugVerifyingKey(pub VerifyingKey);

impl std::fmt::Debug for DebugVerifyingKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("VerifyingKey")
            .field(&DebugTruncated(self.0.as_bytes()))
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct DebugSigningKey(pub SigningKey);

impl std::fmt::Debug for DebugSigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SigningKey")
            .field(&DebugTruncated(self.0.to_bytes()))
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct DebugSignature(pub Signature);

impl std::fmt::Debug for DebugSignature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Signature")
            .field(&DebugTruncated(self.0.to_bytes()))
            .finish()
    }
}
