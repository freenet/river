use chrono::Utc;
use freenet_migrate_build::Component;
use std::process::Command;

fn main() {
    generate_build_info();
    generate_legacy_delegates();
}

fn generate_build_info() {
    // Get the current UTC date and time
    let now = Utc::now();
    // Use ISO 8601 format (UTC) e.g., "2023-10-27T10:30:00Z"
    // This is easily parseable by JavaScript's Date object.
    let build_timestamp_iso = now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    // Set the BUILD_TIMESTAMP_ISO environment variable for the main crate compilation
    println!(
        "cargo:rustc-env=BUILD_TIMESTAMP_ISO={}",
        build_timestamp_iso
    );

    // Get git commit hash (short)
    let git_hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=GIT_COMMIT_HASH={}", git_hash);
}

/// Generate the `LEGACY_DELEGATES` const from `../legacy_delegates.toml` via
/// `freenet-migrate-build` (freenet/river#398): same `&[([u8; 32], [u8; 32])]`
/// shape and (delegate_key, code_hash) order this script used to hand-roll,
/// with every hash — and each row's `delegate_key` derivation — validated at
/// build time.
///
/// * `.rerun_if_changed(false)`: we intentionally emit NO `cargo:rerun-if-changed`
///   anywhere in this script — Cargo's default re-run heuristic is what keeps
///   `BUILD_TIMESTAMP_ISO` fresh, and printing ANY such directive would disable
///   it. We always regenerate; the crate only rewrites the file when content
///   changed, so no spurious recompilation.
/// * `.allow_missing_registry(true)`: the TOML lives at the repo root (outside
///   this crate), so docs.rs / non-workspace builds fall back to an empty list.
fn generate_legacy_delegates() {
    freenet_migrate_build::codegen()
        .entry_registry("../legacy_delegates.toml", Component::Delegate)
        .canonical_consts(false)
        .delegate_pair_view("LEGACY_DELEGATES")
        .out_file("legacy_delegates.rs")
        .rerun_if_changed(false)
        .allow_missing_registry(true)
        .emit()
        .expect("freenet-migrate-build: generate legacy delegates");
}
