use std::env;
use chrono::Utc;

fn main() {
    // Get the current UTC date and time
    let now = Utc::now();
    // Use a compact format YYMMDD-HHMM (UTC)
    let build_datetime = now.format("%y%m%d-%H%M").to_string();

    // Set the BUILD_DATETIME environment variable for the main crate compilation
    println!("cargo:rustc-env=BUILD_DATETIME={}", build_datetime);

    // Re-run this script only if build.rs changes
    println!("cargo:rerun-if-changed=build.rs");
}
