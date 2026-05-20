//! Validates that `common/legacy_room_contracts.toml` — the registry of every
//! previous room-contract WASM generation (freenet/river#292) — is well-formed.
//!
//! Each `code_hash` must be a 64-char hex string (a 32-byte BLAKE3 hash), all
//! hashes must be distinct, and all version labels must be distinct. A
//! malformed registry would silently break cross-generation room recovery.

use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

#[derive(Deserialize)]
struct LegacyRoomContracts {
    entry: Vec<Entry>,
}

#[derive(Deserialize)]
struct Entry {
    version: String,
    #[allow(dead_code)]
    description: String,
    #[allow(dead_code)]
    date: String,
    code_hash: String,
}

fn load_entries() -> Vec<Entry> {
    // `cargo test` runs with the crate dir (`common/`) as the working dir, but
    // tolerate being invoked from the workspace root too.
    let paths = [
        Path::new("legacy_room_contracts.toml"),
        Path::new("common/legacy_room_contracts.toml"),
    ];
    let toml_path = paths
        .iter()
        .find(|p| p.exists())
        .unwrap_or_else(|| panic!("legacy_room_contracts.toml not found in {:?}", paths));

    let content = fs::read_to_string(toml_path).unwrap();
    let parsed: LegacyRoomContracts =
        toml::from_str(&content).expect("Failed to parse legacy_room_contracts.toml");
    parsed.entry
}

#[test]
fn registry_is_non_empty() {
    assert!(
        !load_entries().is_empty(),
        "legacy_room_contracts.toml has no entries"
    );
}

#[test]
fn every_code_hash_is_a_32_byte_hex_string() {
    for entry in load_entries() {
        assert_eq!(
            entry.code_hash.len(),
            64,
            "{}: code_hash must be 64 hex chars (32 bytes), got {}",
            entry.version,
            entry.code_hash.len()
        );
        assert!(
            entry.code_hash.chars().all(|c| c.is_ascii_hexdigit()),
            "{}: code_hash contains non-hex characters",
            entry.version
        );
    }
}

#[test]
fn code_hashes_are_distinct() {
    let mut seen = HashSet::new();
    for entry in load_entries() {
        assert!(
            seen.insert(entry.code_hash.clone()),
            "{}: duplicate code_hash {}",
            entry.version,
            entry.code_hash
        );
    }
}

#[test]
fn versions_are_distinct() {
    let mut seen = HashSet::new();
    for entry in load_entries() {
        assert!(
            seen.insert(entry.version.clone()),
            "duplicate version label {}",
            entry.version
        );
    }
}
