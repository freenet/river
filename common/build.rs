//! Build script for `river-core`.
//!
//! Generates `legacy_room_contracts.rs` from `legacy_room_contracts.toml`, the
//! registry of every previous room-contract WASM generation. The generated
//! `LEGACY_ROOM_CONTRACT_CODE_HASHES` const is consumed by
//! `src/migration.rs` to re-derive older-generation contract keys so a
//! long-dormant room can be recovered. See freenet/river#292.
//!
//! This mirrors `ui/build.rs::generate_legacy_delegates()` for the chat
//! delegate. The TOML lives inside this crate (not at the repo root) so it is
//! published with `river-core` and riverctl built from crates.io keeps the
//! full registry.

use serde::Deserialize;
use std::env;
use std::fs;
use std::path::Path;

#[derive(Deserialize)]
struct LegacyRoomContracts {
    entry: Vec<LegacyEntry>,
}

#[derive(Deserialize)]
struct LegacyEntry {
    version: String,
    description: String,
    date: String,
    code_hash: String,
}

fn main() {
    let toml_path = Path::new("legacy_room_contracts.toml");
    println!("cargo:rerun-if-changed=legacy_room_contracts.toml");
    println!("cargo:rerun-if-changed=build.rs");

    let out_dir = env::var("OUT_DIR").unwrap();
    let dest = Path::new(&out_dir).join("legacy_room_contracts.rs");

    let toml_content = match fs::read_to_string(toml_path) {
        Ok(c) => c,
        Err(e) => {
            // During docs.rs or other non-workspace builds the file may be
            // absent; fall back to an empty registry rather than failing.
            eprintln!("cargo:warning=legacy_room_contracts.toml not found ({e}), using empty list");
            fs::write(
                &dest,
                "pub const LEGACY_ROOM_CONTRACT_CODE_HASHES: &[[u8; 32]] = &[];\n",
            )
            .unwrap();
            return;
        }
    };

    let parsed: LegacyRoomContracts =
        toml::from_str(&toml_content).expect("Failed to parse legacy_room_contracts.toml");

    let mut code = String::new();
    code.push_str(
        "// AUTO-GENERATED from legacy_room_contracts.toml — do not edit.\n\
         // Regenerate by editing legacy_room_contracts.toml and rebuilding.\n\n\
         pub const LEGACY_ROOM_CONTRACT_CODE_HASHES: &[[u8; 32]] = &[\n",
    );

    for entry in &parsed.entry {
        let ch_bytes = hex_to_byte_array(&entry.code_hash, &entry.version);
        code.push_str(&format!(
            "    // {}: {} ({})\n",
            entry.version, entry.description, entry.date
        ));
        code.push_str(&format_byte_array(&ch_bytes, "    "));
    }

    code.push_str("];\n");

    // Only write if the content changed, to avoid spurious recompilation.
    let existing = fs::read_to_string(&dest).unwrap_or_default();
    if existing != code {
        fs::write(&dest, &code).unwrap();
    }
}

fn hex_to_byte_array(hex: &str, version: &str) -> [u8; 32] {
    if hex.len() != 64 {
        panic!(
            "{version} code_hash hex string has {} chars, expected 64",
            hex.len()
        );
    }
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .unwrap_or_else(|_| panic!("{version} code_hash has invalid hex"))
        })
        .collect();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    arr
}

fn format_byte_array(arr: &[u8; 32], indent: &str) -> String {
    let mut s = format!("{indent}[\n");
    for chunk in arr.chunks(10) {
        s.push_str(&format!("{indent}    "));
        for (i, b) in chunk.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            s.push_str(&format!("{b}"));
        }
        s.push_str(",\n");
    }
    s.push_str(&format!("{indent}],\n"));
    s
}
