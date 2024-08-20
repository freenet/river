use crate::util::truncated_base64;
use ed25519_dalek::Signature;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct AuthorizedConfiguration {
    pub configuration: Configuration,
    pub signature: Signature,
}

impl Default for AuthorizedConfiguration {
    fn default() -> Self {
        AuthorizedConfiguration {
            configuration: Configuration::default(),
            signature: Signature::from_bytes(&[0; 64]),
        }
    }
}

impl Default for Configuration {
    fn default() -> Self {
        Configuration {
            configuration_version: 1,
            name: "Default Room".to_string(),
            max_recent_messages: 100,
            max_user_bans: 10,
            max_message_size: 1000,
        }
    }
}

impl fmt::Debug for AuthorizedConfiguration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizedConfiguration")
            .field("configuration", &self.configuration)
            .field("signature", &format_args!("{}", truncated_base64(self.signature.to_bytes())))
            .finish()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Configuration {
    pub configuration_version: u32,
    pub name: String,
    pub max_recent_messages: u32,
    pub max_user_bans: u32,
    pub max_message_size: usize,
}
