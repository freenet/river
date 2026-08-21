//! Bakes this delegate's list of legitimate predecessors into the WASM.
//!
//! Source of truth is `legacy_delegates.toml` at the repo root — the SAME file
//! the UI's sweep is generated from, via the same generator, so the two cannot
//! drift. A predecessor list that disagreed with the sweep's would mean the
//! delegate refusing data the UI believes it should have.
//!
//! An empty registry is a build failure, not an empty list. River re-keys
//! regularly, so predecessors always exist; an empty list would silently make
//! this delegate refuse every inherited push, which presents as "the migration
//! did nothing" with no error anywhere.
use freenet_migrate_build::Component;

fn main() {
    let dest = freenet_migrate_build::codegen()
        .entry_registry("../../legacy_delegates.toml", Component::Delegate)
        .canonical_consts(false)
        .delegate_pair_view("PREDECESSOR_DELEGATE_PAIRS")
        .out_file("predecessors.rs")
        .rerun_if_changed(true)
        .emit()
        .expect("freenet-migrate-build: generate predecessor keys");

    let generated = std::fs::read_to_string(&dest).expect("read generated predecessors");
    assert!(
        generated.contains('['),
        "generated predecessor list is malformed"
    );
    println!("cargo:rerun-if-changed=../../legacy_delegates.toml");
}
