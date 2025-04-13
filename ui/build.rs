use chrono::Utc;

fn main() {
    // Get the current UTC date and time
    let now = Utc::now();
    // Use ISO 8601 format (UTC) e.g., "2023-10-27T10:30:00Z"
    // This is easily parseable by JavaScript's Date object.
    let build_timestamp_iso = now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    // Set the BUILD_TIMESTAMP_ISO environment variable for the main crate compilation
    println!("cargo:rustc-env=BUILD_TIMESTAMP_ISO={}", build_timestamp_iso);

    // Re-run this script only if build.rs changes
    println!("cargo:rerun-if-changed=build.rs");
}
