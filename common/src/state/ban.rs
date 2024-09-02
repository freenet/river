use crate::state::member::{AuthorizedMember, MemberId};
use ed25519_dalek::{Signature, VerifyingKey, SigningKey, Verifier, Signer};
use serde::{Deserialize, Serialize};
use std::time::SystemTime;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use freenet_scaffold::ComposableState;
use freenet_scaffold::util::{fast_hash, FastHash};
use crate::ChatRoomStateV1;
use crate::state::ChatRoomParametersV1;
use crate::util::{sign_struct, verify_struct};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct BansV1(pub Vec<AuthorizedUserBan>);

impl BansV1 {
    fn get_invalid_bans(&self, parent_state: &ChatRoomStateV1, parameters: &ChatRoomParametersV1) -> HashMap<BanId, String> {
        let member_map = parent_state.members.members_by_member_id();
        let mut invalid_bans = HashMap::new();

        for ban in &self.0 {
            let banning_member = match member_map.get(&ban.banned_by) {
                Some(member) => member,
                None => {
                    invalid_bans.insert(ban.id(), "Banning member not found in member list".to_string());
                    continue;
                }
            };

            let banned_member = match member_map.get(&ban.ban.banned_user) {
                Some(member) => member,
                None => {
                    invalid_bans.insert(ban.id(), "Banned member not found in member list".to_string());
                    continue;
                }
            };

            if ban.banned_by != parameters.owner_id() { // No need to check invite chain if banner is owner
                let member_invite_chain = match parent_state.members.get_invite_chain(banning_member, parameters) {
                    Ok(chain) => chain,
                    Err(e) => {
                        invalid_bans.insert(ban.id(), format!("Error getting invite chain: {}", e));
                        continue;
                    }
                };

                if !member_invite_chain.iter().any(|m| m.member.id() == banned_member.member.id()) {
                    invalid_bans.insert(ban.id(), "Banner is not in the invite chain of the banned member".to_string());
                    continue;
                }
            }
        }

        let extra_bans = self.0.len() as isize - parent_state.configuration.configuration.max_user_bans as isize;
        if extra_bans > 0 {
            // Add oldest extra bans to invalid bans
            let mut extra_bans_vec = self.0.clone();
            extra_bans_vec.sort_by_key(|ban| ban.ban.banned_at);
            extra_bans_vec.reverse();
            for ban in extra_bans_vec.iter().take(extra_bans as usize) {
                invalid_bans.insert(ban.id(), "Exceeded maximum number of user bans".to_string());
            }
        }

        invalid_bans
    }
}

impl Default for BansV1 {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl ComposableState for BansV1 {
    type ParentState = ChatRoomStateV1;
    type Summary = Vec<BanId>;
    type Delta = Vec<AuthorizedUserBan>;
    type Parameters = ChatRoomParametersV1;

    fn verify(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters) -> Result<(), String> {
        if !self.get_invalid_bans(parent_state, parameters).is_empty() { 
            return Err("Invalid bans".to_string())
        } 
        
        Ok(())
    }

    fn summarize(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters) -> Self::Summary {
        self.0.iter().map(|ban| ban.id()).collect()
    }

    fn delta(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters, old_state_summary: &Self::Summary) -> Self::Delta {
        // Identify bans in self.0 that are not in old_state_summary
        self.0.iter().filter(|ban| !old_state_summary.contains(&ban.id())).cloned().collect::<Vec<_>>()
    }

    fn apply_delta(&mut self, parent_state: &Self::ParentState, parameters: &Self::Parameters, delta: &Self::Delta) -> Result<(), String> {
        self.0.extend(delta.iter().cloned());
        let invalid_bans = self.get_invalid_bans(parent_state, parameters);
        self.0.retain(|ban| !invalid_bans.contains_key(&ban.id()));
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AuthorizedUserBan {
    pub ban: UserBan,
    pub banned_by: MemberId,
    pub signature: Signature,
}

impl Eq for AuthorizedUserBan {}

impl Hash for AuthorizedUserBan {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.signature.to_bytes().hash(state);
    }
}

impl AuthorizedUserBan {
    pub fn new( ban: UserBan, banned_by: MemberId, banner_signing_key: &SigningKey) -> Self {
        assert_eq!(MemberId::new(&banner_signing_key.verifying_key()), banned_by);
        
        let signature = sign_struct(&ban, banner_signing_key);
        
        Self {
            ban,
            banned_by,
            signature,
        }
    }
    
    pub fn verify_signature(&self, banner_verifying_key: &VerifyingKey) -> Result<(), String> {
        verify_struct(&self.ban, &self.signature, banner_verifying_key).map_err(|e| format!("Invalid ban signature: {}", e))
    }
    
    pub fn id(&self) -> BanId {
        BanId(fast_hash(&self.signature.to_bytes()))
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct UserBan {
    pub owner_member_id: MemberId,
    pub banned_at: SystemTime,
    pub banned_user: MemberId,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Clone, Hash, Debug)]
pub struct BanId(pub FastHash);
