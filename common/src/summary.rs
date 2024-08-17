use std::collections::HashSet;
use serde::{Deserialize, Serialize};
use crate::state::{BanId, MemberId, MessageId};

#[derive(Serialize, Deserialize)]
pub struct ChatRoomSummary {
    pub configuration_version: u32,
    pub member_ids: HashSet<MemberId>,
    pub upgrade_version: Option<u8>,
    pub recent_message_ids: HashSet<MessageId>,
    pub ban_ids: Vec<BanId>,
}