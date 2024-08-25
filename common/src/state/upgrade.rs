use crate::util::{sign_struct, truncated_base64, verify_struct};
use blake3::Hash;
use ed25519_dalek::{Signature, SigningKey, VerifyingKey, Verifier, Signer};
use serde::{Deserialize, Serialize};
use std::fmt;
use crate::state::member::MemberId;

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct AuthorizedUpgrade {
    pub upgrade: Upgrade,
    pub signature: Signature,
}

impl AuthorizedUpgrade {
    pub fn new(upgrade: Upgrade, signing_key: &SigningKey) -> Self {
        Self {
            upgrade : upgrade.clone(),
            signature : sign_struct(&upgrade, signing_key),
        }
    }

    pub fn validate(&self, verifying_key: &VerifyingKey) -> Result<(), ed25519_dalek::SignatureError> {
        verify_struct(&self.upgrade, &self.signature, &verifying_key)
    }
}

impl fmt::Debug for AuthorizedUpgrade {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizedUpgrade")
            .field("upgrade", &self.upgrade)
            .field("signature", &format_args!("{}", truncated_base64(self.signature.to_bytes())))
            .finish()
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Upgrade {
    pub owner_member_id: MemberId,
    pub version: u8,
    pub new_chatroom_address: Hash,
}
