use ed25519_dalek::Signature;
use serde::{Deserialize, Serialize};

/// # Do NOT apply [`crate::util::sig_serde`] to `signature`
///
/// freenet/river#575 re-encoded every room-state signature as a CBOR byte
/// string. This one is deliberately excluded, and NOT because of the signed-
/// representation hazard — the signature covers the web interface bytes plus
/// the version number, so it is outside the signed representation just like the
/// room-state fields.
///
/// It is excluded because the reader is a *separately deployed contract*.
/// `contracts/web-container-contract` deserializes these bytes inside the
/// already-published web-container WASM, which lives at its own fixed contract
/// address and is not re-keyed by the room-contract migration. Emitting the
/// byte-string form from the publish tool would produce metadata the live
/// contract cannot parse, and every River UI publish would be rejected. Moving
/// this field requires deploying an accept-both contract first and only then
/// switching the writer.
///
/// The saving would be ~58 bytes on a single signature per UI publish, so there
/// is nothing to gain by taking that risk.
#[derive(Serialize, Deserialize)]
pub struct WebContainerMetadata {
    pub version: u32,
    pub signature: Signature, // Signature of web interface + version number
}
