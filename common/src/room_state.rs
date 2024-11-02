pub mod ban;
pub mod configuration;
pub mod member;
pub mod member_info;
pub mod message;
pub mod upgrade;

use crate::room_state::ban::BansV1;
use crate::room_state::configuration::AuthorizedConfigurationV1;
use crate::room_state::member::{MemberId, MembersV1};
use crate::room_state::member_info::MemberInfoV1;
use crate::room_state::message::MessagesV1;
use crate::room_state::upgrade::OptionalUpgradeV1;
use ed25519_dalek::VerifyingKey;
use freenet_scaffold_macro::composable;
use serde::{Deserialize, Serialize};

#[composable]
#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Debug)]
pub struct ChatRoomStateV1 {
    // WARNING: The order of these fields is important for the purposes of the #[composable] macro.
    // `configuration` must be first, followed by `bans`, `members`, `member_info_modal`, and then `recent_messages`.
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

    /// The most recent messages in the chat room, the number is limited by the room configuration.
    pub recent_messages: MessagesV1,

    /// If this contract has been replaced by a new contract this will contain the new contract address.
    /// This can only be set by the owner.
    pub upgrade: OptionalUpgradeV1,
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
    use crate::room_state::configuration::Configuration;
    use ed25519_dalek::SigningKey;
    use std::fmt::Debug;

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
        let orig_verify_result = orig_state.verify(&orig_state, &parameters);
        assert!(
            orig_verify_result.is_ok(),
            "Original state verification failed: {:?}",
            orig_verify_result.err()
        );

        let modified_verify_result = modified_state.verify(&modified_state, &parameters);
        assert!(
            modified_verify_result.is_ok(),
            "Modified state verification failed: {:?}",
            modified_verify_result.err()
        );

        let delta = modified_state.delta(
            &orig_state,
            &parameters,
            &orig_state.summarize(&orig_state, &parameters),
        );

        println!("Delta: {:?}", delta);

        let mut new_state = orig_state.clone();
        if let Some(delta) = delta {
            let apply_delta_result = new_state.apply_delta(&orig_state, &parameters, &delta);
            assert!(
                apply_delta_result.is_ok(),
                "Applying delta failed: {:?}",
                apply_delta_result.err()
            );
        }

        assert_eq!(new_state, modified_state);
    }
    fn create_empty_chat_room_state() -> (ChatRoomStateV1, ChatRoomParametersV1, SigningKey) {
        // Create a test room_state with a single member and two messages, one written by
        // the owner and one by the member - the member must be invited by the owner
        let rng = &mut rand::thread_rng();
        let owner_signing_key = SigningKey::generate(rng);
        let owner_verifying_key = owner_signing_key.verifying_key();
        let owner_id : MemberId = owner_verifying_key.into();

        let config = AuthorizedConfigurationV1::new(Configuration::default(), &owner_signing_key);

        (
            ChatRoomStateV1 {
                configuration: config,
                bans: BansV1::default(),
                members: MembersV1::default(),
                member_info: MemberInfoV1::default(),
                recent_messages: MessagesV1::default(),
                upgrade: OptionalUpgradeV1(None),
            },
            ChatRoomParametersV1 {
                owner: owner_verifying_key,
            },
            owner_signing_key,
        )
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
        new_state.apply_delta(&state, &parameters, &delta).unwrap();

        assert_eq!(
            new_state, partially_modified_state,
            "State should be partially modified"
        );
    }
}
