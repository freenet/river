//! Wire encoding for v2 records: fixed-width byte newtypes that serialize as
//! CBOR byte strings, a macro that encodes a named struct as a CBOR array of
//! its fields (no field names on the wire), and the signing bytes every v2
//! payload is signed over.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// Domain prefix of every v2 signature, so no v2 signature verifies as a v1
/// record or as another protocol's message.
pub const SIGNING_DOMAIN: &[u8] = b"freenet-river-room-v2";

macro_rules! fixed_bytes {
    ($(#[$m:meta])* $name:ident, $n:expr) => {
        $(#[$m])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub [u8; $n]);

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_bytes(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let v = ByteBuf::deserialize(d)?;
                let arr: [u8; $n] = v.0.as_slice().try_into().map_err(|_| {
                    serde::de::Error::custom(format!(
                        "expected {} bytes, got {}",
                        $n,
                        v.0.len()
                    ))
                })?;
                Ok($name(arr))
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                for b in &self.0[..4] {
                    write!(f, "{b:02x}")?;
                }
                Ok(())
            }
        }
    };
}

fixed_bytes!(
    /// A 32-byte value: an ed25519 verifying key or a BLAKE3-256 digest.
    Bytes32,
    32
);
fixed_bytes!(
    /// An ed25519 signature.
    Sig,
    64
);
fixed_bytes!(
    /// A 128-bit token (a DM purge token: the first 16 bytes of BLAKE3 of the
    /// DM's signature, as in v1).
    Token16,
    16
);
fixed_bytes!(
    /// An AEAD nonce.
    Nonce12,
    12
);

/// A verifying key, as its 32 bytes. Parsed (and so checked to be a valid
/// point) when a signature is checked against it.
pub type Key = Bytes32;
/// A BLAKE3-256 digest of a record's canonical bytes.
pub type Digest = Bytes32;

impl From<&VerifyingKey> for Bytes32 {
    fn from(k: &VerifyingKey) -> Self {
        Bytes32(k.to_bytes())
    }
}

impl From<&Signature> for Sig {
    fn from(s: &Signature) -> Self {
        Sig(s.to_bytes())
    }
}

/// A byte vector that serializes as one CBOR byte string.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ByteBuf(pub Vec<u8>);

impl Serialize for ByteBuf {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for ByteBuf {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = ByteBuf;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a byte string")
            }
            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<ByteBuf, E> {
                Ok(ByteBuf(v.to_vec()))
            }
            fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<ByteBuf, E> {
                Ok(ByteBuf(v))
            }
        }
        d.deserialize_byte_buf(V)
    }
}

impl fmt::Debug for ByteBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} bytes", self.0.len())
    }
}

/// Declares a struct that encodes as a CBOR array of its fields in
/// declaration order. Field names never reach the wire, so a record pays only
/// for its values.
macro_rules! compact_struct {
    ($(#[$m:meta])* pub struct $name:ident { $($(#[$fm:meta])* pub $f:ident : $t:ty),* $(,)? }) => {
        $(#[$m])*
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub struct $name { $($(#[$fm])* pub $f: $t),* }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                use serde::ser::SerializeTuple;
                let n = [$(stringify!($f)),*].len();
                let mut t = s.serialize_tuple(n)?;
                $(t.serialize_element(&self.$f)?;)*
                t.end()
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct V;
                impl<'de> serde::de::Visitor<'de> for V {
                    type Value = $name;
                    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                        f.write_str(stringify!($name))
                    }
                    #[allow(unused_assignments, unused_mut, unused_variables)]
                    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut a: A) -> Result<$name, A::Error> {
                        let mut i = 0usize;
                        $(
                            let $f: $t = a
                                .next_element()?
                                .ok_or_else(|| serde::de::Error::invalid_length(i, &self))?;
                            i += 1;
                        )*
                        if a.next_element::<serde::de::IgnoredAny>()?.is_some() {
                            return Err(serde::de::Error::custom("trailing fields"));
                        }
                        Ok($name { $($f),* })
                    }
                }
                let n = [$(stringify!($f)),*].len();
                d.deserialize_tuple(n, V)
            }
        }
    };
}
pub(crate) use compact_struct;

/// Declares an enum whose variants each carry one value, encoded as the
/// two-element CBOR array `[tag, value]`.
macro_rules! compact_enum {
    ($(#[$m:meta])* pub enum $name:ident { $($(#[$vm:meta])* $tag:literal => $v:ident($t:ty)),* $(,)? }) => {
        $(#[$m])*
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub enum $name { $($(#[$vm])* $v($t)),* }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                use serde::ser::SerializeTuple;
                let mut t = s.serialize_tuple(2)?;
                match self {
                    $($name::$v(x) => { t.serialize_element(&($tag as u8))?; t.serialize_element(x)?; })*
                }
                t.end()
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct V;
                impl<'de> serde::de::Visitor<'de> for V {
                    type Value = $name;
                    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                        f.write_str(stringify!($name))
                    }
                    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut a: A) -> Result<$name, A::Error> {
                        let tag: u8 = a
                            .next_element()?
                            .ok_or_else(|| serde::de::Error::invalid_length(0, &self))?;
                        let v = match tag {
                            $($tag => $name::$v(a
                                .next_element()?
                                .ok_or_else(|| serde::de::Error::invalid_length(1, &self))?),)*
                            t => return Err(serde::de::Error::custom(format!("unknown tag {t}"))),
                        };
                        if a.next_element::<serde::de::IgnoredAny>()?.is_some() {
                            return Err(serde::de::Error::custom("trailing fields"));
                        }
                        Ok(v)
                    }
                }
                d.deserialize_tuple(2, V)
            }
        }
    };
}
pub(crate) use compact_enum;

/// CBOR bytes of a value.
pub fn to_cbor<T: Serialize + ?Sized>(v: &T) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(v, &mut out).expect("serializing to a Vec does not fail");
    out
}

/// Decodes `bytes` as a `T` whose re-encoding is exactly `bytes`; any other
/// encoding of the same value (indefinite lengths, non-minimal integers,
/// trailing bytes) is refused, so one value has one wire form.
pub fn from_cbor_canonical<T: Serialize + for<'de> Deserialize<'de>>(bytes: &[u8]) -> Option<T> {
    let v: T = ciborium::de::from_reader(bytes).ok()?;
    (to_cbor(&v) == bytes).then_some(v)
}

/// BLAKE3-256 of a value's CBOR bytes.
pub fn digest_of<T: Serialize + ?Sized>(v: &T) -> Digest {
    Bytes32(*blake3::hash(&to_cbor(v)).as_bytes())
}

/// The bytes a v2 payload is signed over: the domain, the record kind's
/// signing tag, the room owner's key, then the payload's CBOR. Binding the
/// owner key here means a record signed for one room never verifies in
/// another, without storing the owner key in every record.
pub fn signing_bytes<T: Serialize + ?Sized>(tag: u8, owner: &Key, payload: &T) -> Vec<u8> {
    let mut out = Vec::with_capacity(SIGNING_DOMAIN.len() + 33 + 64);
    out.extend_from_slice(SIGNING_DOMAIN);
    out.push(tag);
    out.extend_from_slice(&owner.0);
    ciborium::ser::into_writer(payload, &mut out).expect("serializing to a Vec does not fail");
    out
}

/// Signs a payload as `signing_bytes` lays it out.
pub fn sign_payload<T: Serialize + ?Sized>(
    tag: u8,
    owner: &Key,
    payload: &T,
    key: &SigningKey,
) -> Sig {
    Sig::from(&key.sign(&signing_bytes(tag, owner, payload)))
}

/// `verify_strict` of `sig` over `msg` under `key`; false for a key that is
/// not a valid point.
pub fn verify_bytes(key: &Key, msg: &[u8], sig: &Sig) -> bool {
    match VerifyingKey::from_bytes(&key.0) {
        Ok(vk) => vk
            .verify_strict(msg, &Signature::from_bytes(&sig.0))
            .is_ok(),
        Err(_) => false,
    }
}

/// `verify_strict` of a v2 payload signature.
pub fn verify_payload<T: Serialize + ?Sized>(
    tag: u8,
    owner: &Key,
    payload: &T,
    key: &Key,
    sig: &Sig,
) -> bool {
    verify_bytes(key, &signing_bytes(tag, owner, payload), sig)
}

/// The v1 64-bit member id of a key (`MemberId(fast_hash(key bytes))`).
pub fn legacy_id(key: &Key) -> i64 {
    freenet_scaffold::util::fast_hash(&key.0).0
}

#[cfg(test)]
mod tests {
    use super::*;

    compact_struct! {
        pub struct Probe { pub a: u32, pub b: ByteBuf }
    }

    #[test]
    fn compact_struct_encodes_as_an_array_without_names() {
        let p = Probe {
            a: 7,
            b: ByteBuf(vec![1, 2, 3]),
        };
        // [7, h'010203']
        assert_eq!(to_cbor(&p), vec![0x82, 0x07, 0x43, 1, 2, 3]);
        assert_eq!(from_cbor_canonical::<Probe>(&to_cbor(&p)), Some(p));
    }

    #[test]
    fn non_canonical_encodings_are_refused() {
        // [7, h'010203'] with the 7 as a two-byte integer.
        assert_eq!(
            from_cbor_canonical::<Probe>(&[0x82, 0x18, 0x07, 0x43, 1, 2, 3]),
            None
        );
        // Trailing byte.
        assert_eq!(
            from_cbor_canonical::<Probe>(&[0x82, 0x07, 0x43, 1, 2, 3, 0]),
            None
        );
        // An extra field.
        assert_eq!(
            from_cbor_canonical::<Probe>(&[0x83, 0x07, 0x43, 1, 2, 3, 0]),
            None
        );
    }

    #[test]
    fn bytes32_is_a_byte_string() {
        let b = Bytes32([0xab; 32]);
        let enc = to_cbor(&b);
        assert_eq!(enc.len(), 34);
        assert_eq!(&enc[..2], &[0x58, 32]);
    }
}
