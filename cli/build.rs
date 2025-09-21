use std::env;
use std::fs;
use std::path::Path;

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
