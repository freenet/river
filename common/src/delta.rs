use std::collections::HashSet;
use serde::{Deserialize, Serialize};
use super::state::*;

#[derive(Serialize, Deserialize)]
pub struct ChatRoomDelta {
    pub configuration: Option<AuthorizedConfiguration>,
    pub members: HashSet<AuthorizedMember>,
    pub upgrade: Option<AuthorizedUpgrade>,
    pub recent_messages: Vec<AuthorizedMessage>,
    pub ban_log: Vec<AuthorizedUserBan>,
}