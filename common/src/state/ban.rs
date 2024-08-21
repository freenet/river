use crate::state::member::MemberId;
use crate::util::fast_hash;
use ed25519_dalek::{Signature, VerifyingKey, SigningKey, Verifier};
use serde::{Deserialize, Serialize};
use std::time::SystemTime;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AuthorizedUserBan {
    pub room_fhash: i32,
    pub ban: UserBan,
    pub banned_by: VerifyingKey,
    pub signature: Signature,
}

impl AuthorizedUserBan {
    pub fn new(room_fhash: i32, ban: UserBan, banned_by: VerifyingKey, signing_key: &SigningKey) -> Self {
        let mut data_to_sign = Vec::new();
        data_to_sign.extend_from_slice(&room_fhash.to_le_bytes());
        ciborium::ser::into_writer(&ban, &mut data_to_sign).expect("Serialization should not fail");
        data_to_sign.extend_from_slice(banned_by.as_bytes());
        let signature = signing_key.sign(&data_to_sign);
        
        Self {
            room_fhash,
            ban,
            banned_by,
            signature,
        }
    }

    pub fn validate(&self, verifying_key: &VerifyingKey) -> Result<(), ed25519_dalek::SignatureError> {
        let mut data_to_sign = Vec::new();
        data_to_sign.extend_from_slice(&self.room_fhash.to_le_bytes());
        ciborium::ser::into_writer(&self.ban, &mut data_to_sign).expect("Serialization should not fail");
        data_to_sign.extend_from_slice(self.banned_by.as_bytes());
        verifying_key.verify(&data_to_sign, &self.signature)
    }

    pub fn id(&self) -> BanId {
        BanId(fast_hash(&self.signature.to_bytes()))
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct UserBan {
    pub banned_at: SystemTime,
    pub banned_user: MemberId,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Clone, Hash, Debug)]
pub struct BanId(pub i32);
