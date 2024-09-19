pub mod ban;
pub mod configuration;
pub mod member;
pub mod message;
pub mod upgrade;
pub mod user_info;

use crate::state::ban::BansV1;
use crate::state::member::{MemberId, MembersV1};
use crate::state::message::MessagesV1;
use crate::state::upgrade::OptionalUpgradeV1;
use crate::state::user_info::UserInfoV1;
use configuration::AuthorizedConfigurationV1;
use ed25519_dalek::VerifyingKey;
use freenet_scaffold_macro::composable;
use serde::{Deserialize, Serialize};

#[composable]
#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Debug)]
pub struct ChatRoomStateV1 {
    /* Important note: Because bans determine members, and members determine
    which messages are permitted - it is essential that they appear in
    order bans, members, messages. It's also important that configuration
    come before these. TODO: Make these dependencies explicit */
    pub configuration: AuthorizedConfigurationV1,
    pub bans: BansV1,
    pub members: MembersV1,
    pub user_info: UserInfoV1,
    pub recent_messages: MessagesV1,
    pub upgrade: OptionalUpgradeV1,
}

#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Debug)]
pub struct ChatRoomParametersV1 {
    pub owner: VerifyingKey,
}

impl ChatRoomParametersV1 {
    pub fn owner_id(&self) -> MemberId {
        MemberId::new(&self.owner)
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Debug;
    use super::*;
    use crate::state::configuration::Configuration;
    use ed25519_dalek::SigningKey;

    #[test]
    fn test_state() {
        let (state, parameters, owner_signing_key) = create_empty_chat_room_state();

        assert!(state.verify(&state, &parameters).is_ok(), "Empty state should verify");
        
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

        let delta = modified_state.delta(&orig_state, &parameters, &orig_state.summarize(&orig_state, &parameters));
        
        println!("Delta: {:?}", delta);
        
        let mut new_state = orig_state.clone();
        let apply_delta_result = new_state.apply_delta(&orig_state, &parameters, &delta);
        assert!(
            apply_delta_result.is_ok(),
            "Applying delta failed: {:?}",
            apply_delta_result.err()
        );

        assert_eq!(new_state, modified_state);
    }    
    fn create_empty_chat_room_state() -> (ChatRoomStateV1, ChatRoomParametersV1, SigningKey) {
        // Create a test state with a single member and two messages, one written by
        // the owner and one by the member - the member must be invited by the owner
        let rng = &mut rand::thread_rng();
        let owner_signing_key = SigningKey::generate(rng);
        let owner_verifying_key = owner_signing_key.verifying_key();
        let owner_id = MemberId::new(&owner_verifying_key);

        let config = AuthorizedConfigurationV1::new(Configuration::default(), &owner_signing_key);

        (
            ChatRoomStateV1 {
                configuration: config,
                bans: BansV1::default(),
                members: MembersV1::default(),
                user_info: UserInfoV1::default(),
                recent_messages: MessagesV1::default(),
                upgrade: OptionalUpgradeV1(None),
            },
            ChatRoomParametersV1 {
                owner: owner_verifying_key,
            },
            owner_signing_key,
        )
    }
}
