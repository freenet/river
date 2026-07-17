use crate::room_state::member::{AuthorizedMember, MemberId, MembersV1};
use crate::room_state::member_info::MemberInfoV1;
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

/// Validation errors that can occur with bans.
///
/// Since #410, ban ENFORCEMENT authority (owner / ancestor / deputy) is no
/// longer decided in `verify` — it is recomputed from converged state in
/// `ChatRoomStateV1::post_apply_cleanup`. The former invite-chain /
/// excess-count validation variants were removed with that change; the only
/// remaining `verify`-time rejection is an orphaned ban whose banner was
/// themselves banned.
#[derive(Debug, Clone, PartialEq)]
pub enum BanValidationError {
    /// The banning member is not in the current member list AND was themselves
    /// banned — an orphaned ban.
    BannerNotFound(MemberId),
}

impl fmt::Display for BanValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BanValidationError::BannerNotFound(id) => {
                write!(f, "Banning member not found in member list: {:?}", id)
            }
        }
    }
}

impl BansV1 {
    /// Validates the per-ban orphan constraints and returns a map of invalid
    /// bans with errors. Does NOT enforce the `max_user_bans` ceiling — that is
    /// a whole-collection concern applied as a hard count check in `verify` and
    /// enforced (inert-first) in `ChatRoomStateV1::post_apply_cleanup` (#410).
    fn get_invalid_bans(
        &self,
        parent_state: &ChatRoomStateV1,
        parameters: &ChatRoomParametersV1,
    ) -> HashMap<BanId, BanValidationError> {
        let member_map = parent_state.members.members_by_member_id();
        let mut invalid_bans = HashMap::new();
        let banned_user_ids: HashSet<MemberId> = self.0.iter().map(|b| b.ban.banned_user).collect();

        // Validate each ban
        for ban in &self.0 {
            self.validate_single_ban(
                ban,
                &member_map,
                parameters,
                &mut invalid_bans,
                &banned_user_ids,
            );
        }

        invalid_bans
    }

    /// Validates a single ban and adds any validation errors to the invalid_bans map.
    ///
    /// Note (#410): this does NOT reject a ban merely because the banner is not
    /// a current ancestor of the target. Authority to ENFORCE a ban (owner /
    /// ancestor / deputy, including retroactive deputy revocation) is evaluated
    /// at enforcement time in [`crate::room_state::member::MembersV1::banned_member_ids`]
    /// (run from `post_apply_cleanup`), NOT here. A ban whose banner has no
    /// current authority (for example a revoked deputy) is INERT — it removes
    /// nobody — but must still pass `verify`: a legitimately converged state (a
    /// previously-banned user who rejoined after their deputy was revoked)
    /// would otherwise fail validation and break convergence. Keeping ban
    /// authority out of `verify` is exactly what makes `verify` stable across
    /// deputy-state changes. Ban SIGNATURES are still verified in `verify`, and
    /// the orphaned-ban check (banner was themselves banned) is retained.
    fn validate_single_ban(
        &self,
        ban: &AuthorizedUserBan,
        member_map: &HashMap<MemberId, &AuthorizedMember>,
        parameters: &ChatRoomParametersV1,
        invalid_bans: &mut HashMap<BanId, BanValidationError>,
        banned_user_ids: &HashSet<MemberId>,
    ) {
        // If the banned member is no longer present they were already removed
        // (e.g. by this ban, a cascade, or an inactivity prune); nothing left
        // to enforce, so the ban is valid.
        if !member_map.contains_key(&ban.ban.banned_user) {
            return;
        }

        // Owner bans are always valid.
        if ban.banned_by == parameters.owner_id() {
            return;
        }

        // If the banner is not a current member, distinguish an orphaned ban
        // (the banner was themselves banned) from a still-valid ban by a member
        // who was merely pruned for inactivity. If the banner IS a current
        // member the ban is accepted regardless of the banner's current
        // ancestor/deputy authority (see the doc comment above) — enforcement
        // decides who is actually removed.
        if !member_map.contains_key(&ban.banned_by) && banned_user_ids.contains(&ban.banned_by) {
            // Banner was banned — this ban is orphaned.
            invalid_bans.insert(ban.id(), BanValidationError::BannerNotFound(ban.banned_by));
        }
    }

    /// Per-ban validity + signature checks, EXCLUDING the `max_user_bans`
    /// ceiling. `apply_delta` uses this because it defers cap enforcement to
    /// `ChatRoomStateV1::post_apply_cleanup` (where the converged member_info is
    /// available to evict inert bans before enforcing ones); `verify` layers the
    /// hard count ceiling on top. See #410 review round 1.
    fn verify_excluding_cap(
        &self,
        parent_state: &ChatRoomStateV1,
        parameters: &ChatRoomParametersV1,
    ) -> Result<(), String> {
        let invalid_bans = self.get_invalid_bans(parent_state, parameters);
        if !invalid_bans.is_empty() {
            let error_messages: Vec<String> = invalid_bans
                .iter()
                .map(|(id, error)| format!("{:?}: {}", id, error))
                .collect();
            return Err(format!("Invalid bans: {}", error_messages.join(", ")));
        }

        let members_by_id = parent_state.members.members_by_member_id();
        let owner_vk = parameters.owner;
        let owner_id = parameters.owner_id();

        // Verify signatures for all bans.
        for ban in &self.0 {
            if ban.banned_by == owner_id {
                ban.verify_signature(&owner_vk)
                    .map_err(|e| format!("Invalid ban signature: {}", e))?;
            } else if let Some(banning_member) = members_by_id.get(&ban.banned_by) {
                ban.verify_signature(&banning_member.member.member_vk)
                    .map_err(|e| format!("Invalid ban signature: {}", e))?;
            } else {
                // Banning member not in current members list. This can happen when:
                // 1. During merge when bans are applied before the members delta
                // 2. The banner was pruned for inactivity (no recent messages)
                // In both cases, skip signature verification — we can't verify
                // without the banner's key, and the signature was verified when
                // the ban was first created. post_apply_cleanup will remove
                // truly orphaned bans (where the banner was banned, not pruned).
            }
        }

        Ok(())
    }

    /// Whether `ban` is currently ENFORCING (worth keeping under `max_user_bans`
    /// pressure) rather than INERT (evicted first). Pure function of the
    /// converged `(members + member_info)` state (#410 review round 1).
    ///
    /// - If the target is a **current member**, the ban is enforcing iff its
    ///   banner is currently authorized to ban it
    ///   ([`MembersV1::is_ban_authorized`]). A forged ban on a present member,
    ///   or a revoked-deputy ban whose target rejoined, is inert.
    /// - If the target is **absent** (already removed/pruned), keep the ban only
    ///   if its banner is a legitimate authority holder — the owner, a current
    ///   member, or a member currently listed as a deputy by someone. A forged
    ///   ban by a non-member fake id who is nobody's deputy is inert and evicted
    ///   first, which is what defends the un-ban DoS.
    pub fn ban_is_enforcing(
        ban: &AuthorizedUserBan,
        members_by_id: &HashMap<MemberId, &AuthorizedMember>,
        member_info: &MemberInfoV1,
        owner_id: MemberId,
    ) -> bool {
        let banner = ban.banned_by;
        let target = ban.ban.banned_user;
        if members_by_id.contains_key(&target) {
            MembersV1::is_ban_authorized(banner, target, members_by_id, member_info, owner_id)
        } else {
            banner == owner_id
                || members_by_id.contains_key(&banner)
                || member_info
                    .member_info
                    .iter()
                    .any(|mi| mi.member_info.deputies.contains(&banner))
        }
    }
}

impl ComposableState for BansV1 {
    type ParentState = ChatRoomStateV1;
    type Summary = HashSet<BanId>;
    type Delta = Vec<AuthorizedUserBan>;
    type Parameters = ChatRoomParametersV1;

    /// Verifies that all bans in the collection are valid:
    /// - per-ban orphan constraints hold and all signatures are valid
    ///   (`verify_excluding_cap`), AND
    /// - the number of bans does not exceed `max_user_bans` (the hard ceiling
    ///   on stored state; a legitimately-produced state is already ≤ the cap
    ///   because `post_apply_cleanup` evicts inert-first down to it).
    fn verify(
        &self,
        parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
    ) -> Result<(), String> {
        self.verify_excluding_cap(parent_state, parameters)?;

        if self.0.len() > parent_state.configuration.configuration.max_user_bans {
            return Err(format!(
                "Number of bans ({}) exceeds the maximum allowed ({})",
                self.0.len(),
                parent_state.configuration.configuration.max_user_bans
            ));
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

    /// Applies changes from a delta to the current ban state.
    ///
    /// This method:
    /// - Checks for duplicate bans
    /// - Verifies all new bans are valid (per-ban + signatures), EXCLUDING the
    ///   `max_user_bans` ceiling
    /// - Adds the new bans to the collection
    ///
    /// It deliberately does NOT enforce `max_user_bans` here. The cap is
    /// enforced in `ChatRoomStateV1::post_apply_cleanup`, which evicts INERT
    /// (currently-unauthorized) bans before enforcing ones — using the converged
    /// `member_info`, which is not yet available at this point in the field-apply
    /// order. Capping here (by oldest, as before) would let a flood of
    /// forged/inert bans evict the real moderator bans and un-ban spammers
    /// (#410 review round 1). The transient over-cap set is capped by
    /// post_apply_cleanup at the end of the same whole-state apply; `verify`
    /// still rejects any stored state that is over the cap.
    ///
    /// Returns an error if any ban in the delta is invalid or already exists.
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

            // Create a temporary BansV1 with the new bans and validate WITHOUT
            // the max-cap ceiling (deferred to post_apply_cleanup).
            let mut temp_bans = self.clone();
            temp_bans.0.extend(delta.iter().cloned());
            if let Err(e) = temp_bans.verify_excluding_cap(parent_state, parameters) {
                return Err(format!("Invalid delta: {}", e));
            }
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

        // Test 3: Ban by pruned member (not in member list, not banned) — valid
        // With message-based lifecycle, banners not in the members list who
        // are not themselves banned are considered pruned for inactivity.
        let pruned_key = SigningKey::generate(&mut rand::thread_rng());
        let pruned_id: MemberId = pruned_key.verifying_key().into();
        let pruned_ban = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: SystemTime::now(),
                banned_user: member2_id,
            },
            pruned_id,
            &pruned_key,
        );

        let pruned_bans = BansV1(vec![pruned_ban]);
        assert!(
            pruned_bans.verify(&state, &params).is_ok(),
            "Ban by pruned (non-banned) member should pass verification: {:?}",
            pruned_bans.verify(&state, &params).err()
        );

        // Test 3b: Orphaned ban (banner was banned) — invalid
        let orphaned_key = SigningKey::generate(&mut rand::thread_rng());
        let orphaned_id: MemberId = orphaned_key.verifying_key().into();
        let orphaned_ban = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: SystemTime::now(),
                banned_user: member2_id,
            },
            orphaned_id,
            &orphaned_key,
        );
        // A ban targeting the orphaned member makes them "banned" (not just pruned)
        let ban_of_orphaned = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: SystemTime::now(),
                banned_user: orphaned_id,
            },
            owner_id,
            &owner_key,
        );
        let orphaned_bans = BansV1(vec![orphaned_ban, ban_of_orphaned]);
        assert!(
            orphaned_bans.verify(&state, &params).is_err(),
            "Orphaned ban (banner was banned) should fail verification"
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

        // Test 2: A delta that pushes past max_user_bans NO LONGER caps here.
        // The cap moved to `ChatRoomStateV1::post_apply_cleanup` so it can evict
        // INERT-before-enforcing bans using the converged member_info (#410
        // review round 1). `apply_delta` accumulates all valid bans; the
        // whole-state cleanup enforces the ceiling. (State-level capping +
        // inert-first eviction are covered by the DoS test in
        // `common/tests/deputy_ban_test.rs`.)
        let mut many_bans = Vec::new();
        for i in 0..5 {
            many_bans.push(AuthorizedUserBan::new(
                UserBan {
                    owner_member_id: owner_id,
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
            "Applying more bans should succeed: {:?}",
            bans.apply_delta(&state, &params, &delta_exceeding_max)
                .err()
        );
        assert_eq!(
            bans.0.len(),
            6,
            "apply_delta accumulates without capping (cap is enforced in post_apply_cleanup)"
        );
        assert!(
            bans.0.contains(&new_ban),
            "apply_delta must NOT drop the oldest ban — evicting is post_apply_cleanup's job"
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
            6,
            "State should not change after applying duplicate ban"
        );

        // Test 4: More valid bans keep accumulating (still no cap at this level).
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
            "Applying more bans should succeed: {:?}",
            bans.apply_delta(&state, &params, &Some(Vec::new())).err()
        );
        assert_eq!(
            bans.0.len(),
            8,
            "apply_delta keeps accumulating; the cap is enforced by post_apply_cleanup"
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
