//! Regression test for <https://github.com/freenet/river/issues/114>
//!
//! When the Freenet node sends an `ErrorKind::EmptyRing` (variant index 12) or
//! `ErrorKind::PeerNotJoined` (variant index 13), the UI WASM must be compiled
//! against a freenet-stdlib version that knows about these variants. Otherwise
//! deserialization fails with:
//!   "invalid value: integer `12`, expected variant index 0 <= i < 12"
//!
//! This test ensures our dependency on freenet-stdlib includes these variants
//! and that they survive a bincode round-trip — the exact serialization format
//! used by the WebSocket client in `freenet-stdlib/src/client_api/browser.rs`.

use freenet_stdlib::client_api::{ClientError, ErrorKind};

/// Verify that `ErrorKind::EmptyRing` and `ErrorKind::PeerNotJoined` exist and
/// can round-trip through bincode, which is the serialization format the Freenet
/// node uses to send `HostResult` responses over the WebSocket to the UI WASM.
#[test]
fn errorkind_empty_ring_bincode_round_trip() {
    let original: ClientError = ErrorKind::EmptyRing.into();
    let bytes = bincode::serialize(&original).expect("serialize EmptyRing");
    let decoded: ClientError =
        bincode::deserialize(&bytes).expect("deserialize EmptyRing");
    assert!(
        decoded.to_string().contains("ring"),
        "EmptyRing should mention 'ring', got: {decoded}",
    );
}

#[test]
fn errorkind_peer_not_joined_bincode_round_trip() {
    let original: ClientError = ErrorKind::PeerNotJoined.into();
    let bytes = bincode::serialize(&original).expect("serialize PeerNotJoined");
    let decoded: ClientError =
        bincode::deserialize(&bytes).expect("deserialize PeerNotJoined");
    assert!(
        decoded.to_string().contains("joined"),
        "PeerNotJoined should mention 'joined', got: {decoded}",
    );
}

/// Verify that a `Result::Err(ClientError)` containing the new variants can
/// round-trip through bincode — this mirrors the `HostResult` type alias that
/// the browser WebSocket handler deserializes.
#[test]
fn host_result_err_bincode_round_trip() {
    // HostResult = Result<HostResponse, ClientError>, but HostResponse requires
    // the "net" feature. We test the Err path directly since that's where the
    // deserialization crash occurred.
    let result: Result<(), ClientError> = Err(ErrorKind::EmptyRing.into());
    let bytes = bincode::serialize(&result).expect("serialize Err(EmptyRing)");
    let decoded: Result<(), ClientError> =
        bincode::deserialize(&bytes).expect("deserialize Err(EmptyRing)");
    assert!(decoded.is_err());
    assert!(
        decoded.unwrap_err().to_string().contains("ring"),
        "round-tripped error should preserve EmptyRing message",
    );
}
