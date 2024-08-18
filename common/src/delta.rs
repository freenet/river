use crate::state::ban::AuthorizedUserBan;
use crate::state::configuration::AuthorizedConfiguration;
use crate::state::member::AuthorizedMember;
use crate::state::message::AuthorizedMessage;
use crate::state::upgrade::AuthorizedUpgrade;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatRoomDelta {
    pub configuration: Option<AuthorizedConfiguration>,
    pub members: HashSet<AuthorizedMember>,
    pub upgrade: Option<AuthorizedUpgrade>,
    pub recent_messages: Vec<AuthorizedMessage>,
    pub ban_log: Vec<AuthorizedUserBan>,
}
