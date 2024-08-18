use ed25519_dalek::Signature;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AuthorizedConfiguration {
    pub configuration: Configuration,
    pub signature: Signature,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Configuration {
    pub configuration_version: u32,
    pub name: String,
    pub max_recent_messages: u32,
    pub max_user_bans: u32,
}
