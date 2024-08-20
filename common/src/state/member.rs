use crate::util::{fast_hash, truncated_base64};
use ed25519_dalek::{Signature, VerifyingKey, SigningKey, Signer};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::hash::{Hash, Hasher};

#[derive(Serialize, Deserialize, Eq, PartialEq, Clone)]
pub struct AuthorizedMember {
    pub member: Member,
    pub invited_by: VerifyingKey,
    pub signature: Signature,
}

impl AuthorizedMember {
    pub fn new(member: Member, invited_by: VerifyingKey, signing_key: &SigningKey) -> Self {
        let mut data_to_sign = Vec::new();
        data_to_sign.extend_from_slice(member.public_key.as_bytes());
        data_to_sign.extend_from_slice(member.nickname.as_bytes());
        data_to_sign.extend_from_slice(invited_by.as_bytes());
        
        let signature = signing_key.sign(&data_to_sign);
        
        Self {
            member,
            invited_by,
            signature,
        }
    }
}

impl Hash for AuthorizedMember {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.member.hash(state);
    }
}

impl fmt::Debug for AuthorizedMember {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizedMember")
            .field("member", &self.member)
            .field("invited_by", &format_args!("{}", truncated_base64(self.invited_by.as_bytes())))
            .field("signature", &format_args!("{}", truncated_base64(self.signature.to_bytes())))
            .finish()
    }
}

#[derive(Serialize, Deserialize, Eq, PartialEq, Hash, Clone)]
pub struct Member {
    pub public_key: VerifyingKey,
    pub nickname: String,
}

impl fmt::Debug for Member {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Member")
            .field("public_key", &format_args!("{}", truncated_base64(self.public_key.as_bytes())))
            .field("nickname", &self.nickname)
            .finish()
    }
}

#[derive(Eq, PartialEq, Hash, Serialize, Deserialize, Clone, Debug, Ord, PartialOrd, Copy)]
pub struct MemberId(pub i32);

impl Member {
    pub fn id(&self) -> MemberId {
        MemberId(fast_hash(&self.public_key.to_bytes()))
    }
}
