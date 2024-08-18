use crate::state::ban::BanId;
use crate::state::member::MemberId;
use crate::state::message::MessageId;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Serialize, Deserialize, Debug)]
#[derive(PartialEq)]
pub struct ChatRoomSummary {
    pub configuration_version: u32,
    pub member_ids: HashSet<MemberId>,
    pub upgrade_version: Option<u8>,
    pub recent_message_ids: HashSet<MessageId>,
    pub ban_ids: Vec<BanId>,
}
