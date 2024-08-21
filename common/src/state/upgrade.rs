use crate::util::truncated_base64;
use blake3::Hash;
use ed25519_dalek::{Signature, SigningKey, VerifyingKey, Verifier, Signer};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct AuthorizedUpgrade {
    pub room_fhash: i32,
    pub upgrade: Upgrade,
    pub signature: Signature,
}

impl AuthorizedUpgrade {
    pub fn new(room_fhash: i32, upgrade: Upgrade, signing_key: &SigningKey) -> Self {
        let mut data_to_sign = Vec::new();
        data_to_sign.extend_from_slice(&room_fhash.to_le_bytes());
        ciborium::ser::into_writer(&upgrade, &mut data_to_sign).expect("Serialization should not fail");
        let signature = signing_key.sign(&data_to_sign);
        
        Self {
            room_fhash,
            upgrade,
            signature,
        }
    }

    pub fn validate(&self, verifying_key: &VerifyingKey) -> Result<(), ed25519_dalek::SignatureError> {
        let mut data_to_sign = Vec::new();
        data_to_sign.extend_from_slice(&self.room_fhash.to_le_bytes());
        ciborium::ser::into_writer(&self.upgrade, &mut data_to_sign).expect("Serialization should not fail");
        verifying_key.verify(&data_to_sign, &self.signature)
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
    pub version: u8,
    pub new_chatroom_address: Hash,
}
