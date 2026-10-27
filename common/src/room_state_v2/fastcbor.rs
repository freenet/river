//! A small CBOR codec for v1 records, byte-identical to ciborium on the
//! types v1 records use, and much cheaper in WASM for their byte vectors.
//!
//! v1 types derive serde, so a `Vec<u8>` field is a CBOR array of integers;
//! ciborium decodes and encodes it one element at a time through its reader
//! and writer layers, which in WASM made a legacy record cost several times a
//! v2 record of the same size. This codec reads from and writes to a byte
//! slice directly. It accepts only definite lengths and minimal headers, and
//! [`from_canonical`] still requires the re-encoding to equal the input, so
//! one value keeps one wire form. `fastcbor_matches_ciborium` (tests) checks
//! both directions against ciborium on every v1 record kind.
//!
//! Supported: unsigned and negative integers, byte and text strings, arrays,
//! maps, `false`/`true`/`null`, and serde's data model mapped as ciborium
//! maps it (structs as maps with text keys, newtype variants as one-entry
//! maps, unit variants as text, `None` as `null`). Not supported (refused):
//! tags, floats, indefinite lengths. v1 records use none of them.

use serde::de::{self, DeserializeSeed, IntoDeserializer, Visitor};
use serde::ser::{self, Serialize};
use std::fmt;

#[derive(Debug)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl de::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Error(msg.to_string())
    }
}

impl ser::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Error(msg.to_string())
    }
}

fn err<T>(m: &str) -> Result<T, Error> {
    Err(Error(m.to_string()))
}

// ------------------------------------------------------------------ encoder

/// The CBOR bytes of `v`, as ciborium writes them.
pub fn to_vec<T: Serialize + ?Sized>(v: &T) -> Vec<u8> {
    let mut out = Vec::new();
    v.serialize(&mut Encoder { out: &mut out })
        .expect("v1 types encode");
    out
}

struct Encoder<'a> {
    out: &'a mut Vec<u8>,
}

fn head(out: &mut Vec<u8>, major: u8, v: u64) {
    let m = major << 5;
    if v < 24 {
        out.push(m | v as u8);
    } else if v <= u8::MAX as u64 {
        out.push(m | 24);
        out.push(v as u8);
    } else if v <= u16::MAX as u64 {
        out.push(m | 25);
        out.extend_from_slice(&(v as u16).to_be_bytes());
    } else if v <= u32::MAX as u64 {
        out.push(m | 26);
        out.extend_from_slice(&(v as u32).to_be_bytes());
    } else {
        out.push(m | 27);
        out.extend_from_slice(&v.to_be_bytes());
    }
}

impl Encoder<'_> {
    fn int(&mut self, v: i128) -> Result<(), Error> {
        if v >= 0 {
            if v > u64::MAX as i128 {
                return err("integer out of range");
            }
            head(self.out, 0, v as u64);
        } else {
            let n = -1 - v;
            if n > u64::MAX as i128 {
                return err("integer out of range");
            }
            head(self.out, 1, n as u64);
        }
        Ok(())
    }
    fn text(&mut self, s: &str) {
        head(self.out, 3, s.len() as u64);
        self.out.extend_from_slice(s.as_bytes());
    }
}

impl<'a, 'b> ser::Serializer for &'b mut Encoder<'a> {
    type Ok = ();
    type Error = Error;
    type SerializeSeq = Self;
    type SerializeTuple = Self;
    type SerializeTupleStruct = Self;
    type SerializeTupleVariant = Self;
    type SerializeMap = Self;
    type SerializeStruct = Self;
    type SerializeStructVariant = Self;

    fn serialize_bool(self, v: bool) -> Result<(), Error> {
        self.out.push(if v { 0xf5 } else { 0xf4 });
        Ok(())
    }
    fn serialize_i8(self, v: i8) -> Result<(), Error> {
        self.int(v as i128)
    }
    fn serialize_i16(self, v: i16) -> Result<(), Error> {
        self.int(v as i128)
    }
    fn serialize_i32(self, v: i32) -> Result<(), Error> {
        self.int(v as i128)
    }
    fn serialize_i64(self, v: i64) -> Result<(), Error> {
        self.int(v as i128)
    }
    fn serialize_u8(self, v: u8) -> Result<(), Error> {
        head(self.out, 0, v as u64);
        Ok(())
    }
    fn serialize_u16(self, v: u16) -> Result<(), Error> {
        head(self.out, 0, v as u64);
        Ok(())
    }
    fn serialize_u32(self, v: u32) -> Result<(), Error> {
        head(self.out, 0, v as u64);
        Ok(())
    }
    fn serialize_u64(self, v: u64) -> Result<(), Error> {
        head(self.out, 0, v);
        Ok(())
    }
    fn serialize_f32(self, _: f32) -> Result<(), Error> {
        err("floats are not supported")
    }
    fn serialize_f64(self, _: f64) -> Result<(), Error> {
        err("floats are not supported")
    }
    fn serialize_char(self, v: char) -> Result<(), Error> {
        self.text(v.encode_utf8(&mut [0; 4]));
        Ok(())
    }
    fn serialize_str(self, v: &str) -> Result<(), Error> {
        self.text(v);
        Ok(())
    }
    fn serialize_bytes(self, v: &[u8]) -> Result<(), Error> {
        head(self.out, 2, v.len() as u64);
        self.out.extend_from_slice(v);
        Ok(())
    }
    fn serialize_none(self) -> Result<(), Error> {
        self.out.push(0xf6);
        Ok(())
    }
    fn serialize_some<T: Serialize + ?Sized>(self, v: &T) -> Result<(), Error> {
        v.serialize(self)
    }
    fn serialize_unit(self) -> Result<(), Error> {
        self.out.push(0xf6);
        Ok(())
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<(), Error> {
        self.out.push(0xf6);
        Ok(())
    }
    fn serialize_unit_variant(self, _: &'static str, _: u32, v: &'static str) -> Result<(), Error> {
        self.text(v);
        Ok(())
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        v: &T,
    ) -> Result<(), Error> {
        v.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        v: &T,
    ) -> Result<(), Error> {
        head(self.out, 5, 1);
        self.text(variant);
        v.serialize(self)
    }
    fn serialize_seq(self, len: Option<usize>) -> Result<Self, Error> {
        match len {
            Some(n) => {
                head(self.out, 4, n as u64);
                Ok(self)
            }
            None => err("indefinite lengths are not supported"),
        }
    }
    fn serialize_tuple(self, len: usize) -> Result<Self, Error> {
        head(self.out, 4, len as u64);
        Ok(self)
    }
    fn serialize_tuple_struct(self, _: &'static str, len: usize) -> Result<Self, Error> {
        head(self.out, 4, len as u64);
        Ok(self)
    }
    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self, Error> {
        head(self.out, 5, 1);
        self.text(variant);
        head(self.out, 4, len as u64);
        Ok(self)
    }
    fn serialize_map(self, len: Option<usize>) -> Result<Self, Error> {
        match len {
            Some(n) => {
                head(self.out, 5, n as u64);
                Ok(self)
            }
            None => err("indefinite lengths are not supported"),
        }
    }
    fn serialize_struct(self, _: &'static str, len: usize) -> Result<Self, Error> {
        head(self.out, 5, len as u64);
        Ok(self)
    }
    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self, Error> {
        head(self.out, 5, 1);
        self.text(variant);
        head(self.out, 5, len as u64);
        Ok(self)
    }
}

macro_rules! compound {
    ($tr:ident, $method:ident) => {
        impl ser::$tr for &mut Encoder<'_> {
            type Ok = ();
            type Error = Error;
            fn $method<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), Error> {
                v.serialize(&mut **self)
            }
            fn end(self) -> Result<(), Error> {
                Ok(())
            }
        }
    };
}
compound!(SerializeSeq, serialize_element);
compound!(SerializeTuple, serialize_element);
compound!(SerializeTupleStruct, serialize_field);
compound!(SerializeTupleVariant, serialize_field);

impl ser::SerializeMap for &mut Encoder<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_key<T: Serialize + ?Sized>(&mut self, k: &T) -> Result<(), Error> {
        k.serialize(&mut **self)
    }
    fn serialize_value<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), Error> {
        v.serialize(&mut **self)
    }
    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl ser::SerializeStruct for &mut Encoder<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        v: &T,
    ) -> Result<(), Error> {
        self.text(key);
        v.serialize(&mut **self)
    }
    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl ser::SerializeStructVariant for &mut Encoder<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        v: &T,
    ) -> Result<(), Error> {
        self.text(key);
        v.serialize(&mut **self)
    }
    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

// ------------------------------------------------------------------ decoder

/// Decodes `bytes` as a `T` whose re-encoding is exactly `bytes`.
pub fn from_canonical<T: Serialize + de::DeserializeOwned>(bytes: &[u8]) -> Option<T> {
    let mut d = Decoder { b: bytes, i: 0 };
    let v = T::deserialize(&mut d).ok()?;
    if d.i != bytes.len() {
        return None;
    }
    (to_vec(&v) == bytes).then_some(v)
}

struct Decoder<'de> {
    b: &'de [u8],
    i: usize,
}

// Errors are built lazily (`ok_or_else`): `peek` and `head` run once per
// element of a v1 byte vector, and an eager `ok_or(Error(..))` allocated a
// String for each, which made this codec slower than ciborium in WASM.
impl<'de> Decoder<'de> {
    fn peek(&self) -> Result<u8, Error> {
        self.b
            .get(self.i)
            .copied()
            .ok_or_else(|| Error("end of input".into()))
    }

    /// A header: (major type, argument). Minimal encodings only.
    fn head(&mut self) -> Result<(u8, u64), Error> {
        let ib = self.peek()?;
        self.i += 1;
        let major = ib >> 5;
        let info = ib & 31;
        let take = |d: &mut Self, n: usize| -> Result<u64, Error> {
            let s =
                d.b.get(d.i..d.i + n)
                    .ok_or_else(|| Error("end of input".into()))?;
            d.i += n;
            Ok(s.iter().fold(0u64, |a, &x| (a << 8) | x as u64))
        };
        let v = match info {
            0..=23 => info as u64,
            24 => {
                let v = take(self, 1)?;
                if v < 24 {
                    return err("non-minimal header");
                }
                v
            }
            25 => {
                let v = take(self, 2)?;
                if v <= u8::MAX as u64 {
                    return err("non-minimal header");
                }
                v
            }
            26 => {
                let v = take(self, 4)?;
                if v <= u16::MAX as u64 {
                    return err("non-minimal header");
                }
                v
            }
            27 => {
                let v = take(self, 8)?;
                if v <= u32::MAX as u64 {
                    return err("non-minimal header");
                }
                v
            }
            _ => return err("indefinite length or reserved header"),
        };
        Ok((major, v))
    }

    fn slice(&mut self, n: u64) -> Result<&'de [u8], Error> {
        let n = usize::try_from(n).map_err(|_| Error("length".into()))?;
        let s = self
            .b
            .get(
                self.i
                    ..self
                        .i
                        .checked_add(n)
                        .ok_or_else(|| Error("length".into()))?,
            )
            .ok_or_else(|| Error("end of input".into()))?;
        self.i += n;
        Ok(s)
    }
}

impl<'de> de::Deserializer<'de> for &mut Decoder<'de> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, v: V) -> Result<V::Value, Error> {
        let ib = self.peek()?;
        if ib >> 5 == 7 {
            self.i += 1;
            return match ib {
                0xf4 => v.visit_bool(false),
                0xf5 => v.visit_bool(true),
                0xf6 => v.visit_unit(),
                _ => err("unsupported simple value or float"),
            };
        }
        let (major, a) = self.head()?;
        match major {
            0 => v.visit_u64(a),
            1 => {
                let n = -1i128 - a as i128;
                match i64::try_from(n) {
                    Ok(n) => v.visit_i64(n),
                    Err(_) => v.visit_i128(n),
                }
            }
            2 => v.visit_borrowed_bytes(self.slice(a)?),
            3 => {
                let s = self.slice(a)?;
                v.visit_borrowed_str(std::str::from_utf8(s).map_err(|_| Error("utf-8".into()))?)
            }
            4 => v.visit_seq(Seq { d: self, left: a }),
            5 => v.visit_map(Map { d: self, left: a }),
            _ => err("tags are not supported"),
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, v: V) -> Result<V::Value, Error> {
        if self.peek()? == 0xf6 {
            self.i += 1;
            v.visit_none()
        } else {
            v.visit_some(self)
        }
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        v: V,
    ) -> Result<V::Value, Error> {
        v.visit_newtype_struct(self)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _: &'static str,
        _: &'static [&'static str],
        v: V,
    ) -> Result<V::Value, Error> {
        let ib = self.peek()?;
        match ib >> 5 {
            // A unit variant, by name.
            3 => {
                let (_, a) = self.head()?;
                let s = std::str::from_utf8(self.slice(a)?).map_err(|_| Error("utf-8".into()))?;
                v.visit_enum(s.into_deserializer())
            }
            5 => {
                let (_, n) = self.head()?;
                if n != 1 {
                    return err("an enum is a one-entry map");
                }
                v.visit_enum(Enum { d: self })
            }
            _ => err("expected an enum"),
        }
    }

    // A sequence may arrive as a byte string (`Vec<u8>` deserializing a
    // field another type wrote with `serialize_bytes`), as ciborium allows.
    fn deserialize_seq<V: Visitor<'de>>(self, v: V) -> Result<V::Value, Error> {
        if self.peek()? >> 5 == 2 {
            let (_, a) = self.head()?;
            let s = self.slice(a)?;
            return v.visit_seq(ByteSeq { b: s, i: 0 });
        }
        self.deserialize_any(v)
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, _: usize, v: V) -> Result<V::Value, Error> {
        self.deserialize_seq(v)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf unit unit_struct tuple_struct map struct
        identifier ignored_any
    }
}

struct Seq<'a, 'de> {
    d: &'a mut Decoder<'de>,
    left: u64,
}

impl<'de> de::SeqAccess<'de> for Seq<'_, 'de> {
    type Error = Error;
    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        s: T,
    ) -> Result<Option<T::Value>, Error> {
        if self.left == 0 {
            return Ok(None);
        }
        self.left -= 1;
        s.deserialize(&mut *self.d).map(Some)
    }
    fn size_hint(&self) -> Option<usize> {
        usize::try_from(self.left).ok().map(|n| n.min(1 << 16))
    }
}

struct ByteSeq<'de> {
    b: &'de [u8],
    i: usize,
}

impl<'de> de::SeqAccess<'de> for ByteSeq<'de> {
    type Error = Error;
    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        s: T,
    ) -> Result<Option<T::Value>, Error> {
        match self.b.get(self.i) {
            None => Ok(None),
            Some(&x) => {
                self.i += 1;
                s.deserialize(x.into_deserializer()).map(Some)
            }
        }
    }
    fn size_hint(&self) -> Option<usize> {
        Some(self.b.len() - self.i)
    }
}

struct Map<'a, 'de> {
    d: &'a mut Decoder<'de>,
    left: u64,
}

impl<'de> de::MapAccess<'de> for Map<'_, 'de> {
    type Error = Error;
    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, s: K) -> Result<Option<K::Value>, Error> {
        if self.left == 0 {
            return Ok(None);
        }
        self.left -= 1;
        s.deserialize(&mut *self.d).map(Some)
    }
    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, s: V) -> Result<V::Value, Error> {
        s.deserialize(&mut *self.d)
    }
}

struct Enum<'a, 'de> {
    d: &'a mut Decoder<'de>,
}

impl<'de> de::EnumAccess<'de> for Enum<'_, 'de> {
    type Error = Error;
    type Variant = Self;
    fn variant_seed<V: DeserializeSeed<'de>>(self, s: V) -> Result<(V::Value, Self), Error> {
        let v = s.deserialize(&mut *self.d)?;
        Ok((v, self))
    }
}

impl<'de> de::VariantAccess<'de> for Enum<'_, 'de> {
    type Error = Error;
    fn unit_variant(self) -> Result<(), Error> {
        // ciborium writes a unit variant as text, never as a map.
        err("unit variant in a map")
    }
    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, s: T) -> Result<T::Value, Error> {
        s.deserialize(&mut *self.d)
    }
    fn tuple_variant<V: Visitor<'de>>(self, len: usize, v: V) -> Result<V::Value, Error> {
        de::Deserializer::deserialize_tuple(&mut *self.d, len, v)
    }
    fn struct_variant<V: Visitor<'de>>(
        self,
        f: &'static [&'static str],
        v: V,
    ) -> Result<V::Value, Error> {
        de::Deserializer::deserialize_struct(&mut *self.d, "", f, v)
    }
}
