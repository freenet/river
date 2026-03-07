//! Validates that legacy_delegates.toml entries are well-formed.
//!
//! For entries V3+, verifies that delegate_key = BLAKE3(code_hash_bytes).
//! V1 and V2 predate the BLAKE3 fix and may use a different derivation.

use serde::Deserialize;
use std::fs;
use std::path::Path;

#[derive(Deserialize)]
struct LegacyDelegates {
    entry: Vec<Entry>,
}

#[derive(Deserialize)]
struct Entry {
    version: String,
    #[allow(dead_code)]
    description: String,
    #[allow(dead_code)]
    date: String,
    delegate_key: String,
    code_hash: String,
}

fn hex_to_bytes(hex: &str) -> Vec<u8> {
    assert!(
        hex.len() % 2 == 0,
        "Hex string has odd length: {}",
        hex.len()
    );
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

fn load_entries() -> Vec<Entry> {
    let paths = [
        Path::new("../legacy_delegates.toml"),
        Path::new("legacy_delegates.toml"),
    ];
    let toml_path = paths
        .iter()
        .find(|p| p.exists())
        .unwrap_or_else(|| panic!("legacy_delegates.toml not found in {:?}", paths));

    let content = fs::read_to_string(toml_path).unwrap();
    let parsed: LegacyDelegates =
        toml::from_str(&content).expect("Failed to parse legacy_delegates.toml");
    parsed.entry
}

#[test]
fn all_entries_have_valid_hex() {
    let entries = load_entries();
    assert!(
        !entries.is_empty(),
        "No entries found in legacy_delegates.toml"
    );

    for entry in &entries {
        assert_eq!(
            entry.delegate_key.len(),
            64,
            "{}: delegate_key hex has {} chars, expected 64",
            entry.version,
            entry.delegate_key.len()
        );
        assert_eq!(
            entry.code_hash.len(),
            64,
            "{}: code_hash hex has {} chars, expected 64",
            entry.version,
            entry.code_hash.len()
        );

        let dk = hex_to_bytes(&entry.delegate_key);
        let ch = hex_to_bytes(&entry.code_hash);
        assert_eq!(
            dk.len(),
            32,
            "{}: delegate_key has {} bytes, expected 32",
            entry.version,
            dk.len()
        );
        assert_eq!(
            ch.len(),
            32,
            "{}: code_hash has {} bytes, expected 32",
            entry.version,
            ch.len()
        );
    }
}

#[test]
fn delegate_key_is_blake3_of_code_hash() {
    let entries = load_entries();

    // V1 and V2 predate the BLAKE3 derivation fix; skip them
    let verifiable: Vec<&Entry> = entries
        .iter()
        .filter(|e| !matches!(e.version.as_str(), "V1" | "V2"))
        .collect();

    assert!(!verifiable.is_empty(), "No verifiable entries (V3+) found");

    for entry in verifiable {
        let ch_bytes = hex_to_bytes(&entry.code_hash);
        let computed_dk: [u8; 32] = *blake3::hash(&ch_bytes).as_bytes();
        let expected_dk = hex_to_bytes(&entry.delegate_key);

        assert_eq!(
            computed_dk.as_slice(),
            expected_dk.as_slice(),
            "{}: delegate_key != BLAKE3(code_hash)\n  expected: {}\n  computed: {}",
            entry.version,
            entry.delegate_key,
            hex::encode(computed_dk),
        );
    }
}

#[test]
fn no_duplicate_code_hashes() {
    let entries = load_entries();
    let mut seen = std::collections::HashSet::new();
    for entry in &entries {
        assert!(
            seen.insert(&entry.code_hash),
            "Duplicate code_hash in {}: {}",
            entry.version,
            entry.code_hash
        );
    }
}

#[test]
fn no_duplicate_delegate_keys() {
    let entries = load_entries();
    let mut seen = std::collections::HashSet::new();
    for entry in &entries {
        assert!(
            seen.insert(&entry.delegate_key),
            "Duplicate delegate_key in {}: {}",
            entry.version,
            entry.delegate_key
        );
    }
}
