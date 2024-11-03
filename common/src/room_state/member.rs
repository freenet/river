use crate::room_state::ban::BansV1;
use crate::room_state::ChatRoomParametersV1;
use crate::util::{sign_struct, truncated_base32, verify_struct};
use crate::ChatRoomStateV1;
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use freenet_scaffold::util::{fast_hash, FastHash};
use freenet_scaffold::ComposableState;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::hash::{Hash, Hasher};

/*
 Note that the owner should not be in the members list but for most purposes (eg. sending messages)
 they should be treated as if they are in the list. The reason is to avoid storing the owner's
 VerifyingKey twice because it's already stored in the ChatRoomParametersV1.
 */

#[derive(Serialize, Deserialize, Eq, PartialEq, Clone, Debug)]
pub struct MembersV1 {
    pub members: Vec<AuthorizedMember>,
}

impl Default for MembersV1 {
    fn default() -> Self {
        MembersV1 {
            members: Vec::new(),
        }
    }
}

impl ComposableState for MembersV1 {
    type ParentState = ChatRoomStateV1;
    type Summary = HashSet<MemberId>;
    type Delta = MembersDelta;
    type Parameters = ChatRoomParametersV1;

    fn verify(
        &self,
        parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
    ) -> Result<(), String> {
        if self.members.is_empty() {
            return Ok(());
        }

        if self.members.len() > parent_state.configuration.configuration.max_members {
            return Err(format!(
                "Too many members: {} > {}",
                self.members.len(),
                parent_state.configuration.configuration.max_members
            ));
        }

        let owner_id = parameters.owner_id();

        // Build a map of member IDs to their invited_by IDs to check for loops
        let mut invite_map: HashMap<MemberId, MemberId> = HashMap::new();
        for member in &self.members {
            if member.member.id() == owner_id {
                return Err("Owner should not be included in the members list".to_string());
            }
            if member.member.member_vk == parameters.owner {
                return Err("Member cannot have the same verifying key as the room owner".to_string());
            }

            // Check for invite loops
            let mut current_id = member.member.id();
            let mut visited = HashSet::new();
            visited.insert(current_id);

            while current_id != owner_id {
                let invited_by = member.member.invited_by;
                if invited_by == current_id {
                    return Err(format!("Self-invite detected for member {}", current_id));
                }
                if !visited.insert(invited_by) {
                    return Err(format!("Invite loop detected involving member {}", current_id));
                }
                if invited_by != owner_id && !self.members.iter().any(|m| m.member.id() == invited_by) {
                    return Err(format!("Inviter {} not found for member {}", invited_by, current_id));
                }
                current_id = invited_by;
            }


            // Check for invite loops
            let mut current_id = member.member.id();
            let mut visited = HashSet::new();
            visited.insert(current_id);

            while current_id != owner_id {
                let invited_by = member.member.invited_by;
                if invited_by == current_id {
                    return Err(format!("Self-invite detected for member {}", current_id));
                }
                if !visited.insert(invited_by) {
                    return Err(format!("Invite loop detected involving member {}", current_id));
                }
                if invited_by != owner_id && !self.members.iter().any(|m| m.member.id() == invited_by) {
                    return Err(format!("Inviter {} not found for member {}", invited_by, current_id));
                }
                current_id = invited_by;
            }

            invite_map.insert(member.member.id(), member.member.invited_by);
            self.get_invite_chain(member, parameters)?;
        }
        Ok(())
    }
    fn summarize(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
    ) -> Self::Summary {
        self.members.iter().map(|m| m.member.id()).collect()
    }

    fn delta(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
        old_state_summary: &Self::Summary,
    ) -> Option<Self::Delta> {
        let added = self
            .members
            .iter()
            .filter(|m| !old_state_summary.contains(&m.member.id()))
            .cloned()
            .collect::<Vec<_>>();
        if added.is_empty() {
            None
        } else {
            Some(MembersDelta { added })
        }
    }

    fn apply_delta(
        &mut self,
        parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
        delta: &Option<Self::Delta>,
    ) -> Result<(), String> {
        let max_members = parent_state.configuration.configuration.max_members;

        if let Some(delta) = delta {
            // Verify that all new members have valid invites
            for member in &delta.added {
                self.verify_member_invite(member, parent_state, parameters)?;
            }

            // Add new members, but don't exceed max_members
            for member in &delta.added {
                if self.members.len() < max_members {
                    self.members.push(member.clone());
                } else {
                    break;
                }
            }
        }

        // Always check for and remove banned members
        self.remove_banned_members(&parent_state.bans, parameters);

        // Always enforce max members limit
        self.remove_excess_members(parameters, max_members);

        Ok(())
    }
}

impl MembersV1 {
    fn verify_member_invite(
        &self,
        member: &AuthorizedMember,
        _parent_state: &ChatRoomStateV1,
        parameters: &ChatRoomParametersV1,
    ) -> Result<(), String> {
        if member.member.invited_by == parameters.owner_id() {
            // Member was invited by the owner, verify signature against owner's key
            member
                .verify_signature(&parameters.owner)
                .map_err(|e| format!("Invalid signature for member invited by owner: {}", e))?;
        } else {
            // Member was invited by another member, verify the invite chain
            self.get_invite_chain(member, parameters)?;
        }
        Ok(())
    }
}

impl MembersV1 {
    /// Note: doesn't include owner
    pub fn members_by_member_id(&self) -> HashMap<MemberId, &AuthorizedMember> {
        self.members.iter().map(|m| (m.member.id(), m)).collect()
    }

    /// Checks if there are any banned members or members downstream of banned members in the invite chain
    pub fn has_banned_members(&self, bans_v1: &BansV1, parameters: &ChatRoomParametersV1) -> bool {
        self.check_banned_members(bans_v1, parameters).is_some()
    }

    /// Removes banned members or members downstream of banned members in the invite chain
    fn remove_banned_members(&mut self, bans_v1: &BansV1, _parameters: &ChatRoomParametersV1) {
        let mut banned_ids = HashSet::new();
        for ban in &bans_v1.0 {
            banned_ids.insert(ban.ban.banned_user);
            banned_ids.extend(self.get_downstream_members(ban.ban.banned_user));
        }
        self.members
            .retain(|m| !banned_ids.contains(&m.member.id()));
    }

    /// Helper function to get all downstream members of a given member
    fn get_downstream_members(&self, member_id: MemberId) -> HashSet<MemberId> {
        let mut downstream = HashSet::new();
        let mut to_check = vec![member_id];
        while let Some(current) = to_check.pop() {
            for member in &self.members {
                if member.member.invited_by == current {
                    downstream.insert(member.member.id());
                    to_check.push(member.member.id());
                }
            }
        }
        downstream
    }

    /// If the number of members exceeds the specified limit, remove the members with the longest invite chains
    /// until the limit is satisfied
    fn remove_excess_members(&mut self, parameters: &ChatRoomParametersV1, max_members: usize) {
        while self.members.len() > max_members {
            let member_to_remove = self
                .members
                .iter()
                .max_by_key(|m| self.get_invite_chain(m, parameters).unwrap().len())
                .unwrap()
                .member
                .id();
            self.members.retain(|m| m.member.id() != member_to_remove);
        }
    }

    /// Checks for banned members and returns a set of member IDs to be removed if any are found
    fn check_banned_members(
        &self,
        bans_v1: &BansV1,
        parameters: &ChatRoomParametersV1,
    ) -> Option<HashSet<MemberId>> {
        let mut banned_ids = HashSet::new();
        for m in &self.members {
            if let Ok(invite_chain) = self.get_invite_chain(m, parameters) {
                if invite_chain
                    .iter()
                    .any(|m| bans_v1.0.iter().any(|b| b.ban.banned_user == m.member.id()))
                {
                    banned_ids.insert(m.member.id());
                }
            }
        }
        if banned_ids.is_empty() {
            None
        } else {
            Some(banned_ids)
        }
    }

    pub fn get_invite_chain(
        &self,
        member: &AuthorizedMember,
        parameters: &ChatRoomParametersV1,
    ) -> Result<Vec<AuthorizedMember>, String> {
        let mut invite_chain = Vec::new();
        let mut current_member = member;
        let owner_id = parameters.owner_id();
        let mut visited_members = HashSet::new();

        loop {
            if !visited_members.insert(current_member.member.id()) {
                return Err(format!(
                    "Circular invite chain detected for member {:?}",
                    current_member.member.id()
                ));
            }

            if current_member.member.invited_by == current_member.member.id() {
                return Err(format!(
                    "Self-invitation detected for member {:?}",
                    current_member.member.id()
                ));
            }

            if current_member.member.invited_by == owner_id {
                // Member was directly invited by the owner, so we need to verify their signature against the owner's key
                current_member
                    .verify_signature(&parameters.owner)
                    .map_err(|e| {
                        format!(
                            "Invalid signature for member {:?} invited by owner: {}",
                            current_member.member.id(),
                            e
                        )
                    })?;
                break;
            } else {
                let inviter = self
                    .members
                    .iter()
                    .find(|m| m.member.id() == current_member.member.invited_by)
                    .ok_or_else(|| {
                        format!(
                            "Inviter {:?} not found for member {:?}",
                            current_member.member.invited_by,
                            current_member.member.id()
                        )
                    })?;

                current_member
                    .verify_signature(&inviter.member.member_vk)
                    .map_err(|e| {
                        format!(
                            "Invalid signature for member {:?}: {}",
                            current_member.member.id(),
                            e
                        )
                    })?;

                invite_chain.push(inviter.clone());
                current_member = inviter;
            }
        }

        Ok(invite_chain)
    }
}

#[derive(Serialize, Deserialize, Eq, PartialEq, Clone, Debug)]
pub struct MembersDelta {
    added: Vec<AuthorizedMember>,
}

// TODO: need to generalize to support multiple authorization mechanisms such as ghost keys

#[derive(Serialize, Deserialize, Eq, PartialEq, Clone, Debug)]
pub struct AuthorizedMember {
    pub member: Member,
    pub signature: Signature,
}

impl AuthorizedMember {
    pub fn new(member: Member, inviter_signing_key: &SigningKey) -> Self {
        assert_eq!(
            member.invited_by,
            VerifyingKey::from(inviter_signing_key).into(),
            "The member's invited_by must match the inviter's signing key"
        );
        Self {
            member: member.clone(),
            signature: sign_struct(&member, inviter_signing_key),
        }
    }

    pub fn verify_signature(&self, inviter_vk: &VerifyingKey) -> Result<(), String> {
        verify_struct(&self.member, &self.signature, inviter_vk)
            .map_err(|e| format!("Invalid signature: {}", e))
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
}

impl fmt::Debug for Member {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Member")
            .field(
                "public_key",
                &format_args!("{}", truncated_base32(self.member_vk.as_bytes())),
            )
            .finish()
    }
}

#[derive(Eq, PartialEq, Hash, Serialize, Deserialize, Clone, Debug, Ord, PartialOrd, Copy)]
pub struct MemberId(pub FastHash);

impl From<&VerifyingKey> for MemberId {
    fn from(vk: &VerifyingKey) -> Self {
        MemberId(fast_hash(&vk.to_bytes()))
    }
}

impl From<VerifyingKey> for MemberId {
    fn from(vk: VerifyingKey) -> Self {
        MemberId(fast_hash(&vk.to_bytes()))
    }
}

impl Member {
    pub fn id(&self) -> MemberId {
        self.member_vk.into()
    }
}

impl fmt::Display for MemberId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", truncated_base32(&self.0 .0.to_le_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room_state::ban::{AuthorizedUserBan, UserBan};
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;
    use std::time::SystemTime;

    fn create_test_member(owner_id: MemberId, invited_by: MemberId) -> (Member, SigningKey) {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        let member = Member {
            owner_member_id: owner_id,
            invited_by,
            member_vk: verifying_key,
        };
        (member, signing_key)
    }

    #[test]
    fn test_members_verify() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let (member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, _) = create_test_member(owner_id, member1.id());

        let authorized_member1 = AuthorizedMember::new(member1.clone(), &owner_signing_key);
        let authorized_member2 = AuthorizedMember::new(member2.clone(), &member1_signing_key);

        let members = MembersV1 {
            members: vec![authorized_member1.clone(), authorized_member2.clone()],
        };

        println!("Member1 ID: {:?}", member1.id());
        println!("Member2 ID: {:?}", member2.id());
        println!("Owner ID: {:?}", owner_id);

        let mut parent_state = ChatRoomStateV1::default();
        parent_state.configuration.configuration.max_members = 3;
        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        let result = members.verify(&parent_state, &parameters);
        println!("Verification result: {:?}", result);
        assert!(result.is_ok(), "Verification failed: {:?}", result);

        // Test that including the owner in the members list fails verification
        let owner_member = Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: owner_verifying_key,
        };
        let authorized_owner = AuthorizedMember::new(owner_member, &owner_signing_key);
        let members_with_owner = MembersV1 {
            members: vec![authorized_owner, authorized_member1, authorized_member2],
        };
        let result_with_owner = members_with_owner.verify(&parent_state, &parameters);
        println!("Verification result with owner: {:?}", result_with_owner);
        assert!(
            result_with_owner.is_err(),
            "Verification should fail when owner is included: {:?}",
            result_with_owner
        );
    }

    #[test]
    fn test_members_summarize() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let (member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, _) = create_test_member(owner_id, member1.id());

        let authorized_member1 = AuthorizedMember::new(member1.clone(), &owner_signing_key);
        let authorized_member2 = AuthorizedMember::new(member2.clone(), &member1_signing_key);

        let members = MembersV1 {
            members: vec![authorized_member1, authorized_member2],
        };

        let parent_state = ChatRoomStateV1::default();
        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        let summary = members.summarize(&parent_state, &parameters);
        assert_eq!(summary.len(), 2);
        assert!(summary.contains(&member1.id()));
        assert!(summary.contains(&member2.id()));
    }

    #[test]
    fn test_members_delta() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let (member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, _) = create_test_member(owner_id, member1.id());
        let (member3, _) = create_test_member(owner_id, member1.id());

        let authorized_member1 = AuthorizedMember::new(member1.clone(), &owner_signing_key);
        let authorized_member2 = AuthorizedMember::new(member2.clone(), &member1_signing_key);
        let authorized_member3 = AuthorizedMember::new(member3.clone(), &member1_signing_key);

        let old_members = MembersV1 {
            members: vec![authorized_member1.clone(), authorized_member2.clone()],
        };

        let new_members = MembersV1 {
            members: vec![authorized_member1.clone(), authorized_member3.clone()],
        };

        let parent_state = ChatRoomStateV1::default();
        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        let old_summary = old_members.summarize(&parent_state, &parameters);
        let delta = new_members
            .delta(&parent_state, &parameters, &old_summary)
            .unwrap();

        assert_eq!(delta.added.len(), 1);
        assert_eq!(delta.added[0].member.id(), member3.id());
    }

    #[test]
    fn test_members_apply_delta_simple() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let (member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, _) = create_test_member(owner_id, member1.id());
        let (member3, _) = create_test_member(owner_id, member1.id());

        let authorized_member1 = AuthorizedMember::new(member1.clone(), &owner_signing_key);
        let authorized_member2 = AuthorizedMember::new(member2.clone(), &member1_signing_key);
        let authorized_member3 = AuthorizedMember::new(member3.clone(), &member1_signing_key);

        let original_members = MembersV1 {
            members: vec![authorized_member1.clone(), authorized_member2.clone()],
        };

        let delta = MembersDelta {
            added: vec![authorized_member3.clone()],
        };

        let mut parent_state = ChatRoomStateV1::default();
        parent_state.configuration.configuration.max_members = 3;

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        let mut modified_members = original_members.clone();

        assert!(modified_members
            .apply_delta(&parent_state, &parameters, &Some(delta))
            .is_ok());

        assert_eq!(modified_members.members.len(), 3);
        assert!(modified_members
            .members
            .iter()
            .any(|m| m.member.id() == member1.id()));
        assert!(modified_members
            .members
            .iter()
            .any(|m| m.member.id() == member3.id()));
        assert!(modified_members
            .members
            .iter()
            .any(|m| m.member.id() == member2.id()));
    }

    #[test]
    fn test_authorized_member_validate() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let (member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, _) = create_test_member(owner_id, member1.id());

        let authorized_member1 = AuthorizedMember::new(member1.clone(), &owner_signing_key);
        let authorized_member2 = AuthorizedMember::new(member2.clone(), &member1_signing_key);

        assert!(authorized_member1
            .verify_signature(&owner_verifying_key)
            .is_ok());
        assert!(authorized_member2
            .verify_signature(&member1.member_vk)
            .is_ok());

        // Test with invalid signature
        let invalid_member2 = AuthorizedMember {
            member: member2.clone(),
            signature: Signature::from_bytes(&[0; 64]),
        };
        assert!(invalid_member2
            .verify_signature(&member1.member_vk)
            .is_err());
    }

    #[test]
    fn test_member_id() {
        let owner_id = MemberId(FastHash(0));
        let (member, _) = create_test_member(owner_id, owner_id);
        let member_id = member.id();

        assert_eq!(member_id, member.member_vk.into());
    }

    #[test]
    fn test_verify_self_invited_member() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let (mut member, member_signing_key) = create_test_member(owner_id, owner_id);
        member.invited_by = member.id(); // Self-invite

        let authorized_member = AuthorizedMember::new(member, &member_signing_key);

        let members = MembersV1 {
            members: vec![authorized_member],
        };

        let parent_state = ChatRoomStateV1::default();
        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        let result = members.verify(&parent_state, &parameters);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Self-invitation detected"));
    }

    #[test]
    fn test_verify_circular_invite_chain() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let (mut member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, member2_signing_key) = create_test_member(owner_id, member1.id());
        let (member3, member3_signing_key) = create_test_member(owner_id, member2.id());
        member1.invited_by = member3.id(); // Create a circular chain

        let authorized_member1 = AuthorizedMember::new(member1, &member3_signing_key);
        let authorized_member2 = AuthorizedMember::new(member2, &member1_signing_key);
        let authorized_member3 = AuthorizedMember::new(member3, &member2_signing_key);

        let members = MembersV1 {
            members: vec![authorized_member1, authorized_member2, authorized_member3],
        };

        let parent_state = ChatRoomStateV1::default();
        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        let result = members.verify(&parent_state, &parameters);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("Circular invite chain detected"));
    }

    #[test]
    fn test_check_invite_chain() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        // Test case 1: Valid invite chain
        let (member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, member2_signing_key) = create_test_member(owner_id, member1.id());
        let (member3, _) = create_test_member(owner_id, member2.id());

        let authorized_member1 = AuthorizedMember::new(member1.clone(), &owner_signing_key);
        let authorized_member2 = AuthorizedMember::new(member2, &member1_signing_key);
        let authorized_member3 = AuthorizedMember::new(member3, &member2_signing_key);

        let members = MembersV1 {
            members: vec![authorized_member1, authorized_member2.clone()],
        };

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        let result = members.get_invite_chain(&authorized_member3, &parameters);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 2);

        // Test case 2: Circular invite chain
        let (mut circular_member1, circular_member1_signing_key) =
            create_test_member(owner_id, owner_id);
        let (circular_member2, circular_member2_signing_key) =
            create_test_member(owner_id, circular_member1.id());
        circular_member1.invited_by = circular_member2.id();

        let circular_authorized_member1 =
            AuthorizedMember::new(circular_member1, &circular_member2_signing_key);
        let circular_authorized_member2 =
            AuthorizedMember::new(circular_member2, &circular_member1_signing_key);

        let circular_members = MembersV1 {
            members: vec![
                circular_authorized_member1.clone(),
                circular_authorized_member2,
            ],
        };

        let result = circular_members.get_invite_chain(&circular_authorized_member1, &parameters);
        assert!(result.is_err());
        assert!(result
            .clone()
            .unwrap_err()
            .contains("Circular invite chain detected"));

        // Test case 3: Missing inviter
        let non_existent_inviter_id = MemberId(FastHash(999));
        let (orphan_member, _) = create_test_member(owner_id, non_existent_inviter_id);
        let orphan_authorized_member = AuthorizedMember {
            member: orphan_member,
            signature: Signature::from_bytes(&[0; 64]), // Use a dummy signature
        };

        let result = members.get_invite_chain(&orphan_authorized_member, &parameters);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("Inviter"), "Error message: {}", err);
        assert!(err.contains("not found"), "Error message: {}", err);

        // Test case 4: Invalid signature
        let (invalid_member, _) = create_test_member(owner_id, member1.id());
        let invalid_authorized_member = AuthorizedMember {
            member: invalid_member,
            signature: Signature::from_bytes(&[0; 64]),
        };

        let result = members.get_invite_chain(&invalid_authorized_member, &parameters);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid signature"));
    }

    #[test]
    fn test_has_banned_members() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let (member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, member2_signing_key) = create_test_member(owner_id, member1.id());
        let (member3, _) = create_test_member(owner_id, member2.id());

        let authorized_member1 = AuthorizedMember::new(member1.clone(), &owner_signing_key);
        let authorized_member2 = AuthorizedMember::new(member2.clone(), &member1_signing_key);
        let authorized_member3 = AuthorizedMember::new(member3.clone(), &member2_signing_key);

        let members = MembersV1 {
            members: vec![authorized_member1, authorized_member2, authorized_member3],
        };

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        // Test case 1: No banned members
        let empty_bans = BansV1(vec![]);
        assert!(!members.has_banned_members(&empty_bans, &parameters));

        // Test case 2: One banned member
        let banned_member = UserBan {
            owner_member_id: owner_id,
            banned_at: SystemTime::now(),
            banned_user: member2.id(),
        };
        let authorized_ban = AuthorizedUserBan::new(banned_member, owner_id, &owner_signing_key);
        let bans = BansV1(vec![authorized_ban]);
        assert!(members.has_banned_members(&bans, &parameters));
    }

    #[test]
    fn test_remove_banned_members() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let (member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, member2_signing_key) = create_test_member(owner_id, member1.id());
        let (member3, _) = create_test_member(owner_id, member2.id());
        let (member4, _) = create_test_member(owner_id, member1.id());

        let authorized_member1 = AuthorizedMember::new(member1.clone(), &owner_signing_key);
        let authorized_member2 = AuthorizedMember::new(member2.clone(), &member1_signing_key);
        let authorized_member3 = AuthorizedMember::new(member3.clone(), &member2_signing_key);
        let authorized_member4 = AuthorizedMember::new(member4.clone(), &member1_signing_key);

        let mut members = MembersV1 {
            members: vec![
                authorized_member1.clone(),
                authorized_member2.clone(),
                authorized_member3.clone(),
                authorized_member4.clone(),
            ],
        };

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        // Test case 1: No banned members
        let empty_bans = BansV1(vec![]);
        members.remove_banned_members(&empty_bans, &parameters);
        assert_eq!(members.members.len(), 4);

        // Test case 2: One banned member
        let banned_member = UserBan {
            owner_member_id: owner_id,
            banned_at: SystemTime::now(),
            banned_user: member2.id(),
        };
        let authorized_ban = AuthorizedUserBan::new(banned_member, owner_id, &owner_signing_key);
        let bans = BansV1(vec![authorized_ban]);
        members.remove_banned_members(&bans, &parameters);
        assert_eq!(members.members.len(), 2);
        assert!(members
            .members
            .iter()
            .any(|m| m.member.id() == member1.id()));
        assert!(members
            .members
            .iter()
            .any(|m| m.member.id() == member4.id()));
        assert!(!members
            .members
            .iter()
            .any(|m| m.member.id() == member2.id()));
        assert!(!members
            .members
            .iter()
            .any(|m| m.member.id() == member3.id()));

        // Test case 3: Banning a member with no downstream members
        members = MembersV1 {
            members: vec![
                authorized_member1,
                authorized_member2,
                authorized_member3,
                authorized_member4,
            ],
        };
        let banned_member = UserBan {
            owner_member_id: owner_id,
            banned_at: SystemTime::now(),
            banned_user: member4.id(),
        };
        let authorized_ban = AuthorizedUserBan::new(banned_member, owner_id, &owner_signing_key);
        let bans = BansV1(vec![authorized_ban]);
        members.remove_banned_members(&bans, &parameters);
        assert_eq!(members.members.len(), 3);
        assert!(members
            .members
            .iter()
            .any(|m| m.member.id() == member1.id()));
        assert!(members
            .members
            .iter()
            .any(|m| m.member.id() == member2.id()));
        assert!(members
            .members
            .iter()
            .any(|m| m.member.id() == member3.id()));
        assert!(!members
            .members
            .iter()
            .any(|m| m.member.id() == member4.id()));
    }

    #[test]
    fn test_remove_excess_members() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let (member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, member2_signing_key) = create_test_member(owner_id, member1.id());
        let (member3, _) = create_test_member(owner_id, member2.id());

        let authorized_member1 = AuthorizedMember::new(member1.clone(), &owner_signing_key);
        let authorized_member2 = AuthorizedMember::new(member2.clone(), &member1_signing_key);
        let authorized_member3 = AuthorizedMember::new(member3.clone(), &member2_signing_key);

        let mut members = MembersV1 {
            members: vec![authorized_member1, authorized_member2, authorized_member3],
        };

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        // Test case 1: No excess members
        members.remove_excess_members(&parameters, 3);
        assert_eq!(members.members.len(), 3);

        // Test case 2: One excess member
        members.remove_excess_members(&parameters, 2);
        assert_eq!(members.members.len(), 2);
        assert!(members
            .members
            .iter()
            .any(|m| m.member.id() == member1.id()));
        assert!(members
            .members
            .iter()
            .any(|m| m.member.id() == member2.id()));
        assert!(!members
            .members
            .iter()
            .any(|m| m.member.id() == member3.id()));
    }

    #[test]
    fn test_members_by_member_id() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let (member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, _) = create_test_member(owner_id, member1.id());

        let authorized_member1 = AuthorizedMember::new(member1.clone(), &owner_signing_key);
        let authorized_member2 = AuthorizedMember::new(member2.clone(), &member1_signing_key);

        let members = MembersV1 {
            members: vec![authorized_member1.clone(), authorized_member2.clone()],
        };

        let members_map = members.members_by_member_id();

        assert_eq!(members_map.len(), 2);
        assert_eq!(
            members_map.get(&member1.id()).unwrap().member.id(),
            member1.id()
        );
        assert_eq!(
            members_map.get(&member2.id()).unwrap().member.id(),
            member2.id()
        );
    }

    #[test]
    fn test_members_apply_delta_complex() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let (member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, _) = create_test_member(owner_id, member1.id());
        let (member3, _) = create_test_member(owner_id, member1.id());
        let (member4, _) = create_test_member(owner_id, member1.id());

        let authorized_member1 = AuthorizedMember::new(member1.clone(), &owner_signing_key);
        let authorized_member2 = AuthorizedMember::new(member2.clone(), &member1_signing_key);
        let authorized_member3 = AuthorizedMember::new(member3.clone(), &member1_signing_key);
        let authorized_member4 = AuthorizedMember::new(member4.clone(), &member1_signing_key);

        let mut members = MembersV1 {
            members: vec![authorized_member1.clone(), authorized_member2.clone()],
        };

        let mut parent_state = ChatRoomStateV1::default();
        parent_state.configuration.configuration.max_members = 3;

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        // Test applying delta that would exceed max_members
        let delta = MembersDelta {
            added: vec![authorized_member3.clone(), authorized_member4.clone()],
        };

        let result = members.apply_delta(&parent_state, &parameters, &Some(delta));
        assert!(result.is_ok());
        assert_eq!(members.members.len(), 3);
        assert!(members
            .members
            .iter()
            .any(|m| m.member.id() == member1.id()));
        assert!(members
            .members
            .iter()
            .any(|m| m.member.id() == member2.id()));
        assert!(members
            .members
            .iter()
            .any(|m| m.member.id() == member3.id()));
        assert!(!members
            .members
            .iter()
            .any(|m| m.member.id() == member4.id()));

        // Test applying delta with already existing member
        let delta = MembersDelta {
            added: vec![authorized_member2.clone()],
        };

        let result = members.apply_delta(&parent_state, &parameters, &Some(delta));
        assert!(result.is_ok());
        assert_eq!(members.members.len(), 3);
    }

    #[test]
    fn test_remove_excess_members_edge_cases() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let (member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, _) = create_test_member(owner_id, member1.id());

        let authorized_member1 = AuthorizedMember::new(member1.clone(), &owner_signing_key);
        let authorized_member2 = AuthorizedMember::new(member2.clone(), &member1_signing_key);

        let mut members = MembersV1 {
            members: vec![authorized_member1.clone(), authorized_member2.clone()],
        };

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        // Test with max_members set to 0
        members.remove_excess_members(&parameters, 0);
        assert_eq!(members.members.len(), 0);

        // Reset members
        members.members = vec![authorized_member1.clone(), authorized_member2.clone()];

        // Test with max_members greater than current number of members
        members.remove_excess_members(&parameters, 3);
        assert_eq!(members.members.len(), 2);
    }

    #[test]
    #[should_panic(expected = "The member's invited_by must match the inviter's signing key")]
    fn test_authorized_member_new_mismatch() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let (member, _) = create_test_member(owner_id, owner_id);
        let wrong_signing_key = SigningKey::generate(&mut OsRng);

        AuthorizedMember::new(member, &wrong_signing_key);
    }

    #[test]
    fn test_members_verify_edge_cases() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let mut parent_state = ChatRoomStateV1::default();
        parent_state.configuration.configuration.max_members = 2;

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        // Test with empty member list
        let empty_members = MembersV1 { members: vec![] };
        assert!(empty_members.verify(&parent_state, &parameters).is_ok());

        // Test with maximum allowed number of members
        let (member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, _) = create_test_member(owner_id, member1.id());

        let authorized_member1 = AuthorizedMember::new(member1.clone(), &owner_signing_key);
        let authorized_member2 = AuthorizedMember::new(member2.clone(), &member1_signing_key);

        let max_members = MembersV1 {
            members: vec![authorized_member1, authorized_member2],
        };
        assert!(max_members.verify(&parent_state, &parameters).is_ok());

        // Test with members invited by non-existent members
        let non_existent_signing_key = SigningKey::generate(&mut OsRng);
        let non_existent_verifying_key = VerifyingKey::from(&non_existent_signing_key);
        let non_existent_id = non_existent_verifying_key.into();
        let (invalid_member, _) = create_test_member(owner_id, non_existent_id);
        let invalid_authorized_member =
            AuthorizedMember::new(invalid_member, &non_existent_signing_key);

        let invalid_members = MembersV1 {
            members: vec![invalid_authorized_member],
        };
        assert!(invalid_members.verify(&parent_state, &parameters).is_err());
    }

    #[test]
    fn test_room_owner_key_not_allowed_in_members() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let owner_member = Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: owner_verifying_key,
        };

        let authorized_owner_member = AuthorizedMember::new(owner_member, &owner_signing_key);

        let members = MembersV1 {
            members: vec![authorized_owner_member],
        };

        let mut parent_state = ChatRoomStateV1::default();
        parent_state.configuration.configuration.max_members = 2;

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        let result = members.verify(&parent_state, &parameters);
        assert!(result.is_err(), "Room owner should not be allowed in the members list");
        assert!(result.unwrap_err().contains("Owner should not be included in the members list"));
    }
}
