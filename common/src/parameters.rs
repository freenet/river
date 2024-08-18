use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};
use crate::state::MemberId;
use crate::util::fast_hash;

#[derive(Serialize, Deserialize)]
pub struct ChatRoomParameters {
    pub owner: VerifyingKey,
}

impl ChatRoomParameters {
    pub fn owner_member_id(&self) -> MemberId {
        MemberId(fast_hash(&self.owner.to_bytes()))
    }
}