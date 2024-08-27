use std::collections::HashSet;
use crate::util::{sign_struct, truncated_base64, verify_struct};
use ed25519_dalek::{Signature, VerifyingKey, SigningKey, Signer, Verifier};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::hash::{Hash, Hasher};
use freenet_scaffold::ComposableState;
use freenet_scaffold::util::{fast_hash, FastHash};
use crate::ChatRoomState;

#[derive(Serialize, Deserialize, Eq, PartialEq, Clone)]
pub struct Members {
    pub members: Vec<AuthorizedMember>,
}

impl Default for Members {
    fn default() -> Self {
        Members { members: Vec::new() }
    }
}

impl ComposableState for Members {
    type ParentState = ChatRoomState;
    type Summary = HashSet<MemberId>;
    type Delta = MembersDelta;
    type Parameters = ();

    fn verify(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters) -> Result<(), String> {
        for member in &self.members {
            if !member.validate(&self.members) {
                return Err("Invalid member signature".to_string());
            }
        }
        Ok(())
    }

    fn summarize(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters) -> Self::Summary {
        self.members.iter().map(|m| m.member.id()).collect()
    }

    fn delta(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters, old_state_summary: &Self::Summary) -> Self::Delta {
        let added = self.members.iter().filter(|m| !old_state_summary.contains(&m.member.id())).cloned().collect::<Vec<_>>();
        let removed = old_state_summary.iter().filter(|m| !self.members.iter().any(|am| &am.member.id() == *m)).cloned().collect::<Vec<_>>();
        MembersDelta { added, removed }
    }

    fn apply_delta(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters, delta: &Self::Delta) -> Self {
        let mut members = self.members.clone();
        members.retain(|m| !delta.removed.contains(&m.member.id()));
        members.extend(delta.added.iter().cloned());
        let max_members = parent_state.configuration.configuration.max_members;
        while members.len() > max_members {
            // Remove the member that is most distant from the owner in the invite chain

        }
        Members { members }
    }
}

impl Members {
    
    fn invite_chain(&self, member_id: &MemberId) -> Vec<MemberId> {
        let mut chain = Vec::new();
        let mut current_id = member_id;
        while let Some(member) = self.members.iter().find(|m| &m.member.id() == current_id) {
            chain.push(member.member.id());
            current_id = &member.member.invited_by;
        }
        chain
    }
    
    fn invite_chain_len(&self, member_id: &MemberId) -> usize {
        let mut len = 0;
        let mut current_id = member_id;
        while let Some(member) = self.members.iter().find(|m| &m.member.id() == current_id) {
            len += 1;
            current_id = &member.member.invited_by;
        }
        len
    }
}

#[derive(Serialize, Deserialize, Eq, PartialEq, Clone)]
pub struct MembersDelta {
    added: Vec<AuthorizedMember>,
    removed: Vec<MemberId>,
}

#[derive(Serialize, Deserialize, Eq, PartialEq, Clone)]
pub struct AuthorizedMember {
    pub member: Member,
    pub signature: Signature,
}

impl AuthorizedMember {
    pub fn new(member: Member, inviter_signing_key : SigningKey) -> Self {
        Self {
            member : member.clone(),
            signature: sign_struct(&member, &inviter_signing_key),
        }
    }

    pub fn validate(&self, members : &Vec<AuthorizedMember>) -> bool {
        if !members.iter().any(|m| &self.member.invited_by == &m.member.id()) {
            false
        } else {
            let invited_by_member = members.iter().find(|m| &self.member.invited_by == &m.member.id()).unwrap();
            
            verify_struct(&self.member, &self.signature, &invited_by_member.member.member_vk).is_ok()
        }
    }
}

impl Hash for AuthorizedMember {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.member.hash(state);
    }
}

#[derive(Serialize, Deserialize, Eq, PartialEq, Hash, Clone)]
pub struct Member {
    pub owner_member_id: MemberId,
    pub invited_by: MemberId,
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
pub struct MemberId(pub FastHash);

impl MemberId {
    pub fn new(member_vk: &VerifyingKey) -> Self {
        MemberId(fast_hash(&member_vk.to_bytes()))
    }
}

impl Member {
    pub fn id(&self) -> MemberId {
        MemberId::new(&self.member_vk)
    }
}