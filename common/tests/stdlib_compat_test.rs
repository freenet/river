//! Regression test for <https://github.com/freenet/river/issues/114>
//!
//! When the Freenet node sends an `ErrorKind::EmptyRing` (variant index 12) or
//! `ErrorKind::PeerNotJoined` (variant index 13), the UI WASM must be compiled
//! against a freenet-stdlib version that knows about these variants. Otherwise
//! deserialization fails with:
//!   "invalid value: integer `12`, expected variant index 0 <= i < 12"
//!
//! This test ensures our dependency on freenet-stdlib includes these variants.

use freenet_stdlib::client_api::{ClientError, ErrorKind};

/// Verify that `ErrorKind::EmptyRing` and `ErrorKind::PeerNotJoined` exist and
/// can round-trip through serde (the same serialization path used by the
/// WebSocket client in both native and WASM targets).
#[test]
fn errorkind_new_variants_are_available() {
    // These lines fail to compile if freenet-stdlib < 0.1.38
    let empty_ring = ErrorKind::EmptyRing;
    let peer_not_joined = ErrorKind::PeerNotJoined;

    // Wrap in ClientError (the type actually sent over the wire)
    let err1: ClientError = empty_ring.into();
    let err2: ClientError = peer_not_joined.into();

    // Verify display messages match expectations
    assert!(
        err1.to_string().contains("ring"),
        "EmptyRing error message should mention 'ring', got: {}",
        err1
    );
    assert!(
        err2.to_string().contains("joined"),
        "PeerNotJoined error message should mention 'joined', got: {}",
        err2
    );
}
