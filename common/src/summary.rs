use std::collections::HashSet;
use serde::{Deserialize, Serialize};
use crate::ban::BanId;
use crate::member::MemberId;
use crate::message::MessageId;

#[derive(Serialize, Deserialize, Debug)]
#[derive(PartialEq)]
pub struct ChatRoomSummary {
    pub configuration_version: u32,
    pub member_ids: HashSet<MemberId>,
    pub upgrade_version: Option<u8>,
    pub recent_message_ids: HashSet<MessageId>,
    pub ban_ids: Vec<BanId>,
}
