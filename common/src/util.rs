use base64::{Engine as _, engine::general_purpose};
use std::fmt;

pub fn fast_hash(bytes: &[u8]) -> i32 {
    let mut hash: i32 = 0;
    for &byte in bytes {
        hash = hash.wrapping_mul(31).wrapping_add(byte as i32);
    }
    hash
}
pub fn truncated_base64<T: AsRef<[u8]>>(data: T) -> String {
    let encoded = general_purpose::STANDARD_NO_PAD.encode(data);
    encoded.chars().take(10).collect()
}

pub struct DebugTruncated<T>(pub T);

impl<T: AsRef<[u8]>> fmt::Debug for DebugTruncated<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", truncated_base64(&self.0))
    }
}
