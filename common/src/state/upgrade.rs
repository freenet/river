use blake3::Hash;
use crate::state::member::{DebugSignature};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AuthorizedUpgrade {
    pub upgrade: Upgrade,
    pub signature: DebugSignature,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Upgrade {
    pub version: u8,
    pub new_chatroom_address: Hash,
}
