use crate::util::{fast_hash, sign_struct, truncated_base64, verify_struct};
use ed25519_dalek::{Signature, VerifyingKey, SigningKey, Signer, Verifier};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::hash::{Hash, Hasher};

#[derive(Serialize, Deserialize, Eq, PartialEq, Clone)]
pub struct AuthorizedMember {
    pub member: Member,
    pub signature: Signature,
}

impl AuthorizedMember {
    pub fn new(member: Member, inviter_signing_key : SigningKey) -> Self {
        assert_eq!(inviter_signing_key.verifying_key(), member.invited_by, "Inviter signing key must match the invited_by field");
        Self {
            member : member.clone(),
            signature: sign_struct(&member, &inviter_signing_key),
        }
    }
    
    pub fn validate(&self, members : &Vec<AuthorizedMember>) -> bool {
        // Verify that the inviter is a member of the chatroom
        if !members.iter().any(|m| &self.member.invited_by == &m.member.member_vk) {
            false
        } else {
            // Verify that the member is signed by the inviter
            verify_struct(&self.member, &self.signature, &self.member.invited_by).is_ok()
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
    pub owner_member_id : MemberId,
    pub invited_by: VerifyingKey,
    pub member_vk: VerifyingKey,
    pub nickname: String,
}

impl fmt::Debug for Member {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Member")
            .field("public_key", &format_args!("{}", truncated_base64(self.member_vk.as_bytes())))
            .field("nickname", &self.nickname)
            .finish()
    }
}

#[derive(Eq, PartialEq, Hash, Serialize, Deserialize, Clone, Debug, Ord, PartialOrd, Copy)]
pub struct MemberId(pub i32);

impl Member {
    pub fn id(&self) -> MemberId {
        MemberId(fast_hash(&self.member_vk.to_bytes()))
    }
}
