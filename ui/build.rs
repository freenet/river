use chrono::Utc;
use std::process::Command;

fn main() {
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

    // Note: We intentionally do NOT use cargo:rerun-if-changed here.
    // Without it, Cargo will re-run this build script on every compilation,
    // ensuring the timestamp is always fresh. This is important for development
    // to verify which version of the code is deployed.
}
