use serde::{Deserialize, Serialize};

pub fn fast_hash(bytes: &[u8]) -> FastHash {
    let mut hash: i64 = 0;
    for &byte in bytes {
        hash = hash.wrapping_mul(31).wrapping_add(byte as i64);
    }
    FastHash(hash)
}

#[derive(Serialize, Deserialize, Eq, PartialEq, Hash, Clone, Debug, Ord, PartialOrd, Copy)]
pub struct FastHash(pub i64);
