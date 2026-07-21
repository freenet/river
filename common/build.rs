//! Build script for `river-core`.
//!
//! Generates `legacy_room_contracts.rs` from `legacy_room_contracts.toml`, the
//! registry of every previous room-contract WASM generation. The generated
//! `LEGACY_ROOM_CONTRACT_CODE_HASHES` const is consumed by
//! `src/migration.rs` to re-derive older-generation contract keys so a
//! long-dormant room can be recovered. See freenet/river#292.
//!
//! Codegen is delegated to `freenet-migrate-build` (freenet/river#398): it
//! parses the existing `[[entry]]` registry, validates every hash at build
//! time, and emits the same `&[[u8; 32]]` const this script used to hand-roll.
//! The TOML lives inside this crate (not at the repo root) so it is published
//! with `river-core` and riverctl built from crates.io keeps the full
//! registry; `allow_missing_registry` preserves the empty-list fallback for
//! docs.rs and other non-workspace builds.

use freenet_migrate_build::Component;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // The crate emits cargo:rerun-if-changed for the TOML itself.
    freenet_migrate_build::codegen()
        .entry_registry("legacy_room_contracts.toml", Component::Contract)
        .canonical_consts(false)
        .contract_hash_view("LEGACY_ROOM_CONTRACT_CODE_HASHES")
        .out_file("legacy_room_contracts.rs")
        .allow_missing_registry(true)
        .emit()
        .expect("freenet-migrate-build: generate legacy room-contract hashes");
}
