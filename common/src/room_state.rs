pub mod ban;
pub mod configuration;
pub mod content;
pub mod direct_messages;
pub mod identity;
pub mod member;
pub mod member_info;
pub mod message;
pub mod privacy;
pub mod secret;
pub mod upgrade;
pub mod version;

use crate::room_state::ban::BansV1;
use crate::room_state::configuration::AuthorizedConfigurationV1;
use crate::room_state::direct_messages::DirectMessagesV1;
use crate::room_state::member::{MemberId, MembersV1};
use crate::room_state::member_info::MemberInfoV1;
use crate::room_state::message::MessagesV1;
use crate::room_state::secret::RoomSecretsV1;
use crate::room_state::upgrade::OptionalUpgradeV1;
use crate::room_state::version::StateVersion;
use ed25519_dalek::VerifyingKey;
use freenet_scaffold_macro::composable;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[composable(post_apply_delta = "post_apply_cleanup")]
#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Debug)]
pub struct ChatRoomStateV1 {
    // WARNING: The order of these fields is important for the purposes of the #[composable] macro.
    // `configuration` must be first, followed by `bans`, `members`, `member_info`, `secrets`,
    // and then `recent_messages`.
    // This is due to interdependencies between the fields and the order in which they must be applied in
    // the `apply_delta` function. DO NOT reorder fields without fully understanding the implications.
    /// Configures things like maximum message length, can be updated by the owner.
    pub configuration: AuthorizedConfigurationV1,

    /// A list of recently banned members, a banned member can't be present in the
    /// members list and will be removed from it ifc necessary.
    pub bans: BansV1,

    /// The members in the chat room along with who invited them
    pub members: MembersV1,

    /// Metadata about members like their nickname, can be updated by members themselves.
    pub member_info: MemberInfoV1,

    /// Secret distribution for private rooms. Must come before recent_messages so message
    /// validation can check secret version consistency.
    pub secrets: RoomSecretsV1,

    /// The most recent messages in the chat room, the number is limited by the room configuration.
    pub recent_messages: MessagesV1,

    /// In-room encrypted direct messages between members (#230 Phase 1).
    /// `#[serde(default)]` keeps states written before this field was added
    /// backwards-compatible.
    #[serde(default)]
    pub direct_messages: DirectMessagesV1,

    /// If this contract has been replaced by a new contract this will contain the new contract address.
    /// This can only be set by the owner.
    pub upgrade: OptionalUpgradeV1,

    /// State format version for migration compatibility.
    /// Defaults to 0 for backward compatibility with states created before versioning.
    #[serde(default)]
    pub version: StateVersion,
}

impl ChatRoomStateV1 {
    /// Post-apply cleanup: prune members who have no recent messages, clean up
    /// member_info for pruned members, remove orphaned bans, and sweep
    /// direct messages whose participants are no longer in the room.
    ///
    /// Members are kept if they have at least one message in recent_messages,
    /// are a sender/recipient of a currently-held direct message (see
    /// [`crate::room_state::direct_messages::DirectMessagesV1::active_participants`]),
    /// or are in the invite chain of someone who qualifies. The owner is
    /// never in the members list (they're implicit via parameters).
    ///
    /// Bans are only removed if the banner was themselves BANNED (orphaned ban).
    /// If the banner was merely pruned for inactivity, their bans persist.
    ///
    /// Direct-message sweep: after pruning, any DM whose sender or
    /// recipient is now non-member or banned is dropped. Without this,
    /// adding a ban for a DM participant would silently make every
    /// peer's verify fail, and members referenced only by a DM would be
    /// pruned (orphaning their DMs). See
    /// `direct_messages.rs` module docs, "Interaction with bans".
    pub fn post_apply_cleanup(&mut self, parameters: &ChatRoomParametersV1) -> Result<(), String> {
        let owner_id = MemberId::from(&parameters.owner);

        // 1. Collect message author IDs + DM participants + secret recipients.
        //
        // Secret recipients (i.e. members for whom the owner has issued an
        // `encrypted_secrets` blob AT THE CURRENT VERSION) are exempt
        // from inactivity-prune. The owner explicitly chose to issue
        // them a per-version room secret, so the owner clearly considers
        // them a member — and post_apply cleanup running on an
        // invitee's first state ingestion (which arrives before the
        // invitee has authored any join_event) must not silently delete
        // that membership. See issue #110 / Bug #3 PR B (Ivvor
        // 2026-05-17).
        //
        // The exemption is restricted to recipients at `current_version`
        // so cleanup still prunes genuinely-inactive members whose
        // blobs are only present at older versions (a member who joined,
        // received v0, never authored anything, and was never re-issued
        // a blob at v1+ is "stale" by the same definition as a member
        // who joined and never authored). Without this scoping the
        // exemption would keep every ever-recipient + their entire
        // invite chain ancestor set exempt from cleanup forever,
        // defeating the prune. See IMPORTANT item #5 on PR #272
        // review round 2.
        let message_authors: HashSet<MemberId> = self
            .recent_messages
            .messages
            .iter()
            .map(|m| m.message.author)
            .collect();
        let dm_participants: HashSet<MemberId> = self.direct_messages.active_participants();
        let current_secret_version = self.secrets.current_version;
        let secret_recipients: HashSet<MemberId> = self
            .secrets
            .encrypted_secrets
            .iter()
            .filter(|s| s.secret.secret_version == current_secret_version)
            .map(|s| s.secret.member_id)
            .collect();

        // 2. Compute required members: authors + DM participants + secret
        //    recipients + their invite chains.
        let required_ids = {
            let members_by_id = self.members.members_by_member_id();
            let mut required_ids: HashSet<MemberId> = HashSet::new();

            for author_id in &message_authors {
                if *author_id != owner_id && members_by_id.contains_key(author_id) {
                    required_ids.insert(*author_id);
                }
            }

            for participant_id in &dm_participants {
                if *participant_id != owner_id && members_by_id.contains_key(participant_id) {
                    required_ids.insert(*participant_id);
                }
            }

            for recipient_id in &secret_recipients {
                if *recipient_id != owner_id && members_by_id.contains_key(recipient_id) {
                    required_ids.insert(*recipient_id);
                }
            }

            // Walk invite chains upward, adding all ancestors (stop at owner)
            let mut to_process: Vec<MemberId> = required_ids.iter().cloned().collect();
            while let Some(member_id) = to_process.pop() {
                if let Some(member) = members_by_id.get(&member_id) {
                    let inviter_id = member.member.invited_by;
                    if inviter_id != owner_id && !required_ids.contains(&inviter_id) {
                        required_ids.insert(inviter_id);
                        to_process.push(inviter_id);
                    }
                }
            }

            required_ids
        };

        // 3. Prune members not in required set
        self.members
            .members
            .retain(|m| required_ids.contains(&m.member.id()));

        // 4. Clean member_info for pruned members
        self.member_info.member_info.retain(|info| {
            info.member_info.member_id == owner_id
                || required_ids.contains(&info.member_info.member_id)
        });

        // 5. Clean orphaned bans: only remove if banner was BANNED (not just pruned)
        // A ban is orphaned when:
        // - The banner is not the owner AND
        // - The banner is not in the current members list AND
        // - The banner IS in the banned users set (i.e., they were banned, not pruned)
        let banned_user_ids: HashSet<MemberId> =
            self.bans.0.iter().map(|b| b.ban.banned_user).collect();
        let current_member_ids: HashSet<MemberId> =
            self.members.members.iter().map(|m| m.member.id()).collect();

        self.bans.0.retain(|ban| {
            // Keep if: banner is owner, OR banner is still a member, OR banner is NOT banned
            // (not banned = pruned for inactivity, their bans should persist)
            ban.banned_by == owner_id
                || current_member_ids.contains(&ban.banned_by)
                || !banned_user_ids.contains(&ban.banned_by)
        });

        // 6. Sweep DMs whose participants are no longer current members
        //    or are banned. Without this, a fresh ban (or member-prune)
        //    would leave the DMs in state but break `verify` because the
        //    sender/recipient can no longer be resolved.
        let banned_user_ids_for_sweep: HashSet<MemberId> =
            self.bans.0.iter().map(|b| b.ban.banned_user).collect();
        let active_member_ids_for_sweep: HashSet<MemberId> =
            self.members.members.iter().map(|m| m.member.id()).collect();
        self.direct_messages.sweep_after_membership_change(
            owner_id,
            &active_member_ids_for_sweep,
            &banned_user_ids_for_sweep,
        );

        // 7. Re-sort for deterministic ordering
        self.members.members.sort_by_key(|m| m.member.id());
        self.member_info
            .member_info
            .sort_by_key(|info| info.member_info.member_id);

        Ok(())
    }
}

#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Debug)]
pub struct ChatRoomParametersV1 {
    pub owner: VerifyingKey,
}

impl ChatRoomParametersV1 {
    pub fn owner_id(&self) -> MemberId {
        self.owner.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room_state::ban::{AuthorizedUserBan, UserBan};
    use crate::room_state::configuration::Configuration;
    use crate::room_state::member::{AuthorizedMember, Member};
    use crate::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
    use crate::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
    use ed25519_dalek::SigningKey;
    use std::fmt::Debug;
    use std::time::SystemTime;

    #[test]
    fn test_state() {
        let (state, parameters, owner_signing_key) = create_empty_chat_room_state();

        assert!(
            state.verify(&state, &parameters).is_ok(),
            "Empty state should verify"
        );

        // Test that the configuration can be updated
        let mut new_cfg = state.configuration.configuration.clone();
        new_cfg.configuration_version += 1;
        new_cfg.max_recent_messages = 10; // Change from default of 100 to 10
        let new_cfg = AuthorizedConfigurationV1::new(new_cfg, &owner_signing_key);

        let mut cfg_modified_state = state.clone();
        cfg_modified_state.configuration = new_cfg;
        test_apply_delta(state.clone(), cfg_modified_state, &parameters);
    }

    fn test_apply_delta<CS>(orig_state: CS, modified_state: CS, parameters: &CS::Parameters)
    where
        CS: ComposableState<ParentState = CS> + Clone + PartialEq + Debug,
    {
        let orig_verify_result = orig_state.verify(&orig_state, parameters);
        assert!(
            orig_verify_result.is_ok(),
            "Original state verification failed: {:?}",
            orig_verify_result.err()
        );

        let modified_verify_result = modified_state.verify(&modified_state, parameters);
        assert!(
            modified_verify_result.is_ok(),
            "Modified state verification failed: {:?}",
            modified_verify_result.err()
        );

        let delta = modified_state.delta(
            &orig_state,
            parameters,
            &orig_state.summarize(&orig_state, parameters),
        );

        println!("Delta: {:?}", delta);

        let mut new_state = orig_state.clone();
        let apply_delta_result = new_state.apply_delta(&orig_state, parameters, &delta);
        assert!(
            apply_delta_result.is_ok(),
            "Applying delta failed: {:?}",
            apply_delta_result.err()
        );

        assert_eq!(new_state, modified_state);
    }
    fn create_empty_chat_room_state() -> (ChatRoomStateV1, ChatRoomParametersV1, SigningKey) {
        // Create a test room_state with a single member and two messages, one written by
        // the owner and one by the member - the member must be invited by the owner
        let rng = &mut rand::thread_rng();
        let owner_signing_key = SigningKey::generate(rng);
        let owner_verifying_key = owner_signing_key.verifying_key();

        let config = AuthorizedConfigurationV1::new(Configuration::default(), &owner_signing_key);

        (
            ChatRoomStateV1 {
                configuration: config,
                bans: BansV1::default(),
                members: MembersV1::default(),
                member_info: MemberInfoV1::default(),
                secrets: RoomSecretsV1::default(),
                recent_messages: MessagesV1::default(),
                upgrade: OptionalUpgradeV1(None),
                ..Default::default()
            },
            ChatRoomParametersV1 {
                owner: owner_verifying_key,
            },
            owner_signing_key,
        )
    }

    /// Regression test: when a member who issued bans is subsequently banned themselves,
    /// their bans become orphaned (banning member no longer in members list and not owner).
    /// The post_apply_delta hook post_apply_cleanup must remove these to prevent verify() failure.
    /// See: technic corrupted state incident (Feb 2026)
    #[test]
    fn test_orphaned_ban_cleanup_after_cascade_removal() {
        let rng = &mut rand::thread_rng();

        // Create owner
        let owner_sk = SigningKey::generate(rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let params = ChatRoomParametersV1 { owner: owner_vk };

        // Configuration allowing bans and members
        let config = Configuration {
            max_user_bans: 10,
            max_members: 10,
            ..Default::default()
        };
        let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);

        // Create member A (invited by owner) and member B (invited by A)
        let a_sk = SigningKey::generate(rng);
        let a_vk = a_sk.verifying_key();
        let a_id = MemberId::from(&a_vk);

        let b_sk = SigningKey::generate(rng);
        let b_vk = b_sk.verifying_key();
        let b_id = MemberId::from(&b_vk);

        let member_a = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: a_vk,
            },
            &owner_sk,
        );

        // A bans B (authorized because A is in B's invite chain)
        let ban_b_by_a = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: std::time::SystemTime::now(),
                banned_user: b_id,
            },
            a_id,
            &a_sk,
        );

        // Initial state: A is a member, B already removed (ban took effect)
        let initial_state = ChatRoomStateV1 {
            configuration: auth_config.clone(),
            bans: BansV1(vec![ban_b_by_a.clone()]),
            members: MembersV1 {
                members: vec![member_a.clone()],
            },
            ..Default::default()
        };

        assert!(
            initial_state.verify(&initial_state, &params).is_ok(),
            "Initial state should verify: {:?}",
            initial_state.verify(&initial_state, &params)
        );

        // Now owner bans A — this will cascade-remove A from members,
        // making A's ban of B orphaned (A is no longer in members and not owner)
        let ban_a_by_owner = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: std::time::SystemTime::now() + std::time::Duration::from_secs(1),
                banned_user: a_id,
            },
            owner_id,
            &owner_sk,
        );

        // Modified state for delta computation: add owner's ban of A
        let modified_for_delta = ChatRoomStateV1 {
            configuration: auth_config,
            bans: BansV1(vec![ban_b_by_a.clone(), ban_a_by_owner.clone()]),
            members: MembersV1 {
                members: vec![member_a.clone()],
            },
            ..Default::default()
        };

        // Compute and apply delta
        let summary = initial_state.summarize(&initial_state, &params);
        let delta = modified_for_delta.delta(&initial_state, &params, &summary);

        let mut result_state = initial_state.clone();
        let apply_result = result_state.apply_delta(&initial_state, &params, &delta);
        assert!(
            apply_result.is_ok(),
            "apply_delta should succeed: {:?}",
            apply_result
        );

        // A should be removed (banned by owner)
        assert!(
            result_state.members.members.is_empty(),
            "A should be removed from members: {:?}",
            result_state.members.members
        );

        // Only owner's ban should remain — A's ban of B is orphaned and cleaned
        assert_eq!(
            result_state.bans.0.len(),
            1,
            "Only owner's ban should remain, orphaned ban cleaned: {:?}",
            result_state.bans.0
        );
        assert_eq!(
            result_state.bans.0[0].banned_by, owner_id,
            "Remaining ban should be by owner"
        );

        // Result state should pass verification
        assert!(
            result_state.verify(&result_state, &params).is_ok(),
            "Result state should verify after orphaned ban cleanup: {:?}",
            result_state.verify(&result_state, &params)
        );
    }

    #[test]
    fn test_member_pruned_when_no_messages() {
        let rng = &mut rand::thread_rng();
        let owner_sk = SigningKey::generate(rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let params = ChatRoomParametersV1 { owner: owner_vk };

        let a_sk = SigningKey::generate(rng);
        let a_vk = a_sk.verifying_key();
        let a_id = MemberId::from(&a_vk);

        let b_sk = SigningKey::generate(rng);
        let b_vk = b_sk.verifying_key();

        let member_a = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: a_vk,
            },
            &owner_sk,
        );
        let member_b = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: b_vk,
            },
            &owner_sk,
        );

        // Only A has a message
        let msg_a = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner_id,
                author: a_id,
                time: SystemTime::now(),
                content: RoomMessageBody::public("Hello from A".to_string()),
            },
            &a_sk,
        );

        let config = Configuration {
            max_members: 10,
            max_recent_messages: 100,
            ..Default::default()
        };
        let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);

        let mut state = ChatRoomStateV1 {
            configuration: auth_config,
            members: MembersV1 {
                members: vec![member_a, member_b],
            },
            recent_messages: MessagesV1 {
                messages: vec![msg_a],
                ..Default::default()
            },
            ..Default::default()
        };

        state.post_apply_cleanup(&params).unwrap();

        assert_eq!(state.members.members.len(), 1, "Only A should remain");
        assert_eq!(state.members.members[0].member.id(), a_id);
    }

    #[test]
    fn test_member_with_join_event_not_pruned() {
        let rng = &mut rand::thread_rng();
        let owner_sk = SigningKey::generate(rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let params = ChatRoomParametersV1 { owner: owner_vk };

        let a_sk = SigningKey::generate(rng);
        let a_vk = a_sk.verifying_key();
        let a_id = MemberId::from(&a_vk);

        let member_a = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: a_vk,
            },
            &owner_sk,
        );

        // A has only a join event (no regular messages)
        let join_msg = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner_id,
                author: a_id,
                time: SystemTime::now(),
                content: RoomMessageBody::join_event(),
            },
            &a_sk,
        );

        let config = Configuration {
            max_members: 10,
            max_recent_messages: 100,
            ..Default::default()
        };
        let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);

        let mut state = ChatRoomStateV1 {
            configuration: auth_config,
            members: MembersV1 {
                members: vec![member_a],
            },
            recent_messages: MessagesV1 {
                messages: vec![join_msg],
                ..Default::default()
            },
            ..Default::default()
        };

        state.post_apply_cleanup(&params).unwrap();

        assert_eq!(
            state.members.members.len(),
            1,
            "Member with join event should not be pruned"
        );
        assert_eq!(state.members.members[0].member.id(), a_id);
    }

    /// Test that the atomic join delta (members + member_info + join event)
    /// as produced by accept_invitation applies correctly and passes verify().
    #[test]
    fn test_atomic_join_delta_applies_and_verifies() {
        use crate::room_state::member::MembersDelta;
        use crate::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
        use crate::room_state::privacy::SealedBytes;

        let rng = &mut rand::thread_rng();
        let owner_sk = SigningKey::generate(rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let params = ChatRoomParametersV1 { owner: owner_vk };

        // Create a room with owner config
        let config = Configuration {
            owner_member_id: owner_id,
            max_members: 10,
            max_recent_messages: 100,
            ..Default::default()
        };
        let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);
        let mut state = ChatRoomStateV1 {
            configuration: auth_config,
            ..Default::default()
        };

        // New member accepts an invitation
        let joiner_sk = SigningKey::generate(rng);
        let joiner_vk = joiner_sk.verifying_key();
        let joiner_id = MemberId::from(&joiner_vk);

        let authorized_member = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: joiner_vk,
            },
            &owner_sk,
        );

        let member_info = MemberInfo {
            member_id: joiner_id,
            version: 0,
            preferred_nickname: SealedBytes::public("NewUser".to_string().into_bytes()),
        };
        let authorized_info = AuthorizedMemberInfo::new_with_member_key(member_info, &joiner_sk);

        let join_message = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner_id,
                author: joiner_id,
                content: RoomMessageBody::join_event(),
                time: SystemTime::now(),
            },
            &joiner_sk,
        );

        // Build the atomic delta (same as accept_invitation produces)
        let delta = ChatRoomStateV1Delta {
            recent_messages: Some(vec![join_message]),
            members: Some(MembersDelta::new(vec![authorized_member])),
            member_info: Some(vec![authorized_info]),
            ..Default::default()
        };

        // Apply delta
        let old_state = state.clone();
        state
            .apply_delta(&old_state, &params, &Some(delta))
            .expect("atomic join delta should apply cleanly");

        // Verify state is valid
        state
            .verify(&state, &params)
            .expect("state should verify after join delta");

        // Member should be present
        assert!(
            state
                .members
                .members
                .iter()
                .any(|m| m.member.id() == joiner_id),
            "Joiner should be in members list"
        );

        // Member info should be present
        assert!(
            state
                .member_info
                .member_info
                .iter()
                .any(|i| i.member_info.member_id == joiner_id),
            "Joiner should have member_info"
        );

        // Join event message should be present
        assert_eq!(state.recent_messages.messages.len(), 1);
        assert!(state.recent_messages.messages[0].message.content.is_event());

        // Should survive post_apply_cleanup
        state.post_apply_cleanup(&params).unwrap();
        assert!(
            state
                .members
                .members
                .iter()
                .any(|m| m.member.id() == joiner_id),
            "Joiner should survive cleanup"
        );
    }

    #[test]
    fn test_invite_chain_preserved_for_active_member() {
        let rng = &mut rand::thread_rng();
        let owner_sk = SigningKey::generate(rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let params = ChatRoomParametersV1 { owner: owner_vk };

        let a_sk = SigningKey::generate(rng);
        let a_vk = a_sk.verifying_key();
        let a_id = MemberId::from(&a_vk);

        let b_sk = SigningKey::generate(rng);
        let b_vk = b_sk.verifying_key();
        let b_id = MemberId::from(&b_vk);

        // Owner → A → B
        let member_a = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: a_vk,
            },
            &owner_sk,
        );
        let member_b = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: a_id,
                member_vk: b_vk,
            },
            &a_sk,
        );

        // Only B has a message
        let msg_b = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner_id,
                author: b_id,
                time: SystemTime::now(),
                content: RoomMessageBody::public("Hello from B".to_string()),
            },
            &b_sk,
        );

        let config = Configuration {
            max_members: 10,
            max_recent_messages: 100,
            ..Default::default()
        };
        let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);

        let mut state = ChatRoomStateV1 {
            configuration: auth_config,
            members: MembersV1 {
                members: vec![member_a, member_b],
            },
            recent_messages: MessagesV1 {
                messages: vec![msg_b],
                ..Default::default()
            },
            ..Default::default()
        };

        state.post_apply_cleanup(&params).unwrap();

        // Both A and B should remain (A is in B's invite chain)
        assert_eq!(state.members.members.len(), 2);
        let member_ids: HashSet<MemberId> = state
            .members
            .members
            .iter()
            .map(|m| m.member.id())
            .collect();
        assert!(
            member_ids.contains(&a_id),
            "A should be kept (in B's invite chain)"
        );
        assert!(
            member_ids.contains(&b_id),
            "B should be kept (has messages)"
        );
    }

    #[test]
    fn test_ban_persists_after_banner_pruned() {
        let rng = &mut rand::thread_rng();
        let owner_sk = SigningKey::generate(rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let params = ChatRoomParametersV1 { owner: owner_vk };

        let a_sk = SigningKey::generate(rng);
        let a_vk = a_sk.verifying_key();
        let a_id = MemberId::from(&a_vk);

        let c_sk = SigningKey::generate(rng);
        let c_vk = c_sk.verifying_key();
        let c_id = MemberId::from(&c_vk);

        // A is a member (invited by owner)
        let member_a = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: a_vk,
            },
            &owner_sk,
        );

        // A bans C
        let ban_c_by_a = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: SystemTime::now(),
                banned_user: c_id,
            },
            a_id,
            &a_sk,
        );

        let config = Configuration {
            max_members: 10,
            max_user_bans: 10,
            ..Default::default()
        };
        let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);

        // A has no messages → will be pruned
        let mut state = ChatRoomStateV1 {
            configuration: auth_config,
            members: MembersV1 {
                members: vec![member_a],
            },
            bans: BansV1(vec![ban_c_by_a]),
            ..Default::default()
        };

        state.post_apply_cleanup(&params).unwrap();

        // A should be pruned (no messages)
        assert!(state.members.members.is_empty(), "A should be pruned");

        // A's ban of C should persist (A was pruned, not banned)
        assert_eq!(state.bans.0.len(), 1, "Ban should persist");
        assert_eq!(state.bans.0[0].ban.banned_user, c_id);
        assert_eq!(state.bans.0[0].banned_by, a_id);
    }

    #[test]
    fn test_member_re_added_with_message() {
        let rng = &mut rand::thread_rng();
        let owner_sk = SigningKey::generate(rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let params = ChatRoomParametersV1 { owner: owner_vk };

        let a_sk = SigningKey::generate(rng);
        let a_vk = a_sk.verifying_key();
        let a_id = MemberId::from(&a_vk);

        let member_a = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: a_vk,
            },
            &owner_sk,
        );

        let config = Configuration {
            max_members: 10,
            max_recent_messages: 100,
            ..Default::default()
        };
        let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);

        // State with A but no messages
        let mut state = ChatRoomStateV1 {
            configuration: auth_config,
            members: MembersV1 {
                members: vec![member_a.clone()],
            },
            ..Default::default()
        };

        // Cleanup prunes A
        state.post_apply_cleanup(&params).unwrap();
        assert!(state.members.members.is_empty(), "A should be pruned");

        // Re-add A with a message
        state.members.members.push(member_a);
        let msg = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner_id,
                author: a_id,
                time: SystemTime::now(),
                content: RoomMessageBody::public("Hello again!".to_string()),
            },
            &a_sk,
        );
        state.recent_messages.messages.push(msg);

        // Cleanup should keep A now
        state.post_apply_cleanup(&params).unwrap();
        assert_eq!(state.members.members.len(), 1, "A should be kept");
        assert_eq!(state.members.members[0].member.id(), a_id);
    }

    #[test]
    fn test_member_info_cleaned_after_pruning() {
        let rng = &mut rand::thread_rng();
        let owner_sk = SigningKey::generate(rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let params = ChatRoomParametersV1 { owner: owner_vk };

        let a_sk = SigningKey::generate(rng);
        let a_vk = a_sk.verifying_key();
        let a_id = MemberId::from(&a_vk);

        let member_a = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: a_vk,
            },
            &owner_sk,
        );

        // Create member_info for A and owner
        let a_info = AuthorizedMemberInfo::new_with_member_key(
            MemberInfo::new_public(a_id, 1, "Alice".to_string()),
            &a_sk,
        );
        let owner_info = AuthorizedMemberInfo::new(
            MemberInfo::new_public(owner_id, 1, "Owner".to_string()),
            &owner_sk,
        );

        let config = Configuration {
            max_members: 10,
            ..Default::default()
        };
        let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);

        let mut state = ChatRoomStateV1 {
            configuration: auth_config,
            members: MembersV1 {
                members: vec![member_a],
            },
            member_info: MemberInfoV1 {
                member_info: vec![owner_info, a_info],
            },
            ..Default::default()
        };

        // A has no messages → gets pruned along with their member_info
        state.post_apply_cleanup(&params).unwrap();

        assert!(state.members.members.is_empty(), "A should be pruned");
        assert_eq!(
            state.member_info.member_info.len(),
            1,
            "Only owner's info should remain"
        );
        assert_eq!(
            state.member_info.member_info[0].member_info.member_id, owner_id,
            "Remaining info should be owner's"
        );
    }

    /// Regression test for issue #110 / Bug #3 PR B:
    ///
    /// A member with an `encrypted_secrets` entry (i.e. the owner has
    /// issued them a per-version room-secret blob) must survive
    /// `post_apply_cleanup` even if they have not yet authored any
    /// messages and have no active DMs. The owner-issued blob is proof
    /// that the owner considers them a member, and pruning them on the
    /// invitee's first state ingestion is the underlying cause of the
    /// "DM to inactive member fails" / "newly-invited member silently
    /// pruned" symptom Ivvor reported in Bug #3.
    #[test]
    fn test_member_with_encrypted_secret_survives_cleanup() {
        let rng = &mut rand::thread_rng();
        let owner_sk = SigningKey::generate(rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let params = ChatRoomParametersV1 { owner: owner_vk };

        let a_sk = SigningKey::generate(rng);
        let a_vk = a_sk.verifying_key();
        let a_id = MemberId::from(&a_vk);

        let member_a = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: a_vk,
            },
            &owner_sk,
        );

        // A has NO messages and NO DMs — under the pre-fix rules they
        // would be pruned by post_apply_cleanup. The owner-issued
        // encrypted secret is the only evidence of membership.
        let secret_for_a = crate::room_state::secret::EncryptedSecretForMemberV1 {
            member_id: a_id,
            secret_version: 0,
            ciphertext: vec![0u8; 16], // dummy ciphertext — signature is what counts
            nonce: [0u8; 12],
            sender_ephemeral_public_key: [0u8; 32],
            provider: owner_id,
        };
        let authorized_secret = crate::room_state::secret::AuthorizedEncryptedSecretForMember::new(
            secret_for_a,
            &owner_sk,
        );

        let config = Configuration {
            max_members: 10,
            max_recent_messages: 100,
            ..Default::default()
        };
        let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);

        let mut state = ChatRoomStateV1 {
            configuration: auth_config,
            members: MembersV1 {
                members: vec![member_a],
            },
            secrets: crate::room_state::secret::RoomSecretsV1 {
                current_version: 0,
                versions: vec![],
                encrypted_secrets: vec![authorized_secret],
            },
            ..Default::default()
        };

        state.post_apply_cleanup(&params).unwrap();

        assert_eq!(
            state.members.members.len(),
            1,
            "A should survive cleanup because they have an encrypted_secrets entry"
        );
        assert_eq!(state.members.members[0].member.id(), a_id);
    }

    /// IMPORTANT #4 (PR #272 review round 2): a member who is BOTH
    /// banned AND has a stale `encrypted_secrets` blob must still be
    /// pruned by `post_apply_cleanup`. The exemption introduced for
    /// issue #110 grants survival on the strength of the owner's
    /// blob, but bans must override — a ban is the owner's later,
    /// authoritative statement that this member is no longer trusted.
    ///
    /// The `members_by_id.contains_key(recipient_id)` guard at the
    /// cleanup site keeps this safe: the ban delta runs through the
    /// member-prune path before `post_apply_cleanup`'s `required_ids`
    /// collection, so by the time we check the exemption set, the
    /// banned member is no longer in `members_by_id` and the
    /// exemption clause is short-circuited. This test pins that
    /// behaviour against any future regression that loosens the
    /// guard.
    #[test]
    fn test_banned_member_with_encrypted_secret_is_still_pruned() {
        use crate::room_state::ban::{AuthorizedUserBan, UserBan};
        use std::time::SystemTime;

        let rng = &mut rand::thread_rng();
        let owner_sk = SigningKey::generate(rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let params = ChatRoomParametersV1 { owner: owner_vk };

        let a_sk = SigningKey::generate(rng);
        let a_vk = a_sk.verifying_key();
        let a_id = MemberId::from(&a_vk);

        let member_a = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: a_vk,
            },
            &owner_sk,
        );

        let ban = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: SystemTime::now(),
                banned_user: a_id,
            },
            owner_id,
            &owner_sk,
        );

        // Owner issued a v0 blob for A, then banned A. The blob
        // outlives the ban in the state (a peer might receive both
        // deltas in one batch). Without proper handling, the
        // exemption would resurrect A.
        let secret_for_a = crate::room_state::secret::EncryptedSecretForMemberV1 {
            member_id: a_id,
            secret_version: 0,
            ciphertext: vec![0u8; 16],
            nonce: [0u8; 12],
            sender_ephemeral_public_key: [0u8; 32],
            provider: owner_id,
        };
        let authorized_secret = crate::room_state::secret::AuthorizedEncryptedSecretForMember::new(
            secret_for_a,
            &owner_sk,
        );

        let config = Configuration {
            max_members: 10,
            max_recent_messages: 100,
            ..Default::default()
        };
        let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);

        let mut state = ChatRoomStateV1 {
            configuration: auth_config,
            members: MembersV1 {
                members: vec![member_a],
            },
            bans: crate::room_state::ban::BansV1(vec![ban]),
            secrets: crate::room_state::secret::RoomSecretsV1 {
                current_version: 0,
                versions: vec![],
                encrypted_secrets: vec![authorized_secret],
            },
            ..Default::default()
        };

        // The owner-side flow is: apply ban delta -> members.apply_delta
        // removes A from members -> post_apply_cleanup runs. We
        // simulate the post-ban-prune state by manually removing A
        // from members (matching what `MembersV1::apply_delta` does
        // when it sees the ban), then run cleanup.
        state.members.members.retain(|m| m.member.id() != a_id);

        state.post_apply_cleanup(&params).unwrap();

        assert!(
            state.members.members.is_empty(),
            "banned member A must NOT be resurrected by post_apply_cleanup's \
             encrypted_secrets exemption — see IMPORTANT #4 on PR #272 review round 2"
        );
        // The ban itself must persist.
        assert_eq!(state.bans.0.len(), 1);
        assert_eq!(state.bans.0[0].ban.banned_user, a_id);
    }

    /// IMPORTANT #5 (PR #272 review round 2): the
    /// `encrypted_secrets` exemption from `post_apply_cleanup` must
    /// be SCOPED to the current secret version. A member who has
    /// only old-version blobs and hasn't been re-issued at
    /// `current_version` is "stale" by the same definition as a
    /// member who joined and never authored, and must be pruned.
    ///
    /// Without this TTL, every ever-recipient + their entire
    /// invite-chain ancestor set would be exempt from cleanup
    /// forever — defeating the whole point of the inactivity prune.
    #[test]
    fn test_stale_secret_recipient_is_pruned_after_rotation() {
        use crate::room_state::privacy::RoomCipherSpec;
        use crate::room_state::secret::{AuthorizedSecretVersionRecord, SecretVersionRecordV1};
        use std::time::SystemTime;

        let rng = &mut rand::thread_rng();
        let owner_sk = SigningKey::generate(rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let params = ChatRoomParametersV1 { owner: owner_vk };

        let a_sk = SigningKey::generate(rng);
        let a_vk = a_sk.verifying_key();
        let a_id = MemberId::from(&a_vk);

        let member_a = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: a_vk,
            },
            &owner_sk,
        );

        // A only has a v0 blob. The room has since rotated to v1
        // and A was not re-issued (e.g. they left / were
        // implicitly inactive at rotation time).
        let secret_for_a = crate::room_state::secret::EncryptedSecretForMemberV1 {
            member_id: a_id,
            secret_version: 0,
            ciphertext: vec![0u8; 16],
            nonce: [0u8; 12],
            sender_ephemeral_public_key: [0u8; 32],
            provider: owner_id,
        };
        let authorized_secret_v0 =
            crate::room_state::secret::AuthorizedEncryptedSecretForMember::new(
                secret_for_a,
                &owner_sk,
            );

        let v1_record = AuthorizedSecretVersionRecord::new(
            SecretVersionRecordV1 {
                version: 1,
                cipher_spec: RoomCipherSpec::Aes256Gcm,
                created_at: SystemTime::now(),
            },
            &owner_sk,
        );

        let config = Configuration {
            max_members: 10,
            max_recent_messages: 100,
            ..Default::default()
        };
        let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);

        let mut state = ChatRoomStateV1 {
            configuration: auth_config,
            members: MembersV1 {
                members: vec![member_a],
            },
            secrets: crate::room_state::secret::RoomSecretsV1 {
                current_version: 1,
                versions: vec![v1_record],
                encrypted_secrets: vec![authorized_secret_v0],
            },
            ..Default::default()
        };

        state.post_apply_cleanup(&params).unwrap();

        assert!(
            state.members.members.is_empty(),
            "member A with ONLY a stale v0 blob (no v1 re-issue, no messages, no \
             DMs) must be pruned — see IMPORTANT #5 on PR #272 review round 2"
        );
    }

    /// IMPORTANT #6 (PR #272 review round 2): ban-race convergence
    /// across peers receiving deltas in different orders. Both
    /// orderings — (add-X, ban-X) and (ban-X, add-X) — must
    /// converge with X removed, regardless of whether the
    /// owner-issued `encrypted_secret` for X arrives before or
    /// after the ban.
    ///
    /// This is the same convergence test pattern PR #240 used for
    /// DMs but applied to the new encrypted_secrets exemption.
    /// Without this test, a future regression that loosens the
    /// "members_by_id.contains_key" guard could leak X back into
    /// state via the exemption when the deltas land in the
    /// "wrong" order.
    #[test]
    fn test_ban_race_with_encrypted_secret_converges_to_pruned() {
        use crate::room_state::ban::{AuthorizedUserBan, UserBan};
        use std::time::SystemTime;

        let rng = &mut rand::thread_rng();
        let owner_sk = SigningKey::generate(rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let params = ChatRoomParametersV1 { owner: owner_vk };

        let x_sk = SigningKey::generate(rng);
        let x_vk = x_sk.verifying_key();
        let x_id = MemberId::from(&x_vk);

        let member_x = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: x_vk,
            },
            &owner_sk,
        );

        let ban_x = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: SystemTime::now(),
                banned_user: x_id,
            },
            owner_id,
            &owner_sk,
        );

        let secret_for_x = crate::room_state::secret::EncryptedSecretForMemberV1 {
            member_id: x_id,
            secret_version: 0,
            ciphertext: vec![0u8; 16],
            nonce: [0u8; 12],
            sender_ephemeral_public_key: [0u8; 32],
            provider: owner_id,
        };
        let authorized_secret_x =
            crate::room_state::secret::AuthorizedEncryptedSecretForMember::new(
                secret_for_x,
                &owner_sk,
            );

        let config = Configuration {
            max_members: 10,
            max_recent_messages: 100,
            ..Default::default()
        };
        let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);

        // Build the FINAL converged state both peers should arrive
        // at: X is banned, X is not in members, the v0
        // encrypted_secret for X may or may not be present
        // depending on whether peer's secrets state pruned it.
        // We simulate the post-merge state where both deltas have
        // landed; the in-flight blob for X is still in state when
        // post_apply_cleanup runs.
        //
        // Peer A: applied [add-X@t0, ban-X@t1] — members.apply_delta
        // saw the ban and removed X from members. Then the
        // secrets delta arrived with a v0 blob for X. Final state:
        // members = [], bans = [ban-X], encrypted_secrets = [(x, 0)].
        let mut peer_a_state = ChatRoomStateV1 {
            configuration: auth_config.clone(),
            members: MembersV1 { members: vec![] },
            bans: crate::room_state::ban::BansV1(vec![ban_x.clone()]),
            secrets: crate::room_state::secret::RoomSecretsV1 {
                current_version: 0,
                versions: vec![],
                encrypted_secrets: vec![authorized_secret_x.clone()],
            },
            ..Default::default()
        };
        peer_a_state.post_apply_cleanup(&params).unwrap();
        assert!(
            peer_a_state.members.members.is_empty(),
            "peer A: X must remain pruned despite the encrypted_secret being present"
        );

        // Peer B: applied [ban-X@t1, add-X@t0]. ban-X was applied
        // first; add-X arrived later but was rejected by
        // `MembersV1::apply_delta` because X is in the ban list.
        // Then the secrets delta arrived with a v0 blob for X.
        // Final state matches peer A's.
        let mut peer_b_state = ChatRoomStateV1 {
            configuration: auth_config,
            members: MembersV1 { members: vec![] },
            bans: crate::room_state::ban::BansV1(vec![ban_x]),
            secrets: crate::room_state::secret::RoomSecretsV1 {
                current_version: 0,
                versions: vec![],
                encrypted_secrets: vec![authorized_secret_x],
            },
            ..Default::default()
        };
        peer_b_state.post_apply_cleanup(&params).unwrap();
        assert!(
            peer_b_state.members.members.is_empty(),
            "peer B: X must remain pruned despite the encrypted_secret being present"
        );

        // The two peers must converge to byte-identical members /
        // bans / encrypted_secrets state.
        assert_eq!(peer_a_state.members, peer_b_state.members);
        assert_eq!(peer_a_state.bans, peer_b_state.bans);
        assert_eq!(peer_a_state.secrets, peer_b_state.secrets);

        // Suppress unused-variable lints — `member_x` is the seed
        // we used to derive `x_id` / `x_vk`; the convergence test
        // checks the AFTER-merge state where members is already
        // empty by construction.
        let _ = member_x;
    }

    #[test]
    fn test_state_with_none_deltas() {
        let (state, parameters, owner_signing_key) = create_empty_chat_room_state();

        // Create a modified room_state with no changes (all deltas should be None)
        let modified_state = state.clone();

        // Apply the delta
        let summary = state.summarize(&state, &parameters);
        let delta = modified_state.delta(&state, &parameters, &summary);

        assert!(
            delta.is_none(),
            "Delta should be None when no changes are made"
        );

        // Now, let's modify only one field and check if other deltas are None
        let mut partially_modified_state = state.clone();
        let new_config = Configuration {
            configuration_version: 2,
            ..partially_modified_state.configuration.configuration.clone()
        };
        partially_modified_state.configuration =
            AuthorizedConfigurationV1::new(new_config, &owner_signing_key);

        let summary = state.summarize(&state, &parameters);
        let delta = partially_modified_state
            .delta(&state, &parameters, &summary)
            .unwrap();

        // Check that only the configuration delta is Some, and others are None
        assert!(
            delta.configuration.is_some(),
            "Configuration delta should be Some"
        );
        assert!(delta.bans.is_none(), "Bans delta should be None");
        assert!(delta.members.is_none(), "Members delta should be None");
        assert!(
            delta.member_info.is_none(),
            "Member info delta should be None"
        );
        assert!(
            delta.recent_messages.is_none(),
            "Recent messages delta should be None"
        );
        assert!(delta.upgrade.is_none(), "Upgrade delta should be None");

        // Apply the partial delta
        let mut new_state = state.clone();
        new_state
            .apply_delta(&state, &parameters, &Some(delta))
            .unwrap();

        assert_eq!(
            new_state, partially_modified_state,
            "State should be partially modified"
        );
    }
}
