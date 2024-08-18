use blake3::Hash;
use ed25519_dalek::Signature;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AuthorizedUpgrade {
    pub upgrade: Upgrade,
    pub signature: Signature,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Upgrade {
    pub version: u8,
    pub new_chatroom_address: Hash,
}
