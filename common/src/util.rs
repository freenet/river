use base64::{engine::general_purpose, Engine as _};
use data_encoding::BASE32;
use ed25519_dalek::{Signature, SignatureError, Signer, SigningKey, Verifier, VerifyingKey};
use serde::Serialize;

pub fn sign_struct<T: Serialize>(message: T, signing_key: &SigningKey) -> Signature {
    let mut data_to_sign = Vec::new();
    ciborium::ser::into_writer(&message, &mut data_to_sign).expect("Serialization should not fail");
    signing_key.sign(&data_to_sign)
}

pub fn verify_struct<T: Serialize>(
    message: &T,
    signature: &Signature,
    verifying_key: &VerifyingKey,
) -> Result<(), SignatureError> {
    let mut data_to_sign = Vec::new();
    ciborium::ser::into_writer(message, &mut data_to_sign).expect("Serialization should not fail");
    verifying_key.verify(&data_to_sign, signature)
}

/// Serde helper: encode an [`ed25519_dalek::Signature`] as a CBOR **byte
/// string** while still decoding the legacy **array-of-integers** form.
///
/// `Signature`'s derived `Serialize` calls `serialize_tuple(64)`, ciborium maps
/// a tuple to `serialize_seq`, and every byte >= 0x18 then costs 2 bytes on the
/// wire. A signature therefore occupies ~122 CBOR bytes instead of the 66 a
/// byte string needs (2-byte header + 64 payload). The saving is exactly the
/// number of bytes in the signature that are >= 0x18, so it varies per
/// signature: ~58 B on average over uniformly-distributed signature bytes,
/// 55 B for the sample measured in freenet/river#575. Room state carries a
/// signature per member, per member-info record, per message, per ban, per
/// secret record and per direct message, so on a large room this is a
/// double-digit percentage of the whole state.
///
/// This is the same trap [`crate::room_state::content`] documents for River's
/// own `Vec<u8>` fields (fixed for `ActionContentV1::payload` in
/// freenet/river#443). The reason it survived that fix is that it arrives
/// through an *imported* type whose `Serialize` impl River does not control,
/// and River's own `SignatureBytes` newtype already does the right thing — so
/// nothing at the call site hints that these 64 bytes cost 122.
///
/// # Why this is safe, and where the boundary is
///
/// [`sign_struct`] / [`verify_struct`] serialize the INNER struct only. In
/// every `AuthorizedX { x, signature }` wrapper in River, `x` is what gets
/// signed and `signature` sits outside that representation, so re-encoding the
/// signature field cannot invalidate anything. This is the same boundary
/// freenet/river#443 stopped at.
///
/// The unsafe side of that boundary is documented on
/// [`crate::room_state::message::RoomMessageBody`]: `data` / `ciphertext` live
/// INSIDE `MessageV1`, which `verify_struct` re-serializes, so re-encoding
/// those would invalidate every existing signature. Do not extend this helper
/// to any field that is inside a signed representation.
///
/// # Why `deserialize` must accept both forms
///
/// Rooms created before this change hold their signatures in the array form,
/// and contract migration re-PUTs that existing state into the new contract.
/// Without the legacy arm, every pre-existing member, message, ban and secret
/// would fail to deserialize and the migration would strand every room. The
/// same applies to `?invitation=…` links, which are base58(CBOR) of a struct
/// containing an `AuthorizedMember`: links minted before this change must keep
/// working.
///
/// `deserialize_any` is sound here because these types are only ever handled by
/// self-describing formats — ciborium on the wire and in storage, `serde_json`
/// for CLI output and `cli/src/storage.rs`. Do NOT reuse this helper for a type
/// that may be handled by a non-self-describing format such as bincode.
/// (`serde_json` writes `serialize_bytes` as an array of numbers, so JSON
/// output is byte-identical before and after this change and round-trips
/// through the legacy `visit_seq` arm.)
///
/// Like the `payload_bytes` helper it is modelled on, `deserialize_any` is the
/// only ciborium entry point that does not transparently skip a `Header::Tag`,
/// so this is marginally stricter than the derived path it replaces. Nothing in
/// River emits CBOR tags.
pub mod sig_serde {
    use ed25519_dalek::Signature;
    use serde::de::{Error as _, SeqAccess, Visitor};
    use serde::{Deserializer, Serializer};
    use std::fmt;

    /// Length of an Ed25519 signature in bytes (R || s).
    pub const SIGNATURE_LEN: usize = 64;

    pub fn serialize<S: Serializer>(
        signature: &Signature,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&signature.to_bytes())
    }

    struct BytesOrLegacySeq;

    impl<'de> Visitor<'de> for BytesOrLegacySeq {
        type Value = Signature;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a 64-byte CBOR byte string, or a legacy 64-element array of byte values")
        }

        fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
            let bytes: [u8; SIGNATURE_LEN] = v
                .try_into()
                // A static message keeps `core::fmt` machinery out of the
                // contract WASM.
                .map_err(|_| E::custom("signature is not 64 bytes"))?;
            Ok(Signature::from_bytes(&bytes))
        }

        fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<Self::Value, E> {
            self.visit_bytes(&v)
        }

        /// Legacy form: a CBOR array of integers, one per byte.
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            // Unlike the `Vec<u8>` case in `room_state::content`, there is no
            // `size_hint`-driven allocation to bound here: the destination is a
            // fixed 64-byte array and the loop below refuses the 65th element,
            // so an attacker-declared array length cannot drive either an
            // allocation or unbounded work. This decode runs inside the room
            // contract on untrusted peer data.
            let mut out = [0u8; SIGNATURE_LEN];
            let mut len = 0usize;
            // Read as u16 purely for a clearer error message.
            // `next_element::<u8>()` would ALSO be correct — serde's u8 visitor
            // range-checks and errors above 255, it does not truncate.
            while let Some(byte) = seq.next_element::<u16>()? {
                if byte > u8::MAX as u16 {
                    return Err(A::Error::custom("signature element is not a byte value"));
                }
                if len == SIGNATURE_LEN {
                    return Err(A::Error::custom("signature is longer than 64 bytes"));
                }
                out[len] = byte as u8;
                len += 1;
            }
            if len != SIGNATURE_LEN {
                return Err(A::Error::custom("signature is shorter than 64 bytes"));
            }
            Ok(Signature::from_bytes(&out))
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Signature, D::Error> {
        deserializer.deserialize_any(BytesOrLegacySeq)
    }
}

pub fn truncated_base64<T: AsRef<[u8]>>(data: T) -> String {
    let encoded = general_purpose::STANDARD_NO_PAD.encode(data);
    encoded.chars().take(10).collect()
}

pub fn truncated_base32(bytes: &[u8]) -> String {
    let encoded = BASE32.encode(bytes);
    encoded.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

    #[test]
    fn test_sign_verify_struct() {
        let mut csprng = OsRng;
        let signing_key = SigningKey::generate(&mut csprng);
        let verifying_key = signing_key.verifying_key();

        let message = "Hello, World!";
        let signature = sign_struct(message, &signing_key);
        assert!(verify_struct(&message, &signature, &verifying_key).is_ok());
    }
}
