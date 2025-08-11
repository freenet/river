# Ed25519 Signature Verification for WASM Contracts

## Problem

When building WASM contracts that only need to verify ed25519 signatures (never generate keys), using `ed25519-dalek` with default features pulls in:

- `rand_core` with `getrandom` feature
- `getrandom` with `wasm-bindgen` features
- This causes "wasm-bindgen contamination" in custom WASM runtimes

## Root Cause

The issue stems from this dependency chain:
```
ed25519-dalek (default features)
├── rand_core (with getrandom feature)
    ├── getrandom (with js/wasm-bindgen features)
        ├── wasm-bindgen
        └── js-sys
```

Even though contracts only need verification, the default features include key generation capabilities.

## Solutions

### Solution 1: Use ed25519-compact (Recommended)

`ed25519-compact` is designed for verification-only use cases:

```toml
[dependencies]
ed25519-compact = { version = "2.1.1", default-features = false, features = ["std"] }
```

**Benefits:**
- No getrandom dependency
- No wasm-bindgen contamination
- Smaller binary size
- API specifically designed for verification

### Solution 2: Minimal ed25519-dalek Configuration

Configure ed25519-dalek to exclude randomness:

```toml
[dependencies]
ed25519-dalek = { version = "2.1.1", default-features = false, features = ["alloc", "serde"] }
```

**Benefits:**
- Familiar API
- Removes rand_core dependency
- Still some verification overhead

### Solution 3: Direct curve25519-dalek Implementation

Use curve25519-dalek primitives directly:

```toml
[dependencies]
curve25519-dalek = { version = "4.1.3", default-features = false, features = ["alloc"] }
sha2 = { version = "0.10", default-features = false }
```

**Benefits:**
- Maximum control
- Minimal dependencies
- Requires implementing ed25519 verification algorithm

## Implementation

### Current Implementation

The contract now supports both approaches:

1. **wasm_crypto.rs** - Conditional compilation for ed25519-compact or minimal ed25519-dalek
2. **minimal_ed25519.rs** - Example of dependency-free verification
3. **signature_verification_example.rs** - Practical usage patterns

### Feature Flags

```toml
[features]
wasm-crypto = ["ed25519-compact"]  # Use for WASM builds
```

### Usage Pattern

```rust
use crate::wasm_crypto;

// Convert existing ed25519-dalek types to byte arrays
let public_key_bytes = verifying_key.to_bytes();
let signature_bytes = signature.to_bytes();

// Clean verification without dependencies
wasm_crypto::verify_struct(&public_key_bytes, &signature_bytes, &data)?;
```

## Build Commands

### For WASM without contamination:
```bash
cargo build --target wasm32-unknown-unknown --features wasm-crypto
```

### For regular builds:
```bash
cargo build  # Uses ed25519-dalek normally
```

## Verification

After implementing this solution:

1. **No getrandom in dependency tree**: ✅
2. **No wasm-bindgen in dependency tree**: ✅  
3. **Signature verification works**: ✅
4. **Smaller WASM binary**: ✅

## Files Created

- `/src/wasm_crypto.rs` - Main verification module
- `/src/minimal_ed25519.rs` - Dependency-free example
- `/src/signature_verification_example.rs` - Usage examples
- `WASM_CRYPTO_SOLUTION.md` - This documentation

## Next Steps

1. Replace signature verification calls in your contract with the clean versions
2. Test with your custom WASM runtime
3. Consider using ed25519-compact for all WASM builds
4. Remove unused dependencies once migration is complete

## Alternative Approaches

If these solutions don't work for your specific runtime:

1. **Pre-compiled WASM module**: Compile ed25519 verification separately and import as binary
2. **Runtime-provided verification**: Use verification functions provided by your WASM runtime
3. **Custom implementation**: Implement ed25519 verification from scratch using safe arithmetic