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
//! registry. Because it ships with the package, a missing registry is a hard
//! build failure everywhere except docs.rs — a blanket
//! `allow_missing_registry(true)` would let any other missing-file case
//! silently disable the dormant-room recovery via an empty table.

use freenet_migrate_build::Component;

/// A docs.rs build — the one environment granted the empty-registry fallback.
fn docs_rs_build() -> bool {
    std::env::var_os("DOCS_RS").is_some()
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // The crate emits cargo:rerun-if-changed for the TOML itself.
    let dest = freenet_migrate_build::codegen()
        .entry_registry("legacy_room_contracts.toml", Component::Contract)
        .canonical_consts(false)
        .contract_hash_view("LEGACY_ROOM_CONTRACT_CODE_HASHES")
        .out_file("legacy_room_contracts.rs")
        .allow_missing_registry(docs_rs_build())
        .emit()
        .expect("freenet-migrate-build: generate legacy room-contract hashes");

    // Append-only registry; it can never legitimately be empty. An empty
    // const silently disables the multi-hop dormant-room recovery (#292), so
    // fail the build rather than ship it. Asserted on the GENERATED output —
    // what actually ships — not on the input file.
    if !docs_rs_build() {
        let generated = std::fs::read_to_string(&dest)
            .expect("read generated legacy_room_contracts.rs for the emptiness gate");
        let rows = generated.matches("\n    [").count();
        assert!(
            rows > 0,
            "generated LEGACY_ROOM_CONTRACT_CODE_HASHES has ZERO entries. The \
             room-contract migration table would ship EMPTY and older \
             generations of room state would silently stop being found. Check \
             legacy_room_contracts.toml — this registry is append-only and \
             must never be empty."
        );
    }
}
