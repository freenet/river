pub fn fast_hash(bytes: &[u8]) -> i32 {
    let mut hash: i32 = 0;
    for &byte in bytes {
        hash = hash.wrapping_mul(31).wrapping_add(byte as i32);
    }
    hash
}