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
//!
//! The pools started at 30x30 = 900 combinations, which users reported as
//! "samey" — a room of a few dozen people saw the same words over and over.
//! They are now 100x100 = 10,000 combinations. Keep the two pools disjoint
//! (so no handle reads "Echo Echo"), keep every word short, ASCII and
//! inoffensive, and keep them the same length as the constants below claim —
//! the tests at the bottom of this file pin all of that.

use ed25519_dalek::VerifyingKey;

/// Upper bound on the length of any handle this module can produce.
///
/// Every combination must fit a room's `max_nickname_size`, which defaults
/// to 50 bytes. `handles_fit_the_default_nickname_limit` checks the real
/// default rather than trusting this number, and
/// `handles_are_within_max_handle_len` checks this one, so a future word
/// that blows the budget fails CI instead of producing a nickname the
/// room contract rejects.
pub const MAX_HANDLE_LEN: usize = 20;

/// First half of the handle. Exactly 100 entries — see `pools_are_100`.
pub const FIRST_NAMES: [&str; 100] = [
    "Acid", "Zero", "Crash", "Cyber", "Ghost", "Neon", "Razor", "Static", "Phantom", "Chrome",
    "Glitch", "Cipher", "Daemon", "Null", "Hex", "Rogue", "Vapor", "Plasma", "Volt", "Frost",
    "Quantum", "Binary", "Proxy", "Logic", "Echo", "Pixel", "Neural", "Photon", "Onyx", "Cobalt",
    "Nitro", "Turbo", "Solar", "Lunar", "Cosmic", "Astro", "Argon", "Xenon", "Neutron", "Proton",
    "Flux", "Prism", "Laser", "Lumen", "Nova", "Pulsar", "Quasar", "Comet", "Meteor", "Orbit",
    "Zenith", "Nebula", "Aurora", "Halcyon", "Obsidian", "Titanium", "Iridium", "Mercury",
    "Silicon", "Carbon", "Crystal", "Granite", "Velvet", "Indigo", "Violet", "Crimson", "Scarlet",
    "Azure", "Magenta", "Amber", "Copper", "Silver", "Ember", "Cinder", "Blaze", "Tempest",
    "Thunder", "Monsoon", "Zephyr", "Glacier", "Tundra", "Midnight", "Twilight", "Umbra",
    "Eclipse", "Stellar", "Astral", "Lucid", "Silent", "Hollow", "Feral", "Wired", "Analog",
    "Digital", "Modular", "Fractal", "Syntax", "Bitwise", "Cache", "Packet",
];

/// Second half of the handle. Exactly 100 entries — see `pools_are_100`.
pub const LAST_NAMES: [&str; 100] = [
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
    "Runner",
    "Prowler",
    "Seeker",
    "Voyager",
    "Corsair",
    "Ronin",
    "Vagabond",
    "Wanderer",
    "Pilgrim",
    "Courier",
    "Beacon",
    "Lantern",
    "Compass",
    "Anchor",
    "Harbor",
    "Keystone",
    "Bastion",
    "Citadel",
    "Rampart",
    "Bulwark",
    "Talon",
    "Osprey",
    "Kestrel",
    "Condor",
    "Harrier",
    "Albatross",
    "Skylark",
    "Nightjar",
    "Peregrine",
    "Corvid",
    "Lynx",
    "Jackal",
    "Viper",
    "Cobra",
    "Mantis",
    "Panther",
    "Jaguar",
    "Ocelot",
    "Wolfhound",
    "Serpent",
    "Foundry",
    "Forge",
    "Anvil",
    "Crucible",
    "Furnace",
    "Bellows",
    "Piston",
    "Turbine",
    "Dynamo",
    "Flywheel",
    "Lattice",
    "Matrix",
    "Nexus",
    "Conduit",
    "Junction",
    "Gateway",
    "Bridge",
    "Tunnel",
    "Terminal",
    "Console",
    "Sigil",
    "Rune",
    "Glyph",
    "Token",
    "Ledger",
    "Archive",
    "Codex",
    "Almanac",
    "Manifest",
    "Protocol",
];

/// Fold a byte slice into a stable index seed (simple polynomial hash).
///
/// Deliberately `u64` rather than `usize`: `usize` is 32-bit on the wasm32
/// build that ships and 64-bit natively, so a `usize` accumulator would
/// give the two different index values for the same key (and the native
/// unit tests would not be testing what the browser actually produces).
fn fold(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(0u64, |acc, &b| acc.wrapping_mul(31).wrapping_add(b as u64))
}

/// Derive a deterministic default handle (e.g. "Cipher Daylight") from a
/// member's verifying key. Pure function of the key — same key in, same
/// handle out, every time.
pub fn generate_default_nickname(vk: &VerifyingKey) -> String {
    let bytes = vk.as_bytes();
    // Use disjoint key halves for the two indices so the first and last
    // word vary independently.
    let first = FIRST_NAMES[(fold(&bytes[0..16]) % FIRST_NAMES.len() as u64) as usize];
    let last = LAST_NAMES[(fold(&bytes[16..32]) % LAST_NAMES.len() as u64) as usize];
    format!("{first} {last}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use std::collections::{HashMap, HashSet};

    /// Every handle the generator can emit, for the exhaustive checks.
    fn all_handles() -> impl Iterator<Item = String> {
        FIRST_NAMES
            .iter()
            .flat_map(|first| LAST_NAMES.iter().map(move |last| format!("{first} {last}")))
    }

    #[test]
    fn pools_are_100() {
        // The doc-comments and the deterministic-distribution reasoning
        // both assume 100x100 = 10,000 combinations.
        assert_eq!(FIRST_NAMES.len(), 100);
        assert_eq!(LAST_NAMES.len(), 100);
        assert_eq!(FIRST_NAMES.len() * LAST_NAMES.len(), 10_000);
    }

    #[test]
    fn pools_have_no_duplicates() {
        for (label, pool) in [
            ("FIRST_NAMES", &FIRST_NAMES[..]),
            ("LAST_NAMES", &LAST_NAMES[..]),
        ] {
            let unique: HashSet<&&str> = pool.iter().collect();
            assert_eq!(
                unique.len(),
                pool.len(),
                "{label} contains a duplicate — it wastes a slot and skews the distribution"
            );
        }
    }

    #[test]
    fn pools_are_disjoint() {
        let first: HashSet<&&str> = FIRST_NAMES.iter().collect();
        let overlap: Vec<&&str> = LAST_NAMES.iter().filter(|w| first.contains(w)).collect();
        assert!(
            overlap.is_empty(),
            "a word in both pools can produce a doubled handle (e.g. \"Echo Echo\"): {overlap:?}"
        );
    }

    #[test]
    fn words_are_short_ascii_and_capitalized() {
        for word in FIRST_NAMES.iter().chain(LAST_NAMES.iter()) {
            assert!(
                word.chars().all(|c| c.is_ascii_alphabetic()),
                "{word} must be plain ASCII letters"
            );
            assert!(
                word.starts_with(|c: char| c.is_ascii_uppercase()),
                "{word} must be capitalized"
            );
            assert!(
                (3..=9).contains(&word.len()),
                "{word} is {} chars — keep pool words between 3 and 9 so every handle stays chat-sized",
                word.len()
            );
        }
    }

    #[test]
    fn handles_are_within_max_handle_len() {
        let longest = all_handles().max_by_key(String::len).unwrap();
        assert!(
            longest.len() <= MAX_HANDLE_LEN,
            "longest handle {longest:?} is {} bytes, over MAX_HANDLE_LEN ({MAX_HANDLE_LEN})",
            longest.len()
        );
    }

    #[test]
    fn handles_fit_the_default_nickname_limit() {
        // The room contract rejects a member_info whose nickname exceeds the
        // room's `max_nickname_size`, so a generated default that does not
        // fit would make the join / self-heal UPDATE fail outright. Check
        // against the real default rather than a hardcoded copy of it.
        let limit =
            river_core::room_state::configuration::Configuration::default().max_nickname_size;
        for handle in all_handles() {
            assert!(
                handle.len() <= limit,
                "{handle:?} is {} bytes, over the default max_nickname_size of {limit}",
                handle.len()
            );
        }
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
        // Not a guarantee (10,000 combos, birthday paradox), but a sanity
        // check that the fold actually spreads keys around. 200 keys into
        // 10,000 buckets: ~198 unique expected, so 180 leaves ample slack.
        let mut rng = rand::thread_rng();
        let mut seen = HashSet::new();
        for _ in 0..200 {
            let sk = SigningKey::generate(&mut rng);
            seen.insert(generate_default_nickname(&sk.verifying_key()));
        }
        assert!(
            seen.len() > 180,
            "fold barely spread keys: {} unique out of 200",
            seen.len()
        );
    }

    /// The point of the bigger pools is variety, which only materializes if
    /// the fold indexes evenly across the whole of each one.
    ///
    /// A reachability floor alone is NOT enough, and it is worth being precise
    /// about why: a "how many words were reached" check is a total-COLLAPSE
    /// detector only. Degenerate multipliers (2, 32, even 100 — one sharing
    /// factors with the pool size) still reach all 100 buckets over 2,000
    /// keys, and so does a synthetic distribution putting 90% of its mass on
    /// 10 words. So this test also bounds the SHARE any single word may take,
    /// which is what actually catches skew.
    #[test]
    fn fold_spreads_evenly_across_both_pools() {
        let mut rng = rand::thread_rng();
        const KEYS: usize = 2_000;
        let mut firsts: HashMap<String, usize> = HashMap::new();
        let mut lasts: HashMap<String, usize> = HashMap::new();
        for _ in 0..KEYS {
            let sk = SigningKey::generate(&mut rng);
            let name = generate_default_nickname(&sk.verifying_key());
            let (first, last) = name.split_once(' ').unwrap();
            *firsts.entry(first.to_string()).or_default() += 1;
            *lasts.entry(last.to_string()).or_default() += 1;
        }

        for (label, counts, pool) in [
            ("first", &firsts, FIRST_NAMES.len()),
            ("last", &lasts, LAST_NAMES.len()),
        ] {
            // Reachability: catches a fold collapsed onto a subset.
            assert!(
                counts.len() >= 80,
                "{label}: only {} of {pool} words reachable",
                counts.len()
            );

            // Evenness: expected share is KEYS/pool (20). Allow 4x headroom
            // for sampling noise — the observed max is ~2x — while still
            // failing the 90%-on-10-words case a reachability floor waves
            // through.
            let expected = KEYS / pool;
            let (worst_word, worst_count) = counts
                .iter()
                .max_by_key(|(_, c)| **c)
                .expect("non-empty pool");
            assert!(
                *worst_count <= expected * 4,
                "{label}: \"{worst_word}\" took {worst_count} of {KEYS} draws \
                 (expected ~{expected}) — the fold is skewed, not uniform"
            );
        }
    }
}
