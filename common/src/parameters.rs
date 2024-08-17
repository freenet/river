use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct ChatRoomParameters {
    pub owner: VerifyingKey,
}