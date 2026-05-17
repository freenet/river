//! Direct-message **body** encoding — the structured payload that goes
//! INSIDE the ECIES ciphertext of an [`AuthorizedDirectMessage`].
//!
//! [`AuthorizedDirectMessage`]: crate::room_state::direct_messages::AuthorizedDirectMessage
//!
//! # Why this lives here
//!
//! The room contract validates the OUTER envelope of a DM (sender
//! signature, membership, caps, tombstones) but never reads the
//! plaintext — that's opaque ECIES ciphertext only the recipient can
//! decrypt. The body is therefore a pure client-↔-client concern: every
//! River UI / `riverctl` client agrees on how to encode and decode the
//! plaintext bytes, but the contract is indifferent. Putting this in
//! `river-core` keeps UI + CLI byte-identical without dragging any
//! contract WASM changes into the picture (i.e. no delegate / contract
//! migration entry is required for adding a body variant).
//!
//! # Wire format
//!
//! ```text
//!     magic_byte (0x80)           ( 1 byte)
//!     cbor(DirectMessageBody)     (variable)
//! ```
//!
//! `0x80` is a UTF-8 *continuation* byte and CANNOT appear as the first
//! byte of any valid UTF-8 string. Pre-this-format DM plaintexts were
//! always UTF-8 text (encoded via `String::into_bytes`), so any DM
//! starting with `0x80` is unambiguously a new-format body. Bodies
//! whose first byte is anything else are decoded as legacy
//! [`DirectMessageBody::Text`] via lossy UTF-8 conversion.
//!
//! See [`encode_body`] / [`decode_body`] for the canonical wire
//! transition, plus the unit tests below pinning round-trips and the
//! legacy fallback path.
//!
//! # Why CBOR and not bincode
//!
//! CBOR is already in `river-core`'s dep graph (used by `Invitation`
//! encoding in the UI), and is forwards-compatible with optional fields
//! out of the box (`#[serde(default)]`). bincode would tighten the
//! encoding a few bytes but would create a separate deserialization
//! surface to evolve.
//!
//! # Adding a new variant
//!
//! 1. Add the variant at the END of [`DirectMessageBody`] so older
//!    clients still decode the existing variants.
//! 2. Existing test coverage (`encode_decode_*`, `legacy_text_decodes_as_text`,
//!    plus your new round-trip test) pin the wire shape.
//! 3. Update both [`crate::room_state::direct_messages::compose_direct_message`]
//!    callers (UI + CLI) to produce the new variant when appropriate.

use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};

/// First byte of every new-format DM body. `0x80` is a UTF-8 continuation
/// byte and cannot appear as the leading byte of any valid UTF-8 string,
/// so a legitimate legacy text body can never collide with this magic.
pub const DM_BODY_MAGIC: u8 = 0x80;

/// Structured plaintext payload of a direct message.
///
/// Round-trips through [`encode_body`] / [`decode_body`]. Legacy
/// plaintext (any pre-this-format DM, which was raw UTF-8 bytes)
/// decodes via [`decode_body`] as [`DirectMessageBody::Text`] using a
/// lossy UTF-8 conversion — see [`decode_body`]'s documentation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DirectMessageBody {
    /// Plain user-typed text. Equivalent in wire bytes to what was
    /// previously the entire decrypted body.
    Text {
        #[serde(default)]
        text: String,
    },

    /// Structured invite handed peer-to-peer via DM. The recipient
    /// renders this as an "Invitation card" with an Accept button that
    /// re-uses the URL-bar accept-invitation handler — no full page
    /// reload required.
    ///
    /// `invitation_payload` is the CBOR-encoded
    /// `ui::components::members::Invitation` (the same bytes that get
    /// base58-encoded as the `?invitation=…` URL parameter). Encoding
    /// it as CBOR bytes here (rather than the base58 string form)
    /// saves the encode-decode round-trip on the recipient side and
    /// keeps the wire body smaller — base58 is a 1.37x expansion.
    ///
    /// `room_owner_vk` redundantly carries the target room's owner key
    /// so a client can show a "you're invited to room X" affordance
    /// without first round-tripping the payload through Invitation
    /// decode. It MUST match what the payload's invitation deserialises
    /// to; the recipient SHOULD reject mismatches as malformed.
    Invite {
        /// Owner verifying-key of the target room. Cheap to inspect
        /// without decoding `invitation_payload`.
        room_owner_vk: VerifyingKey,
        /// CBOR-encoded `Invitation` (room + invitee signing key +
        /// authorised member). Same bytes that would otherwise be
        /// base58-encoded as the `?invitation=…` URL parameter.
        invitation_payload: Vec<u8>,
        /// Optional sender-typed message rendered above the Accept
        /// button. `None` means "no extra text"; the recipient SHOULD
        /// hide the message box entirely in that case rather than
        /// rendering an empty line.
        #[serde(default)]
        personal_message: Option<String>,
    },
}

/// Encode a [`DirectMessageBody`] to wire bytes per the format described
/// in the module docs (magic byte + CBOR). The returned `Vec<u8>` is
/// what gets passed to `compose_direct_message`'s `body` parameter.
pub fn encode_body(body: &DirectMessageBody) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(64);
    out.push(DM_BODY_MAGIC);
    ciborium::ser::into_writer(body, &mut out)
        .map_err(|e| format!("encode_body: CBOR serialization failed: {}", e))?;
    Ok(out)
}

/// Decode wire bytes back into a [`DirectMessageBody`].
///
/// Decoding rules:
///
/// 1. If `bytes` is empty → returns [`DirectMessageBody::Text`] with an
///    empty string. (Defensive — a zero-length plaintext shouldn't
///    happen in practice but we'd rather surface it as an empty text
///    bubble than an error.)
/// 2. If `bytes[0] == DM_BODY_MAGIC` → strip the magic byte and CBOR-
///    decode the rest. Failures bubble up as `Err(String)` so the
///    caller can render a placeholder (matches the existing "unable to
///    decrypt" placeholder UX for inbound DMs).
/// 3. Otherwise → treat as legacy plaintext, lossy-UTF-8-convert the
///    entire byte slice, and return [`DirectMessageBody::Text`]. This
///    is the path that keeps DMs sent by pre-Invite clients still
///    rendering as text after this change ships.
pub fn decode_body(bytes: &[u8]) -> Result<DirectMessageBody, String> {
    if bytes.is_empty() {
        return Ok(DirectMessageBody::Text {
            text: String::new(),
        });
    }
    if bytes[0] == DM_BODY_MAGIC {
        let body: DirectMessageBody = ciborium::de::from_reader(&bytes[1..]).map_err(|e| {
            format!(
                "decode_body: CBOR deserialization of new-format body failed: {}",
                e
            )
        })?;
        return Ok(body);
    }
    // Legacy path: pre-this-format DMs were raw UTF-8 bytes.
    let text = String::from_utf8_lossy(bytes).into_owned();
    Ok(DirectMessageBody::Text { text })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn sample_vk() -> VerifyingKey {
        let seed = [7u8; 32];
        SigningKey::from_bytes(&seed).verifying_key()
    }

    #[test]
    fn encode_decode_text_round_trip() {
        let body = DirectMessageBody::Text {
            text: "Hello, peer".to_string(),
        };
        let bytes = encode_body(&body).expect("encode");
        let decoded = decode_body(&bytes).expect("decode");
        assert_eq!(body, decoded);
    }

    #[test]
    fn encode_decode_text_empty_round_trip() {
        let body = DirectMessageBody::Text {
            text: String::new(),
        };
        let bytes = encode_body(&body).expect("encode");
        let decoded = decode_body(&bytes).expect("decode");
        assert_eq!(body, decoded);
    }

    #[test]
    fn encode_decode_invite_round_trip() {
        let body = DirectMessageBody::Invite {
            room_owner_vk: sample_vk(),
            invitation_payload: vec![1, 2, 3, 4, 5],
            personal_message: Some("join us!".to_string()),
        };
        let bytes = encode_body(&body).expect("encode");
        let decoded = decode_body(&bytes).expect("decode");
        assert_eq!(body, decoded);
    }

    #[test]
    fn encode_decode_invite_round_trip_no_personal_message() {
        let body = DirectMessageBody::Invite {
            room_owner_vk: sample_vk(),
            invitation_payload: vec![],
            personal_message: None,
        };
        let bytes = encode_body(&body).expect("encode");
        let decoded = decode_body(&bytes).expect("decode");
        assert_eq!(body, decoded);
    }

    #[test]
    fn new_format_bytes_start_with_magic() {
        let body = DirectMessageBody::Text {
            text: "anything".to_string(),
        };
        let bytes = encode_body(&body).expect("encode");
        assert_eq!(bytes[0], DM_BODY_MAGIC);
    }

    #[test]
    fn legacy_text_decodes_as_text() {
        // Pre-this-format DMs were raw UTF-8 bytes — exercise the
        // fallback path that the deployed clients depend on.
        let legacy_bytes = b"hello from the past";
        let decoded = decode_body(legacy_bytes).expect("decode legacy");
        assert_eq!(
            decoded,
            DirectMessageBody::Text {
                text: "hello from the past".to_string()
            }
        );
    }

    #[test]
    fn legacy_multibyte_utf8_decodes_as_text() {
        // Make sure a legacy plaintext that starts with a normal UTF-8
        // leading byte (here a 2-byte sequence) doesn't accidentally
        // hit the new-format path.
        let legacy_bytes = "café".as_bytes();
        let decoded = decode_body(legacy_bytes).expect("decode legacy");
        assert_eq!(
            decoded,
            DirectMessageBody::Text {
                text: "café".to_string()
            }
        );
    }

    #[test]
    fn empty_bytes_decode_as_empty_text() {
        let decoded = decode_body(&[]).expect("decode empty");
        assert_eq!(
            decoded,
            DirectMessageBody::Text {
                text: String::new()
            }
        );
    }

    #[test]
    fn malformed_new_format_returns_err() {
        // Magic byte present but the rest is not valid CBOR for our
        // enum. The caller (UI bubble / CLI list) renders a placeholder
        // when this errors.
        let bytes = vec![DM_BODY_MAGIC, 0xff, 0xff, 0xff];
        assert!(decode_body(&bytes).is_err());
    }

    #[test]
    fn legacy_plain_text_never_collides_with_magic() {
        // Sanity-pin the cornerstone invariant: no valid UTF-8 string
        // begins with the magic continuation byte. We verify this by
        // attempting to construct each valid 1..=4-byte UTF-8 leading
        // byte form and asserting none of them equal DM_BODY_MAGIC.
        //
        // The point isn't to brute-force all valid UTF-8 leading
        // bytes — that would be 0x00-0x7F (1-byte) + 0xC2-0xDF (2-byte
        // lead) + 0xE0-0xEF (3-byte lead) + 0xF0-0xF4 (4-byte lead) —
        // it's to fail loudly if someone "fixes" DM_BODY_MAGIC to
        // some value that IS a valid UTF-8 leading byte (e.g. 0x20,
        // 0x7B), at which point a user typing a DM that begins with
        // that character would silently mis-decode.
        assert!(
            DM_BODY_MAGIC >= 0x80 && DM_BODY_MAGIC <= 0xBF,
            "DM_BODY_MAGIC must be a UTF-8 continuation byte (0x80..=0xBF) so it cannot start a valid UTF-8 string"
        );
    }
}
