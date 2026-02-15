pub mod ban;
pub mod configuration;
pub mod content;
pub mod member;
pub mod member_info;
pub mod message;
pub mod privacy;
pub mod secret;
pub mod upgrade;
pub mod version;

use crate::room_state::ban::BansV1;
use crate::room_state::configuration::AuthorizedConfigurationV1;
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
    /// member_info for pruned members, and remove orphaned bans.
    ///
    /// Members are only kept if they have at least one message in recent_messages,
    /// or are in the invite chain of someone who does. The owner is never in the
    /// members list (they're implicit via parameters).
    ///
    /// Bans are only removed if the banner was themselves BANNED (orphaned ban).
    /// If the banner was merely pruned for inactivity, their bans persist.
    fn post_apply_cleanup(&mut self, parameters: &ChatRoomParametersV1) -> Result<(), String> {
        let owner_id = MemberId::from(&parameters.owner);

        // 1. Collect message author IDs
        let message_authors: HashSet<MemberId> = self
            .recent_messages
            .messages
            .iter()
            .map(|m| m.message.author)
            .collect();

        // 2. Compute required members: authors + their invite chains
        let required_ids = {
            let members_by_id = self.members.members_by_member_id();
            let mut required_ids: HashSet<MemberId> = HashSet::new();

            for author_id in &message_authors {
                if *author_id != owner_id && members_by_id.contains_key(author_id) {
                    required_ids.insert(*author_id);
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

        // 6. Re-sort for deterministic ordering
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
