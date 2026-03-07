//! Validates that legacy_delegates.toml entries are well-formed.
//!
//! For entries V3+, verifies that delegate_key = BLAKE3(code_hash_bytes).
//! V1 and V2 predate the BLAKE3 fix and may use a different derivation.

use std::fs;
use std::path::Path;

fn hex_to_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

struct Entry {
    version: String,
    delegate_key: String,
    code_hash: String,
}

fn load_entries() -> Vec<Entry> {
    // Find the TOML relative to the test binary or workspace root
    let paths = [
        Path::new("../legacy_delegates.toml"),
        Path::new("legacy_delegates.toml"),
    ];
    let toml_path = paths
        .iter()
        .find(|p| p.exists())
        .unwrap_or_else(|| panic!("legacy_delegates.toml not found in {:?}", paths));

    let content = fs::read_to_string(toml_path).unwrap();
    let mut entries = Vec::new();

    let mut version = String::new();
    let mut delegate_key = String::new();
    let mut code_hash;

    for line in content.lines() {
        let line = line.trim();
        if line.starts_with("version") {
            version = line.split('"').nth(1).unwrap_or_default().to_string();
        } else if line.starts_with("delegate_key") {
            delegate_key = line.split('"').nth(1).unwrap_or_default().to_string();
        } else if line.starts_with("code_hash") {
            code_hash = line.split('"').nth(1).unwrap_or_default().to_string();
            entries.push(Entry {
                version: version.clone(),
                delegate_key: delegate_key.clone(),
                code_hash: code_hash.clone(),
            });
        }
    }

    entries
}

#[test]
fn all_entries_have_valid_hex() {
    let entries = load_entries();
    assert!(
        !entries.is_empty(),
        "No entries found in legacy_delegates.toml"
    );

    for entry in &entries {
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
