pub mod upgrade;
pub mod member;
pub mod message;
pub mod configuration;
pub mod ban;

use ed25519_dalek::VerifyingKey;
use configuration::AuthorizedConfigurationV1;
use serde::{Deserialize, Serialize};
use freenet_scaffold_macro::composable;
use crate::state::ban::BansV1;
use crate::state::member::{MemberId, MembersV1};
use crate::state::message::MessagesV1;
use crate::state::upgrade::OptionalUpgradeV1;

#[composable(apply_delta_mut = true)]
#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Debug)]
pub struct ChatRoomStateV1 {
    /* Important note: Because bans determine members, and members determine
       which messages are permitted - it is essential that they appear in
       order bans, members, messages. It's also important that configuration
       come before these. TODO: Make these dependencies explicit */
    pub configuration: AuthorizedConfigurationV1,
    pub bans : BansV1,
    pub members: MembersV1,
    pub recent_messages: MessagesV1,
    pub upgrade: OptionalUpgradeV1,
}

impl ComposableState for ChatRoomStateV1 {
    type ParentState = Self;
    type Summary = Self;
    type Delta = Self;
    type Parameters = ChatRoomParametersV1;

    fn verify(&self, _parent_state: &Self::ParentState, parameters: &Self::Parameters) -> Result<(), String> {
        self.configuration.verify(self, parameters)?;
        self.bans.verify(self, parameters)?;
        self.members.verify(self, parameters)?;
        self.recent_messages.verify(self, parameters)?;
        self.upgrade.verify(self, parameters)?;
        Ok(())
    }

    fn summarize(&self, _parent_state: &Self::ParentState, _parameters: &Self::Parameters) -> Self::Summary {
        self.clone()
    }

    fn delta(&self, _parent_state: &Self::ParentState, _parameters: &Self::Parameters, old_state_summary: &Self::Summary) -> Self::Delta {
        if self != old_state_summary {
            self.clone()
        } else {
            ChatRoomStateV1::default()
        }
    }

    fn apply_delta(&mut self, _parent_state: &Self::ParentState, parameters: &Self::Parameters, delta: &Self::Delta) -> Result<(), String> {
        if delta != &ChatRoomStateV1::default() {
            *self = delta.clone();
        }
        self.verify(self, parameters)
    }
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
