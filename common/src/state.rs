pub mod upgrade;
pub mod member;
pub mod message;
pub mod configuration;
pub mod ban;

use ed25519_dalek::VerifyingKey;
use configuration::AuthorizedConfiguration;
use serde::{Deserialize, Serialize};
use freenet_scaffold_macro::composable;
use crate::state::ban::Bans;
use crate::state::member::{MemberId, Members};
use crate::state::message::Messages;
use crate::state::upgrade::OptionalUpgrade;

#[composable]
#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Debug)]
pub struct ChatRoomState {
    pub configuration: AuthorizedConfiguration,
    pub members: Members,
    pub upgrade: OptionalUpgrade,
    pub recent_messages: Messages,
    pub bans : Bans,
}

#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Debug)]
pub struct ChatRoomParameters {
    pub owner: VerifyingKey,
}

impl ChatRoomParameters {
    pub fn owner_id(&self) -> MemberId {
        MemberId::new(&self.owner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::member::Member;
    use crate::state::message::Message;
    use crate::state::upgrade::Upgrade;
    use std::time::SystemTime;

    #[test]
    fn test_chat_room_state() {
        let chat_room_state = ChatRoomState::default();
        
    }
}