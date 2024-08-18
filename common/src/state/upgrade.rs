use blake3::Hash;
use blake3::Hash;
use ed25519_dalek::Signature;
use serde::{Deserialize, Serialize};
use std::fmt;
use crate::util::truncated_base64;

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct AuthorizedUpgrade {
    pub upgrade: Upgrade,
    pub signature: Signature,
}

impl fmt::Debug for AuthorizedUpgrade {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizedUpgrade")
            .field("upgrade", &self.upgrade)
            .field("signature", &format_args!("{}", truncated_base64(self.signature.to_bytes())))
            .finish()
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct Upgrade {
    pub version: u8,
    pub new_chatroom_address: Hash,
}
