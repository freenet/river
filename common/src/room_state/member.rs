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
use std::fmt::{Debug, Display};
use std::hash::{Hash, Hasher};

/*
Note that the owner should not be in the members list but for most purposes (eg. sending messages)
they should be treated as if they are in the list. The reason is to avoid storing the owner's
VerifyingKey twice because it's already stored in the ChatRoomParametersV1.
*/

#[derive(Serialize, Deserialize, Eq, PartialEq, Clone, Debug, Default)]
pub struct MembersV1 {
    pub members: Vec<AuthorizedMember>,
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
        let members_by_id = self.members_by_member_id();

        for member in &self.members {
            if member.member.id() == owner_id {
                return Err("Owner should not be included in the members list".to_string());
            }
            if member.member.member_vk == parameters.owner {
                return Err(
                    "Member cannot have the same verifying key as the room owner".to_string(),
                );
            }
            if member.member.invited_by == member.member.id() {
                return Err("Self-invitation detected".to_string());
            }

            // Verify the full invite chain with Ed25519 signature checks
            self.get_invite_chain_with_lookup(member, parameters, &members_by_id)?;
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
            // Build a combined lookup map that includes both existing members
            // AND members being added in this delta. This is necessary because
            // during merge, a member and their inviter may both be in the delta
            // (e.g., member B invited by member A, both being added from the
            // other state). Without this, verify would fail with "Inviter not found".
            let mut combined_members_by_id = self.members_by_member_id();
            for member in &delta.added {
                combined_members_by_id
                    .entry(member.member.id())
                    .or_insert(member);
            }

            // Verify that all new members have valid invites
            for member in &delta.added {
                self.verify_member_invite_with_lookup(member, parameters, &combined_members_by_id)?;
            }

            // Add ALL new members (deduplicated), let remove_excess_members handle trimming.
            // This ensures CRDT convergence: regardless of delta order, the same set of
            // members will be kept based on the deterministic removal criteria.
            for member in &delta.added {
                // Skip if this member already exists
                if self
                    .members
                    .iter()
                    .any(|m| m.member.id() == member.member.id())
                {
                    continue;
                }
                self.members.push(member.clone());
            }
        }

        // Always check for and remove banned members
        self.remove_banned_members(&parent_state.bans, parameters);

        // Always enforce max members limit
        self.remove_excess_members(parameters, max_members);

        // Sort for deterministic ordering (CRDT convergence requirement)
        self.members.sort_by_key(|m| m.member.id());

        Ok(())
    }
}

impl MembersV1 {
    /// Verify a member's invite chain using a pre-built lookup map.
    /// The lookup map should include both existing members AND delta members
    /// when called during apply_delta, so that inviters in the same delta
    /// can be found.
    fn verify_member_invite_with_lookup(
        &self,
        member: &AuthorizedMember,
        parameters: &ChatRoomParametersV1,
        members_by_id: &HashMap<MemberId, &AuthorizedMember>,
    ) -> Result<(), String> {
        if member.member.invited_by == parameters.owner_id() {
            // Member was invited by the owner, verify signature against owner's key
            member
                .verify_signature(&parameters.owner)
                .map_err(|e| format!("Invalid signature for member invited by owner: {}", e))?;
        } else {
            // Member was invited by another member, verify the invite chain
            self.get_invite_chain_with_lookup(member, parameters, members_by_id)?;
        }
        Ok(())
    }
}

impl MembersV1 {
    /// Returns true if the given member_id invited the target_id, properly handling both
    /// regular members and the room owner. Use this instead of checking the members list directly.
    pub fn is_inviter_of(
        &self,
        member_id: MemberId,
        target_id: MemberId,
        params: &ChatRoomParametersV1,
    ) -> bool {
        if member_id == params.owner_id() {
            // Check if target was invited by owner
            self.members
                .iter()
                .find(|m| m.member.id() == target_id)
                .map(|m| m.member.invited_by == member_id)
                .unwrap_or(false)
        } else {
            // Check regular members
            self.members
                .iter()
                .find(|m| m.member.id() == target_id)
                .map(|m| m.member.invited_by == member_id)
                .unwrap_or(false)
        }
    }

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
    /// until the limit is satisfied. When chain lengths are equal, remove the member with the highest MemberId
    /// for deterministic ordering (CRDT convergence requirement).
    fn remove_excess_members(&mut self, parameters: &ChatRoomParametersV1, max_members: usize) {
        if self.members.len() <= max_members {
            return;
        }

        let members_by_id = self.members_by_member_id();
        let owner_id = parameters.owner_id();

        // Pre-compute chain lengths once for all members (no Ed25519 verification needed)
        let mut chain_lengths: Vec<(MemberId, usize)> = self
            .members
            .iter()
            .map(|m| {
                let len = Self::invite_chain_length(m, owner_id, &members_by_id);
                (m.member.id(), len)
            })
            .collect();

        // Sort by chain length descending, then by MemberId descending for deterministic tie-breaking
        chain_lengths.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.cmp(&a.0)));

        // Collect IDs to remove
        let excess = self.members.len() - max_members;
        let ids_to_remove: HashSet<MemberId> = chain_lengths
            .iter()
            .take(excess)
            .map(|(id, _)| *id)
            .collect();

        self.members
            .retain(|m| !ids_to_remove.contains(&m.member.id()));
    }

    /// Checks for banned members and returns a set of member IDs to be removed if any are found.
    /// Uses chain walking without Ed25519 verification since we only need to check membership,
    /// not cryptographic validity.
    fn check_banned_members(
        &self,
        bans_v1: &BansV1,
        parameters: &ChatRoomParametersV1,
    ) -> Option<HashSet<MemberId>> {
        let banned_user_ids: HashSet<MemberId> =
            bans_v1.0.iter().map(|b| b.ban.banned_user).collect();
        if banned_user_ids.is_empty() {
            return None;
        }

        let members_by_id = self.members_by_member_id();
        let owner_id = parameters.owner_id();
        let mut result = HashSet::new();

        for m in &self.members {
            // Walk the invite chain without Ed25519 verification
            let chain_ids = Self::invite_chain_ids(m, owner_id, &members_by_id);
            if chain_ids.iter().any(|id| banned_user_ids.contains(id)) {
                result.insert(m.member.id());
            }
        }

        if result.is_empty() {
            None
        } else {
            Some(result)
        }
    }

    /// Get the full invite chain with Ed25519 signature verification at each link.
    /// This is the authoritative verification used by `verify()`.
    pub fn get_invite_chain(
        &self,
        member: &AuthorizedMember,
        parameters: &ChatRoomParametersV1,
    ) -> Result<Vec<AuthorizedMember>, String> {
        let members_by_id = self.members_by_member_id();
        self.get_invite_chain_with_lookup(member, parameters, &members_by_id)
    }

    /// Get the full invite chain with Ed25519 signature verification, using a pre-built
    /// HashMap for O(1) member lookups instead of linear scans.
    fn get_invite_chain_with_lookup(
        &self,
        member: &AuthorizedMember,
        parameters: &ChatRoomParametersV1,
        members_by_id: &HashMap<MemberId, &AuthorizedMember>,
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
                let inviter = members_by_id
                    .get(&current_member.member.invited_by)
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

                invite_chain.push((*inviter).clone());
                current_member = inviter;
            }
        }

        Ok(invite_chain)
    }

    /// Walk the invite chain and return the length WITHOUT Ed25519 signature verification.
    /// Used by `remove_excess_members` where we only need chain length for comparison.
    fn invite_chain_length(
        member: &AuthorizedMember,
        owner_id: MemberId,
        members_by_id: &HashMap<MemberId, &AuthorizedMember>,
    ) -> usize {
        let mut length = 0;
        let mut current_id = member.member.invited_by;
        let mut visited = HashSet::new();
        visited.insert(member.member.id());

        while current_id != owner_id {
            if !visited.insert(current_id) {
                break; // Circular chain — will be caught by verify()
            }
            length += 1;
            match members_by_id.get(&current_id) {
                Some(inviter) => current_id = inviter.member.invited_by,
                None => break, // Missing inviter — will be caught by verify()
            }
        }
        length
    }

    /// Walk the invite chain and return all member IDs in the chain WITHOUT Ed25519 verification.
    /// Used by `check_banned_members` where we only need to check if any chain member is banned.
    fn invite_chain_ids(
        member: &AuthorizedMember,
        owner_id: MemberId,
        members_by_id: &HashMap<MemberId, &AuthorizedMember>,
    ) -> Vec<MemberId> {
        let mut chain_ids = vec![member.member.id()];
        let mut current_id = member.member.invited_by;
        let mut visited = HashSet::new();
        visited.insert(member.member.id());

        while current_id != owner_id {
            if !visited.insert(current_id) {
                break;
            }
            chain_ids.push(current_id);
            match members_by_id.get(&current_id) {
                Some(inviter) => current_id = inviter.member.invited_by,
                None => break,
            }
        }
        chain_ids
    }
}

#[derive(Serialize, Deserialize, Eq, PartialEq, Clone, Debug)]
pub struct MembersDelta {
    added: Vec<AuthorizedMember>,
}

impl MembersDelta {
    pub fn new(added: Vec<AuthorizedMember>) -> Self {
        MembersDelta { added }
    }
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

    /// Create an AuthorizedMember with a pre-computed signature.
    /// Use this when signing is done externally (e.g., via delegate).
    pub fn with_signature(member: Member, signature: Signature) -> Self {
        Self { member, signature }
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

/*
Finding a VerifyingKey that would have a MemberId collision would require approximately
3 * 10^59 years on current hardware.
*/
#[derive(Eq, PartialEq, Hash, Serialize, Deserialize, Clone, Ord, PartialOrd, Copy)]
pub struct MemberId(pub FastHash);

impl Display for MemberId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", truncated_base32(&self.0 .0.to_le_bytes()))
    }
}

impl Debug for MemberId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "MemberId({})",
            truncated_base32(&self.0 .0.to_le_bytes())
        )
    }
}

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
    fn test_invite_chain_length() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id: MemberId = owner_verifying_key.into();

        // Build a chain: owner -> m1 -> m2 -> m3
        let (member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, member2_signing_key) = create_test_member(owner_id, member1.id());
        let (member3, _) = create_test_member(owner_id, member2.id());

        let auth_m1 = AuthorizedMember::new(member1.clone(), &owner_signing_key);
        let auth_m2 = AuthorizedMember::new(member2.clone(), &member1_signing_key);
        let auth_m3 = AuthorizedMember::new(member3.clone(), &member2_signing_key);

        let members = MembersV1 {
            members: vec![auth_m1.clone(), auth_m2.clone(), auth_m3.clone()],
        };
        let members_by_id = members.members_by_member_id();

        // Depth 0: member directly invited by owner
        assert_eq!(
            MembersV1::invite_chain_length(&auth_m1, owner_id, &members_by_id),
            0
        );
        // Depth 1: one hop from owner
        assert_eq!(
            MembersV1::invite_chain_length(&auth_m2, owner_id, &members_by_id),
            1
        );
        // Depth 2: two hops from owner
        assert_eq!(
            MembersV1::invite_chain_length(&auth_m3, owner_id, &members_by_id),
            2
        );
    }

    #[test]
    fn test_invite_chain_ids() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id: MemberId = owner_verifying_key.into();

        // Build a chain: owner -> m1 -> m2 -> m3
        let (member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, member2_signing_key) = create_test_member(owner_id, member1.id());
        let (member3, _) = create_test_member(owner_id, member2.id());

        let auth_m1 = AuthorizedMember::new(member1.clone(), &owner_signing_key);
        let auth_m2 = AuthorizedMember::new(member2.clone(), &member1_signing_key);
        let auth_m3 = AuthorizedMember::new(member3.clone(), &member2_signing_key);

        let members = MembersV1 {
            members: vec![auth_m1.clone(), auth_m2.clone(), auth_m3.clone()],
        };
        let members_by_id = members.members_by_member_id();

        // m1 (depth 0): chain is just [m1] (no ancestors other than owner)
        let ids = MembersV1::invite_chain_ids(&auth_m1, owner_id, &members_by_id);
        assert_eq!(ids, vec![member1.id()]);

        // m2 (depth 1): chain is [m2, m1]
        let ids = MembersV1::invite_chain_ids(&auth_m2, owner_id, &members_by_id);
        assert_eq!(ids, vec![member2.id(), member1.id()]);

        // m3 (depth 2): chain is [m3, m2, m1]
        let ids = MembersV1::invite_chain_ids(&auth_m3, owner_id, &members_by_id);
        assert_eq!(ids, vec![member3.id(), member2.id(), member1.id()]);
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
        // Now ALL members are added first, then excess removed deterministically:
        // - member1 has shortest invite chain (invited by owner)
        // - member2, member3, member4 all have same chain length (invited by member1)
        // - One of member2/3/4 is removed based on highest MemberId (deterministic tie-breaker)
        let delta = MembersDelta {
            added: vec![authorized_member3.clone(), authorized_member4.clone()],
        };

        let result = members.apply_delta(&parent_state, &parameters, &Some(delta));
        assert!(result.is_ok());
        assert_eq!(members.members.len(), 3);
        // member1 is always kept (shortest invite chain)
        assert!(members
            .members
            .iter()
            .any(|m| m.member.id() == member1.id()));
        // Exactly 2 of [member2, member3, member4] are kept
        let kept_count = [member2.id(), member3.id(), member4.id()]
            .iter()
            .filter(|id| members.members.iter().any(|m| m.member.id() == **id))
            .count();
        assert_eq!(
            kept_count, 2,
            "Exactly 2 of the 3 equal-chain-length members should be kept"
        );

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
        assert!(
            result.is_err(),
            "Room owner should not be allowed in the members list"
        );
        assert!(result
            .unwrap_err()
            .contains("Owner should not be included in the members list"));
    }
}
