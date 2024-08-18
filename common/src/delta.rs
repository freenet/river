use std::collections::HashSet;
use serde::{Deserialize, Serialize};
use crate::configuration::AuthorizedConfiguration;
use crate::member::AuthorizedMember;
use crate::upgrade::AuthorizedUpgrade;
use crate::message::AuthorizedMessage;
use crate::ban::AuthorizedUserBan;

#[derive(Serialize, Deserialize)]
#[derive(Clone)]
pub struct ChatRoomDelta {
    pub configuration: Option<AuthorizedConfiguration>,
    pub members: HashSet<AuthorizedMember>,
    pub upgrade: Option<AuthorizedUpgrade>,
    pub recent_messages: Vec<AuthorizedMessage>,
    pub ban_log: Vec<AuthorizedUserBan>,
}
