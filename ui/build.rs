use chrono::Utc;
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

#[derive(Deserialize)]
struct LegacyDelegates {
    entry: Vec<LegacyEntry>,
}

#[derive(Deserialize)]
struct LegacyEntry {
    version: String,
    description: String,
    date: String,
    delegate_key: String,
    code_hash: String,
}

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

fn generate_legacy_delegates() {
    let toml_path = Path::new("..").join("legacy_delegates.toml");
    println!("cargo:rerun-if-changed={}", toml_path.display());

    let toml_content = match fs::read_to_string(&toml_path) {
        Ok(c) => c,
        Err(e) => {
            // During docs.rs or non-workspace builds, the file may not exist
            eprintln!(
                "cargo:warning=legacy_delegates.toml not found ({}), using empty list",
                e
            );
            let out_dir = env::var("OUT_DIR").unwrap();
            let dest = Path::new(&out_dir).join("legacy_delegates.rs");
            fs::write(
                dest,
                "pub const LEGACY_DELEGATES: &[([u8; 32], [u8; 32])] = &[];\n",
            )
            .unwrap();
            return;
        }
    };

    let parsed: LegacyDelegates =
        toml::from_str(&toml_content).expect("Failed to parse legacy_delegates.toml");

    let mut code = String::new();
    code.push_str(
        "// AUTO-GENERATED from legacy_delegates.toml — do not edit.\n\
         // Regenerate by touching legacy_delegates.toml and rebuilding.\n\n\
         pub const LEGACY_DELEGATES: &[([u8; 32], [u8; 32])] = &[\n",
    );

    for entry in &parsed.entry {
        let dk_bytes = hex_to_byte_array(&entry.delegate_key, &entry.version, "delegate_key");
        let ch_bytes = hex_to_byte_array(&entry.code_hash, &entry.version, "code_hash");

        code.push_str(&format!(
            "    // {}: {} ({})\n",
            entry.version, entry.description, entry.date
        ));
        code.push_str("    (\n");
        code.push_str(&format_byte_array(&dk_bytes, "        "));
        code.push_str(&format_byte_array(&ch_bytes, "        "));
        code.push_str("    ),\n");
    }

    code.push_str("];\n");

    let out_dir = env::var("OUT_DIR").unwrap();
    let dest = Path::new(&out_dir).join("legacy_delegates.rs");
    fs::write(&dest, &code).unwrap();
}

fn hex_to_byte_array(hex: &str, version: &str, field: &str) -> [u8; 32] {
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect();
    if bytes.len() != 32 {
        panic!(
            "{} {} has {} bytes, expected 32",
            version,
            field,
            bytes.len()
        );
    }
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
