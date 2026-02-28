//! Verifies that delegate key computation uses the correct formula.
//!
//! The chat delegate key = BLAKE3(BLAKE3(wasm_bytecode) || params).
//! A previous bug (2026-02-28) used SHA256 instead of BLAKE3 for the code hash,
//! producing wrong legacy migration keys and breaking delegate migration.
//!
//! This test ensures our key computation formula matches freenet-stdlib.

use freenet_stdlib::prelude::*;

/// Verify that DelegateKey = BLAKE3(BLAKE3(wasm) || params) for empty params.
///
/// This catches the SHA256-vs-BLAKE3 bug: if someone computes a legacy delegate
/// key using SHA256 for the code hash, it won't match what freenet-stdlib produces.
#[test]
fn delegate_key_formula_matches_manual_blake3_computation() {
    let fake_wasm = b"test-delegate-wasm-bytes-for-key-verification";
    let code = DelegateCode::from(fake_wasm.to_vec());
    let params = Parameters::from(Vec::<u8>::new());
    let delegate = Delegate::from((&code, &params));
    let key = delegate.key();

    // Manually compute: DelegateKey = BLAKE3(BLAKE3(wasm) || empty_params)
    let code_hash: [u8; 32] = *blake3::hash(fake_wasm).as_bytes();
    let expected_key: [u8; 32] = *blake3::hash(&code_hash).as_bytes();

    assert_eq!(
        key.bytes(),
        &expected_key,
        "DelegateKey must equal BLAKE3(BLAKE3(wasm)) for empty params.\n\
         If this fails, the key computation formula has changed in freenet-stdlib."
    );
}
