use crate::room_state::member::{AuthorizedMember, MemberId};
use crate::room_state::ChatRoomParametersV1;
use crate::util::{sign_struct, verify_struct};
use crate::ChatRoomStateV1;
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use freenet_scaffold::util::{fast_hash, FastHash};
use freenet_scaffold::ComposableState;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::time::SystemTime;

/// Represents a collection of user bans in a chat room
///
/// This structure maintains a list of authorized bans and provides methods
/// to verify, summarize, and apply changes to the ban list while ensuring
/// all bans are valid according to room rules.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct BansV1(pub Vec<AuthorizedUserBan>);

/// Represents different types of validation errors that can occur with bans
#[derive(Debug, Clone, PartialEq)]
pub enum BanValidationError {
    /// The banned member was not found in the member list
    MemberNotFound(MemberId),

    /// The banning member was not found in the member list
    BannerNotFound(MemberId),

    /// The banning member is not in the invite chain of the banned member
    NotInInviteChain(MemberId, MemberId),

    /// A circular invite chain was detected
    SelfInvitationDetected(MemberId),

    /// The inviting member was not found for a member in the chain
    InviterNotFound(MemberId),

    /// The number of bans exceeds the maximum allowed
    ExceededMaximumBans,
}

impl fmt::Display for BanValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BanValidationError::MemberNotFound(id) => {
                write!(f, "Banned member not found in member list: {:?}", id)
            }
            BanValidationError::BannerNotFound(id) => {
                write!(f, "Banning member not found in member list: {:?}", id)
            }
            BanValidationError::NotInInviteChain(banner_id, banned_id) => write!(
                f,
                "Banner {:?} is not in the invite chain of banned member {:?}",
                banner_id, banned_id
            ),
            BanValidationError::SelfInvitationDetected(id) => {
                write!(f, "Self-invitation detected for member {:?}", id)
            }
            BanValidationError::InviterNotFound(id) => {
                write!(f, "Inviting member not found for {:?}", id)
            }
            BanValidationError::ExceededMaximumBans => {
                write!(f, "Exceeded maximum number of user bans")
            }
        }
    }
}

impl BansV1 {
    /// Validates all bans in the collection and returns a map of invalid bans with errors
    ///
    /// This method checks:
    /// - If the banned member still exists, verifies the banning member is in their invite chain
    ///   (unless banner is owner). If the banned member was already removed, the ban is valid.
    /// - If the banning member exists (for non-owner bans where banned member still exists)
    /// - If the number of bans exceeds the maximum allowed
    fn get_invalid_bans(
        &self,
        parent_state: &ChatRoomStateV1,
        parameters: &ChatRoomParametersV1,
    ) -> HashMap<BanId, BanValidationError> {
        let member_map = parent_state.members.members_by_member_id();
        let mut invalid_bans = HashMap::new();

        // Validate each ban
        for ban in &self.0 {
            self.validate_single_ban(ban, &member_map, parameters, &mut invalid_bans);
        }

        // Check for excess bans
        self.identify_excess_bans(parent_state, &mut invalid_bans);

        invalid_bans
    }

    /// Validates a single ban and adds any validation errors to the invalid_bans map
    fn validate_single_ban(
        &self,
        ban: &AuthorizedUserBan,
        member_map: &HashMap<MemberId, &AuthorizedMember>,
        parameters: &ChatRoomParametersV1,
        invalid_bans: &mut HashMap<BanId, BanValidationError>,
    ) {
        // Check if banned member exists - if not, that's OK, they've been removed due to the ban.
        // We can skip the invite chain verification in that case since:
        // 1. The ban signature verification (done separately) proves authenticity
        // 2. The ban has already taken effect (member was removed)
        // 3. The invite chain was valid when the ban was first created and applied
        let banned_member = match member_map.get(&ban.ban.banned_user) {
            Some(member) => member,
            None => {
                // Banned member already removed - ban is valid, skip further checks
                return;
            }
        };

        // Skip banning member verification if banner is room owner
        if ban.banned_by != parameters.owner_id() {
            // Check if banning member exists
            let banning_member = match member_map.get(&ban.banned_by) {
                Some(member) => member,
                None => {
                    invalid_bans
                        .insert(ban.id(), BanValidationError::BannerNotFound(ban.banned_by));
                    return;
                }
            };

            // Verify banning member is in the invite chain of banned member
            if let Err(error) = self.validate_invite_chain(
                banned_member,
                banning_member,
                member_map,
                parameters.owner_id(),
                ban.id(),
            ) {
                invalid_bans.insert(ban.id(), error);
            }
        }
    }

    /// Validates that the banning member is in the invite chain of the banned member
    fn validate_invite_chain(
        &self,
        banned_member: &AuthorizedMember,
        banning_member: &AuthorizedMember,
        member_map: &HashMap<MemberId, &AuthorizedMember>,
        owner_id: MemberId,
        _ban_id: BanId,
    ) -> Result<(), BanValidationError> {
        let mut current_member = banned_member;
        let mut chain = Vec::new();

        while current_member.member.id() != owner_id {
            chain.push(current_member);

            // If we found the banning member in the chain, the ban is valid
            if current_member.member.id() == banning_member.member.id() {
                return Ok(());
            }

            // Move up the invite chain
            current_member = match member_map.get(&current_member.member.invited_by) {
                Some(m) => m,
                None => {
                    return Err(BanValidationError::InviterNotFound(
                        current_member.member.id(),
                    ));
                }
            };

            // Check for circular invite chains
            if chain.contains(&current_member) {
                return Err(BanValidationError::SelfInvitationDetected(
                    current_member.member.id(),
                ));
            }
        }

        // If we reached the owner without finding the banning member, the ban is invalid
        Err(BanValidationError::NotInInviteChain(
            banning_member.member.id(),
            banned_member.member.id(),
        ))
    }

    /// Identifies bans that exceed the maximum allowed limit.
    /// When timestamps are equal, uses BanId as a secondary sort key
    /// for deterministic ordering (CRDT convergence requirement).
    fn identify_excess_bans(
        &self,
        parent_state: &ChatRoomStateV1,
        invalid_bans: &mut HashMap<BanId, BanValidationError>,
    ) {
        let max_bans = parent_state.configuration.configuration.max_user_bans;
        let extra_bans = self.0.len() as isize - max_bans as isize;

        if extra_bans > 0 {
            // Add oldest extra bans to invalid bans
            // Sort by timestamp (newest first), with BanId as tie-breaker
            let mut extra_bans_vec = self.0.clone();
            extra_bans_vec.sort_by(|a, b| {
                // Primary: sort by timestamp (will be reversed, so older = later in list)
                // Secondary: sort by BanId for deterministic tie-breaking
                a.ban
                    .banned_at
                    .cmp(&b.ban.banned_at)
                    .then_with(|| a.id().cmp(&b.id()))
            });
            extra_bans_vec.reverse();

            for ban in extra_bans_vec.iter().take(extra_bans as usize) {
                invalid_bans.insert(ban.id(), BanValidationError::ExceededMaximumBans);
            }
        }
    }
}

impl ComposableState for BansV1 {
    type ParentState = ChatRoomStateV1;
    type Summary = HashSet<BanId>;
    type Delta = Vec<AuthorizedUserBan>;
    type Parameters = ChatRoomParametersV1;

    /// Verifies that all bans in the collection are valid
    ///
    /// Checks that:
    /// - All bans have valid signatures
    /// - Banning members are authorized to ban (in invite chain)
    /// - The number of bans doesn't exceed the maximum allowed
    fn verify(
        &self,
        parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
    ) -> Result<(), String> {
        let invalid_bans = self.get_invalid_bans(parent_state, parameters);
        if !invalid_bans.is_empty() {
            let error_messages: Vec<String> = invalid_bans
                .iter()
                .map(|(id, error)| format!("{:?}: {}", id, error))
                .collect();
            return Err(format!("Invalid bans: {}", error_messages.join(", ")));
        }

        // Check if the number of bans exceeds the maximum allowed
        if self.0.len() > parent_state.configuration.configuration.max_user_bans {
            return Err(format!(
                "Number of bans ({}) exceeds the maximum allowed ({})",
                self.0.len(),
                parent_state.configuration.configuration.max_user_bans
            ));
        }

        let members_by_id = parent_state.members.members_by_member_id();

        let owner_vk = parameters.owner;
        let owner_id = parameters.owner_id();

        // Verify signatures for all bans
        for ban in &self.0 {
            if ban.banned_by == owner_id {
                ban.verify_signature(&owner_vk)
                    .map_err(|e| format!("Invalid ban signature: {}", e))?;
            } else if let Some(banning_member) = members_by_id.get(&ban.banned_by) {
                ban.verify_signature(&banning_member.member.member_vk)
                    .map_err(|e| format!("Invalid ban signature: {}", e))?;
            } else {
                // Banning member not in current members list. This can happen
                // during merge when bans are applied before the members delta.
                // Skip signature verification here — clean_orphaned_bans will
                // remove this ban after all fields are applied if the banning
                // member truly doesn't exist in the final state.
            }
        }

        Ok(())
    }

    /// Creates a summary of the current ban state
    ///
    /// Returns a set of all ban IDs currently in the collection
    fn summarize(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
    ) -> Self::Summary {
        self.0.iter().map(|ban| ban.id()).collect()
    }

    /// Computes the difference between current ban state and old state
    ///
    /// Returns a vector of bans that exist in the current state but not in the old state,
    /// or None if there are no differences
    fn delta(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
        old_state_summary: &Self::Summary,
    ) -> Option<Self::Delta> {
        // Identify bans in self.0 that are not in old_state_summary
        let delta = self
            .0
            .iter()
            .filter(|ban| !old_state_summary.contains(&ban.id()))
            .cloned()
            .collect::<Vec<_>>();
        if delta.is_empty() {
            None
        } else {
            Some(delta)
        }
    }

    /// Applies changes from a delta to the current ban state
    ///
    /// This method:
    /// - Checks for duplicate bans
    /// - Verifies all new bans are valid
    /// - Adds the new bans to the collection
    /// - Removes oldest bans if the total exceeds max_user_bans
    ///
    /// Returns an error if any ban in the delta is invalid or already exists
    fn apply_delta(
        &mut self,
        parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
        delta: &Option<Self::Delta>,
    ) -> Result<(), String> {
        if let Some(delta) = delta {
            // Check for duplicate bans
            let existing_ban_ids: std::collections::HashSet<_> =
                self.0.iter().map(|ban| ban.id()).collect();
            for new_ban in delta {
                if existing_ban_ids.contains(&new_ban.id()) {
                    return Err(format!("Duplicate ban detected: {:?}", new_ban.id()));
                }
            }

            // Create a temporary BansV1 with the new bans
            let mut temp_bans = self.clone();
            temp_bans.0.extend(delta.iter().cloned());

            // Remove oldest bans if we exceed the limit
            let max_bans = parent_state.configuration.configuration.max_user_bans;
            if temp_bans.0.len() > max_bans {
                // Sort by banned_at time (oldest first), with BanId tie-breaking
                // for deterministic ordering (CRDT convergence requirement)
                temp_bans.0.sort_by(|a, b| {
                    a.ban
                        .banned_at
                        .cmp(&b.ban.banned_at)
                        .then_with(|| a.id().cmp(&b.id()))
                });
                // Remove oldest bans to get back to the limit
                let to_remove = temp_bans.0.len() - max_bans;
                temp_bans.0.drain(0..to_remove);
            }

            // Verify the temporary room_state (excluding the max_bans check since we just enforced it)
            if let Err(e) = temp_bans.verify(parent_state, parameters) {
                return Err(format!("Invalid delta: {}", e));
            }

            // If verification passes, update the actual room_state
            self.0 = temp_bans.0;
        }

        // Sort for deterministic ordering (CRDT convergence requirement)
        self.0.sort_by(|a, b| {
            a.ban
                .banned_at
                .cmp(&b.ban.banned_at)
                .then_with(|| a.id().cmp(&b.id()))
        });

        Ok(())
    }
}

/// A user ban with authorization proof
///
/// Contains the ban details, the ID of the member who created the ban,
/// and a cryptographic signature proving the ban's authenticity
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
    /// Creates a new authorized ban
    ///
    /// Signs the ban with the provided signing key and verifies that the
    /// banned_by ID matches the public key derived from the signing key
    pub fn new(ban: UserBan, banned_by: MemberId, banner_signing_key: &SigningKey) -> Self {
        assert_eq!(
            MemberId::from(banner_signing_key.verifying_key()),
            banned_by
        );

        let signature = sign_struct(&ban, banner_signing_key);

        Self {
            ban,
            banned_by,
            signature,
        }
    }

    /// Create an AuthorizedUserBan with a pre-computed signature.
    /// Use this when signing is done externally (e.g., via delegate).
    pub fn with_signature(ban: UserBan, banned_by: MemberId, signature: Signature) -> Self {
        Self {
            ban,
            banned_by,
            signature,
        }
    }

    /// Verifies that the ban's signature is valid
    ///
    /// Checks that the signature was created by the key corresponding to the provided verifying key
    pub fn verify_signature(&self, banner_verifying_key: &VerifyingKey) -> Result<(), String> {
        verify_struct(&self.ban, &self.signature, banner_verifying_key)
            .map_err(|e| format!("Invalid ban signature: {}", e))
    }

    /// Generates a unique identifier for this ban based on its signature
    pub fn id(&self) -> BanId {
        BanId(fast_hash(&self.signature.to_bytes()))
    }
}

/// Contains the core information about a user ban
///
/// Includes the room owner's ID, the time of the ban, and the ID of the banned user
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct UserBan {
    pub owner_member_id: MemberId,
    pub banned_at: SystemTime,
    pub banned_user: MemberId,
}

/// A unique identifier for a ban
///
/// Created from a hash of the ban's signature to ensure uniqueness
#[derive(Serialize, Deserialize, PartialEq, Eq, Clone, Hash, Debug, Ord, PartialOrd)]
pub struct BanId(pub FastHash);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room_state::configuration::AuthorizedConfigurationV1;
    use crate::room_state::member::{AuthorizedMember, Member, MembersV1};
    use ed25519_dalek::SigningKey;
    use std::time::Duration;

    fn create_test_chat_room_state() -> ChatRoomStateV1 {
        // Create a minimal ChatRoomStateV1 for testing
        ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::default(),
            bans: Default::default(),
            members: MembersV1::default(),
            member_info: Default::default(),
            secrets: Default::default(),
            recent_messages: Default::default(),
            upgrade: Default::default(),
            ..Default::default()
        }
    }

    fn create_test_parameters() -> ChatRoomParametersV1 {
        // Create minimal ChatRoomParametersV1 for testing
        let owner_key = SigningKey::generate(&mut rand::thread_rng());
        ChatRoomParametersV1 {
            owner: owner_key.verifying_key(),
        }
    }

    #[test]
    fn test_bans_verify() {
        let mut state = create_test_chat_room_state();
        let params = create_test_parameters();

        // Create some test members
        let owner_key = SigningKey::generate(&mut rand::thread_rng());
        let owner_id: MemberId = owner_key.verifying_key().into();
        let member1_key = SigningKey::generate(&mut rand::thread_rng());
        let member1_id: MemberId = member1_key.verifying_key().into();
        let member2_key = SigningKey::generate(&mut rand::thread_rng());
        let member2_id: MemberId = member2_key.verifying_key().into();

        // Add members to the room_state
        state.members.members.push(AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: owner_key.verifying_key(),
            },
            &owner_key,
        ));
        state.members.members.push(AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: member1_key.verifying_key(),
            },
            &owner_key,
        ));
        state.members.members.push(AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: member1_id,
                member_vk: member2_key.verifying_key(),
            },
            &member1_key,
        ));

        // Update the configuration to allow bans
        state.configuration.configuration.max_user_bans = 5;

        // Test 1: Valid ban by owner
        let ban1 = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: SystemTime::now(),
                banned_user: member1_id,
            },
            owner_id,
            &owner_key,
        );

        let bans = BansV1(vec![ban1]);
        assert!(
            bans.verify(&state, &params).is_ok(),
            "Valid ban should be verified successfully: {:?}",
            bans.verify(&state, &params).err()
        );

        // Test 2: Exceeding max_user_bans
        let mut many_bans = Vec::new();
        for _ in 0..6 {
            many_bans.push(AuthorizedUserBan::new(
                UserBan {
                    owner_member_id: owner_id,
                    banned_at: SystemTime::now(),
                    banned_user: member1_id,
                },
                owner_id,
                &owner_key,
            ));
        }
        let too_many_bans = BansV1(many_bans);
        assert!(
            too_many_bans.verify(&state, &params).is_err(),
            "Exceeding max_user_bans should fail verification"
        );

        // Test 3: Invalid ban (banning member not in member list)
        let invalid_key = SigningKey::generate(&mut rand::thread_rng());
        let invalid_id = invalid_key.verifying_key().into();
        let invalid_ban = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: SystemTime::now(),
                banned_user: member2_id,
            },
            invalid_id,
            &invalid_key,
        );

        let invalid_bans = BansV1(vec![invalid_ban]);
        assert!(
            invalid_bans.verify(&state, &params).is_err(),
            "Invalid ban should fail verification"
        );

        // Test 4: Valid ban by non-owner member
        let ban_by_member = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: SystemTime::now(),
                banned_user: member2_id,
            },
            member1_id,
            &member1_key,
        );

        let member_bans = BansV1(vec![ban_by_member]);
        assert!(
            member_bans.verify(&state, &params).is_ok(),
            "Valid ban by non-owner member should pass verification"
        );
    }

    #[test]
    fn test_bans_summarize() {
        let state = create_test_chat_room_state();
        let params = create_test_parameters();

        let key = SigningKey::generate(&mut rand::thread_rng());
        let id: MemberId = key.verifying_key().into();

        let ban1 = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: id,
                banned_at: SystemTime::now(),
                banned_user: id,
            },
            id,
            &key,
        );

        let ban2 = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: id,
                banned_at: SystemTime::now() + Duration::from_secs(1),
                banned_user: id,
            },
            id,
            &key,
        );

        let bans = BansV1(vec![ban1.clone(), ban2.clone()]);
        let summary = bans.summarize(&state, &params);

        assert_eq!(summary.len(), 2);
        assert!(summary.contains(&ban1.id()));
        assert!(summary.contains(&ban2.id()));
    }

    #[test]
    fn test_bans_delta() {
        let state = create_test_chat_room_state();
        let params = create_test_parameters();

        let key = SigningKey::generate(&mut rand::thread_rng());
        let id: MemberId = key.verifying_key().into();

        let ban1 = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: id,
                banned_at: SystemTime::now(),
                banned_user: id,
            },
            id,
            &key,
        );

        let ban2 = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: id,
                banned_at: SystemTime::now() + Duration::from_secs(1),
                banned_user: id,
            },
            id,
            &key,
        );

        let bans = BansV1(vec![ban1.clone(), ban2.clone()]);

        // Test 1: Empty old summary
        let empty_summary = HashSet::new();
        let delta = bans.delta(&state, &params, &empty_summary);
        assert_eq!(delta, Some(vec![ban1.clone(), ban2.clone()]));

        // Test 2: Partial old summary
        let partial_summary: HashSet<BanId> = vec![ban1.id()].into_iter().collect();
        let delta = bans.delta(&state, &params, &partial_summary);
        assert_eq!(delta, Some(vec![ban2.clone()]));

        // Test 3: Full old summary
        let full_summary: HashSet<BanId> = vec![ban1.id(), ban2.id()].into_iter().collect();
        let delta = bans.delta(&state, &params, &full_summary);
        assert_eq!(delta, None);
    }

    #[test]
    fn test_bans_apply_delta() {
        let mut state = create_test_chat_room_state();
        let params = create_test_parameters();

        let owner_key = SigningKey::generate(&mut rand::thread_rng());
        let owner_id: MemberId = owner_key.verifying_key().into();
        let member_key = SigningKey::generate(&mut rand::thread_rng());
        let member_id: MemberId = member_key.verifying_key().into();

        // Add members to the room_state
        state.members.members.push(AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: owner_key.verifying_key(),
            },
            &owner_key,
        ));
        state.members.members.push(AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: member_key.verifying_key(),
            },
            &owner_key,
        ));

        // Update the configuration to allow bans
        state.configuration.configuration.max_user_bans = 5;

        let mut bans = BansV1::default();

        let new_ban = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: SystemTime::now(),
                banned_user: member_id,
            },
            owner_id,
            &owner_key,
        );

        // Test 1: Apply valid delta
        let delta = vec![new_ban.clone()];
        assert!(
            bans.apply_delta(&state, &params, &Some(delta.clone()))
                .is_ok(),
            "Valid delta should be applied successfully: {:?}",
            bans.apply_delta(&state, &params, &Some(delta)).err()
        );
        assert_eq!(
            bans.0.len(),
            1,
            "Bans should contain one ban after applying delta"
        );
        assert_eq!(bans.0[0], new_ban, "Applied ban should match the new ban");

        // Test 2: Apply delta exceeding max_user_bans - should succeed by removing oldest bans
        let mut many_bans = Vec::new();
        for i in 0..5 {
            many_bans.push(AuthorizedUserBan::new(
                UserBan {
                    owner_member_id: owner_id,
                    // Give each ban a different timestamp so we can verify oldest are removed
                    banned_at: SystemTime::now() + Duration::from_secs(i as u64 + 10),
                    banned_user: member_id,
                },
                owner_id,
                &owner_key,
            ));
        }
        let delta_exceeding_max = Some(many_bans.clone());
        assert!(
            bans.apply_delta(&state, &params, &delta_exceeding_max)
                .is_ok(),
            "Delta exceeding max_user_bans should succeed by removing oldest: {:?}",
            bans.apply_delta(&state, &params, &delta_exceeding_max)
                .err()
        );
        assert_eq!(
            bans.0.len(),
            5,
            "Bans should be at max_user_bans limit after removing oldest"
        );
        // The original new_ban (the oldest) should have been removed
        assert!(
            !bans.0.contains(&new_ban),
            "Oldest ban should have been removed"
        );

        // Test 3: Apply invalid delta (duplicate ban) - use one of the bans still in the list
        let existing_ban = many_bans.last().unwrap().clone();
        let invalid_delta = Some(vec![existing_ban]);
        assert!(
            bans.apply_delta(&state, &params, &invalid_delta).is_err(),
            "Applying duplicate ban should fail: {:?}",
            bans.apply_delta(&state, &params, &invalid_delta).ok()
        );
        assert_eq!(
            bans.0.len(),
            5,
            "State should not change after applying duplicate ban"
        );

        // Test 4: Adding more bans should evict oldest ones but keep max_user_bans
        let mut additional_bans = Vec::new();
        for i in 0..2 {
            additional_bans.push(AuthorizedUserBan::new(
                UserBan {
                    owner_member_id: owner_id,
                    banned_at: SystemTime::now() + Duration::from_secs(i as u64 + 100),
                    banned_user: member_id,
                },
                owner_id,
                &owner_key,
            ));
        }
        assert!(
            bans.apply_delta(&state, &params, &Some(additional_bans))
                .is_ok(),
            "Applying more bans should succeed by evicting oldest: {:?}",
            bans.apply_delta(&state, &params, &Some(Vec::new())).err()
        );
        assert_eq!(
            bans.0.len(),
            5,
            "State should still have max number of bans after evicting oldest"
        );
    }

    #[test]
    fn test_authorized_user_ban() {
        let owner_key = SigningKey::generate(&mut rand::thread_rng());
        let owner_id: MemberId = owner_key.verifying_key().into();
        let member_key = SigningKey::generate(&mut rand::thread_rng());
        let member_id: MemberId = member_key.verifying_key().into();

        let ban = UserBan {
            owner_member_id: owner_id,
            banned_at: SystemTime::now(),
            banned_user: member_id,
        };

        let authorized_ban = AuthorizedUserBan::new(ban.clone(), owner_id, &owner_key);

        // Test 1: Verify signature
        assert!(authorized_ban
            .verify_signature(&owner_key.verifying_key())
            .is_ok());

        // Test 2: Verify signature with wrong key
        let wrong_key = SigningKey::generate(&mut rand::thread_rng());
        assert!(authorized_ban
            .verify_signature(&wrong_key.verifying_key())
            .is_err());

        // Test 3: Check ban ID
        let id1 = authorized_ban.id();
        let id2 = authorized_ban.id();
        assert_eq!(id1, id2);

        // Test 4: Different bans should have different IDs
        let another_ban = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: SystemTime::now() + Duration::from_secs(1),
                banned_user: member_id,
            },
            owner_id,
            &owner_key,
        );
        assert_ne!(authorized_ban.id(), another_ban.id());
    }
}
