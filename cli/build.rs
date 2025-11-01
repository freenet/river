use std::env;
use std::fs;
use std::path::Path;
use std::process;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // Get the output directory
    let out_dir = env::var("OUT_DIR").unwrap();
    let dest_path = Path::new(&out_dir).join("room_contract.wasm");

    // Try to find the WASM file in several locations
    let possible_paths = [
        // When building from workspace
        "../ui/public/contracts/room_contract.wasm",
        // When building from workspace root
        "ui/public/contracts/room_contract.wasm",
        // Pre-built WASM included in the package (required for crates.io)
        // This file MUST be committed to the repo for publishing
        "contracts/room_contract.wasm",
    ];

    let mut wasm_found = false;

    for path in &possible_paths {
        if Path::new(path).exists() {
            println!("cargo:rerun-if-changed={}", path);
            fs::copy(path, &dest_path).expect("Failed to copy WASM file");
            println!("cargo:warning=Copied room_contract.wasm from {}", path);
            wasm_found = true;

            verify_matches_built_artifact(&dest_path);
            break;
        }
    }

    if !wasm_found {
        // For crates.io publishing, we need the WASM file to be included
        // Create a dummy file or panic based on whether this is a docs build
        if env::var("DOCS_RS").is_ok() {
            // During docs.rs build, create a dummy file
            fs::write(&dest_path, b"dummy").expect("Failed to create dummy WASM file");
        } else {
            panic!(
                "room_contract.wasm not found! Please ensure it exists in one of these locations: {:?}",
                possible_paths
            );
        }
    }
}

fn verify_matches_built_artifact(dest_path: &Path) {
    if std::env::var("RIVER_SKIP_CONTRACT_CHECK").is_ok() {
        return;
    }

    let expected_built_wasm = Path::new("..")
        .join("target/wasm32-unknown-unknown/release/room_contract.wasm");

    if !expected_built_wasm.exists() {
        // Nothing to compare against (contract probably not rebuilt yet)
        return;
    }

    let dest_bytes = match fs::read(dest_path) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!(
                "Failed to read copied room_contract.wasm at {}: {err}",
                dest_path.display()
            );
            process::exit(1);
        }
    };

    let built_bytes = match fs::read(&expected_built_wasm) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!(
                "Failed to read built room_contract.wasm at {}: {err}",
                expected_built_wasm.display()
            );
            process::exit(1);
        }
    };

    if dest_bytes != built_bytes {
        panic!(
            "room_contract.wasm is out of date.\n\
             The CLI is bundling {}, but the freshly built artifact at {}\n\
             differs. Run `cargo make sync-cli-wasm` to refresh the bundled WASM.",
            dest_path.display(),
            expected_built_wasm.display()
        );
    }
}
