pub mod upgrade;
pub mod member;
pub mod message;
pub mod configuration;
pub mod ban;

pub mod tests;

use crate::state::member::{AuthorizedMember, MemberId, Members};
use ban::AuthorizedUserBan;
use configuration::AuthorizedConfiguration;
use message::AuthorizedMessage;
use serde::{Deserialize, Serialize};
use blake3::traits::digest::Mac;
use freenet_scaffold_macro::composable;
use crate::state::ban::Bans;
use crate::state::message::Messages;
use crate::state::upgrade::OptionalUpgrade;

#[composable]
#[derive(Serialize, Deserialize, Clone, Default, PartialEq)]
pub struct ChatRoomState {
    pub configuration: AuthorizedConfiguration,
    pub members: Members,
    pub upgrade: OptionalUpgrade,
    pub recent_messages: Messages,
    pub bans : Bans,
}

impl Eq for ChatRoomState {}

fn tst() {
    let state = ChatRoomState::default();
}