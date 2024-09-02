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

#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Debug)]
pub struct ChatRoomParametersV1 {
    pub owner: VerifyingKey,
}

impl ChatRoomParametersV1 {
    pub fn owner_id(&self) -> MemberId {
        MemberId::new(&self.owner)
    }
}
