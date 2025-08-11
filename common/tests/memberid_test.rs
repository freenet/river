use river_core::room_state::member::MemberId;
use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_scaffold::util::{fast_hash, FastHash};
use data_encoding::BASE32;
use std::collections::HashMap;

#[cfg(test)]
mod collision_tests {
    use super::*;
    use rand::rngs::OsRng;

    #[test]
    fn test_memberid_collision_analysis() {
        println!("\n=== MemberId Collision Analysis ===");
        
        // 1. Demonstrate how MemberId is created
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = VerifyingKey::from(&signing_key);
        let member_id: MemberId = verifying_key.into();
        
        println!("1. MemberId Creation Process:");
        println!("   VerifyingKey (32 bytes): {}", hex::encode(verifying_key.as_bytes()));
        println!("   FastHash value (i64): {}", member_id.0.0);
        println!("   Display string (8 chars): {}", member_id);
        println!("   Debug format: {:?}", member_id);
        
        // 2. Show truncation process step by step
        let hash_bytes = member_id.0.0.to_le_bytes();
        let base32_full = BASE32.encode(&hash_bytes);
        let base32_truncated = base32_full.chars().take(8).collect::<String>();
        
        println!("\n2. Truncation Process:");
        println!("   i64 as bytes: {:?}", hash_bytes);
        println!("   Full BASE32: {}", base32_full);
        println!("   Truncated (8 chars): {}", base32_truncated);
        assert_eq!(format!("{}", member_id), base32_truncated);
        
        // 3. Test for potential display collisions
        println!("\n3. Collision Testing:");
        let mut display_strings: HashMap<String, (Vec<u8>, i64)> = HashMap::new();
        let mut collision_found = false;
        
        for i in 0..10000 {
            let signing_key = SigningKey::generate(&mut OsRng);
            let verifying_key = VerifyingKey::from(&signing_key);
            let member_id: MemberId = verifying_key.into();
            let display = format!("{}", member_id);
            
            if let Some((existing_vk, existing_hash)) = display_strings.get(&display) {
                println!("   COLLISION FOUND after {} iterations!", i + 1);
                println!("     Display string: {}", display);
                println!("     Key 1: {}", hex::encode(existing_vk));
                println!("     Key 2: {}", hex::encode(verifying_key.as_bytes()));
                println!("     Hash 1: {}", existing_hash);
                println!("     Hash 2: {}", member_id.0.0);
                collision_found = true;
                break;
            }
            
            display_strings.insert(display, (verifying_key.as_bytes().to_vec(), member_id.0.0));
        }
        
        if !collision_found {
            println!("   No display collisions found in 10,000 samples");
        }
        
        // 4. Demonstrate that different FastHash values can produce same display
        println!("\n4. Display String Collision Demonstration:");
        
        // Create FastHash values that differ only in high bits (beyond what's shown)
        let base_hash = 0x123456789ABCDEF0_i64;
        let test_values = [
            base_hash,
            base_hash ^ (0xFF_i64 << 56), // Flip high byte
            base_hash ^ (0xFF00_i64 << 48), // Flip second highest byte
        ];
        
        for (i, &hash_val) in test_values.iter().enumerate() {
            let member_id = MemberId(FastHash(hash_val));
            let display = format!("{}", member_id);
            println!("   Hash {}: {} -> {}", i + 1, hash_val, display);
        }
        
        // 5. Analyze collision probability
        println!("\n5. Collision Analysis:");
        println!("   - Display uses 8 BASE32 chars = 8 * 5 = 40 bits");
        println!("   - 2^40 = 1,099,511,627,776 possible display strings");
        println!("   - Birthday paradox: ~50% collision chance with sqrt(2^40) ≈ 2^20 ≈ 1M samples");
        println!("   - With ed25519 keys (2^256 space), display collisions are inevitable at scale");
        
        println!("\n6. Security Implications:");
        println!("   - Different VerifyingKeys can have identical display strings");
        println!("   - Internal FastHash values remain different (no logical collision)");
        println!("   - UI may show multiple users with same 8-character ID");
        println!("   - Could cause user confusion in member lists/logs");
        
        // The comment in the code claims collisions require 3 * 10^59 years, but that's
        // for full VerifyingKey collisions, not display string collisions
        println!("   - Code comment about '3 * 10^59 years' refers to full key collision");
        println!("   - Display string collisions are much more likely!");
    }

    #[test]
    fn test_fasthash_properties() {
        println!("\n=== FastHash Algorithm Analysis ===");
        
        // Test the fast_hash algorithm behavior
        let test_data = [
            &[0x00][..],
            &[0xFF][..],
            &[0x00, 0x01][..],
            &[0x01, 0x00][..],
            b"hello",
            b"world",
        ];
        
        for data in test_data {
            let hash = fast_hash(data);
            println!("Data: {:?} -> Hash: {}", data, hash.0);
        }
        
        // Test hash distribution (simplified)
        let mut hash_map = HashMap::new();
        for i in 0..1000 {
            let data = [i as u8];
            let hash = fast_hash(&data);
            let bucket = (hash.0 % 10) as usize;
            *hash_map.entry(bucket).or_insert(0) += 1;
        }
        
        println!("\nHash distribution across 10 buckets (1000 samples):");
        for i in 0..10 {
            println!("  Bucket {}: {} items", i, hash_map.get(&i).unwrap_or(&0));
        }
    }
}

// Helper function for hex encoding (since hex crate might not be available)
fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        super::hex_encode(bytes)
    }
}