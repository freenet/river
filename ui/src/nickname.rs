//! Default nickname generation.
//!
//! When a member joins a room without typing a nickname, River assigns one
//! automatically. The old scheme produced `User-<6 base58 chars>` (e.g.
//! `User-a3F9bX`), which reads like a spam account or an error.
//!
//! Instead we deterministically derive a 90s-hacker-culture handle (think
//! *Hackers*, *The Matrix*, *Sneakers*) from the member's verifying key:
//! one word from [`FIRST_NAMES`], one from [`LAST_NAMES`], e.g. "Cipher
//! Daylight". The derivation is a pure function of the key, so — exactly
//! like the old `User-XYZ` scheme — the same member always gets the same
//! handle, with no stored state and no RNG. That property matters: the
//! member-info self-heal path regenerates this name and must land on the
//! same value every time.

use ed25519_dalek::VerifyingKey;

/// First half of the handle. Exactly 30 entries — see `pools_are_30` test.
pub const FIRST_NAMES: [&str; 30] = [
    "Acid", "Zero", "Crash", "Cyber", "Ghost", "Neon", "Razor", "Static", "Phantom", "Chrome",
    "Glitch", "Cipher", "Daemon", "Null", "Hex", "Rogue", "Vapor", "Plasma", "Volt", "Frost",
    "Quantum", "Binary", "Proxy", "Logic", "Echo", "Pixel", "Neural", "Photon", "Onyx", "Cobalt",
];

/// Second half of the handle. Exactly 30 entries — see `pools_are_30` test.
pub const LAST_NAMES: [&str; 30] = [
    "Override",
    "Phreak",
    "Wraith",
    "Surge",
    "Spike",
    "Pulse",
    "Breaker",
    "Worm",
    "Drift",
    "Storm",
    "Specter",
    "Cascade",
    "Cortex",
    "Reaper",
    "Circuit",
    "Modem",
    "Kernel",
    "Daylight",
    "Vector",
    "Falcon",
    "Raven",
    "Socket",
    "Glider",
    "Nomad",
    "Sentinel",
    "Havoc",
    "Relay",
    "Vertex",
    "Payload",
    "Mainframe",
];

/// Fold a byte slice into a stable index seed (simple polynomial hash).
fn fold(bytes: &[u8]) -> usize {
    bytes.iter().fold(0usize, |acc, &b| {
        acc.wrapping_mul(31).wrapping_add(b as usize)
    })
}

/// Derive a deterministic default handle (e.g. "Cipher Daylight") from a
/// member's verifying key. Pure function of the key — same key in, same
/// handle out, every time.
pub fn generate_default_nickname(vk: &VerifyingKey) -> String {
    let bytes = vk.as_bytes();
    // Use disjoint key halves for the two indices so the first and last
    // word vary independently.
    let first = FIRST_NAMES[fold(&bytes[0..16]) % FIRST_NAMES.len()];
    let last = LAST_NAMES[fold(&bytes[16..32]) % LAST_NAMES.len()];
    format!("{first} {last}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    #[test]
    fn pools_are_30() {
        // The doc-comments and the deterministic-distribution reasoning
        // both assume 30x30 = 900 combinations.
        assert_eq!(FIRST_NAMES.len(), 30);
        assert_eq!(LAST_NAMES.len(), 30);
    }

    #[test]
    fn is_deterministic() {
        let mut rng = rand::thread_rng();
        let sk = SigningKey::generate(&mut rng);
        let vk = sk.verifying_key();
        // Same key must always produce the same handle — the self-heal
        // path relies on this.
        assert_eq!(
            generate_default_nickname(&vk),
            generate_default_nickname(&vk)
        );
    }

    #[test]
    fn handle_is_two_words_from_the_pools() {
        let mut rng = rand::thread_rng();
        for _ in 0..200 {
            let sk = SigningKey::generate(&mut rng);
            let name = generate_default_nickname(&sk.verifying_key());
            let (first, last) = name
                .split_once(' ')
                .expect("handle is two space-separated words");
            assert!(
                FIRST_NAMES.contains(&first),
                "unexpected first word: {first}"
            );
            assert!(LAST_NAMES.contains(&last), "unexpected last word: {last}");
        }
    }

    #[test]
    fn distinct_keys_mostly_distinct_handles() {
        // Not a guarantee (900 combos, birthday paradox), but a sanity
        // check that the fold actually spreads keys around.
        let mut rng = rand::thread_rng();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..50 {
            let sk = SigningKey::generate(&mut rng);
            seen.insert(generate_default_nickname(&sk.verifying_key()));
        }
        // 50 keys into 900 buckets: expect well over half unique.
        assert!(
            seen.len() > 30,
            "fold barely spread keys: {} unique",
            seen.len()
        );
    }
}
