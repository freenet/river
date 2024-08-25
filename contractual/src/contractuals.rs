use ed25519_dalek::Signature;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone)]
pub struct Signed<T : Serialize + Deserialize> {
    pub message: T,
    pub signature: Signature,
}
