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

#[composable]
#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Debug)]
pub struct ChatRoomStateV1 {
    pub configuration: AuthorizedConfigurationV1,
    pub members: MembersV1,
    pub upgrade: OptionalUpgradeV1,
    pub recent_messages: MessagesV1,
    pub bans : BansV1,
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
