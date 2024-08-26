use crate::state::member::MemberId;
use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};
use freenet_scaffold::util::fast_hash;

#[derive(Serialize, Deserialize)]
pub struct ChatRoomParameters {
    pub owner: VerifyingKey,
}

impl ChatRoomParameters {
    pub fn owner_member_id(&self) -> MemberId {
        MemberId::new(&self.owner)
    }
}
