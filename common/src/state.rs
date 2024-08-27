pub mod upgrade;
pub mod member;
pub mod message;
pub mod configuration;
pub mod ban;

use ed25519_dalek::VerifyingKey;
use crate::state::member::{Members};
use configuration::AuthorizedConfiguration;
use serde::{Deserialize, Serialize};
use freenet_scaffold_macro::composable;
use crate::state::ban::Bans;
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

impl Eq for ChatRoomState {}

fn tst() {
    let state = ChatRoomState::default();
}