//! The exact time a signature covers.
//!
//! River signs the CBOR of structs that hold a time (`MessageV1.time`, `UserBan.banned_at`,
//! `SecretVersionRecordV1.created_at`, `ProvisionalBan.banned_at`). A verifier decodes the struct
//! and encodes it again to check the signature. With `std::time::SystemTime` that round trip is
//! lossy on Windows, whose clock has 100 ns resolution: a nanosecond field that is not a multiple
//! of 100 comes back different, the bytes differ, and the signature fails (freenet/river#971,
//! #1018).
//!
//! [`SignedTime`] holds the `(secs, nanos)` pair exactly as it was signed, on every platform. Its
//! serde encoding is byte for byte the encoding of `SystemTime` (a struct named `SystemTime`
//! with `secs_since_epoch: u64` and `nanos_since_epoch: u32`), so every signature made before
//! this type existed still verifies. Convert to `SystemTime` only at the edges: display and
//! comparison with the clock.

use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::{Add, AddAssign, Sub, SubAssign};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const NANOS_PER_SEC: u32 = 1_000_000_000;

/// A time since the Unix epoch, held exactly as `(secs, nanos)`.
///
/// Ordering is chronological. `nanos` is always below one second.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SignedTime {
    secs: u64,
    nanos: u32,
}

impl SignedTime {
    pub const UNIX_EPOCH: SignedTime = SignedTime { secs: 0, nanos: 0 };

    /// Builds the time, carrying nanoseconds of a second or more into the seconds.
    ///
    /// Returns `None` where `SystemTime`'s deserializer returns an error: when the result
    /// would not fit an `i64` of seconds.
    pub fn new(secs: u64, nanos: u32) -> Option<Self> {
        let secs = secs.checked_add(u64::from(nanos / NANOS_PER_SEC))?;
        if secs > i64::MAX as u64 {
            return None;
        }
        Some(SignedTime {
            secs,
            nanos: nanos % NANOS_PER_SEC,
        })
    }

    /// Whole seconds since the epoch.
    pub fn secs(&self) -> u64 {
        self.secs
    }

    /// Nanoseconds past `secs()`, below 1_000_000_000.
    pub fn nanos(&self) -> u32 {
        self.nanos
    }

    /// The current time, truncated to a whole millisecond, so a signature made from it survives
    /// a decode on every platform (#1013).
    // The one raw clock read for signed times; it is rounded in the same expression.
    #[allow(clippy::disallowed_methods)]
    pub fn now() -> Self {
        Self::from(SystemTime::now()).whole_millis()
    }

    /// This time truncated to a whole millisecond.
    pub fn whole_millis(self) -> Self {
        SignedTime {
            secs: self.secs,
            nanos: self.nanos / 1_000_000 * 1_000_000,
        }
    }

    pub fn checked_add(self, d: Duration) -> Option<Self> {
        let nanos = self.nanos + d.subsec_nanos();
        Self::new(self.secs.checked_add(d.as_secs())?, nanos)
    }

    pub fn checked_sub(self, d: Duration) -> Option<Self> {
        let mut secs = self.secs.checked_sub(d.as_secs())?;
        let nanos = if self.nanos >= d.subsec_nanos() {
            self.nanos - d.subsec_nanos()
        } else {
            secs = secs.checked_sub(1)?;
            self.nanos + NANOS_PER_SEC - d.subsec_nanos()
        };
        Some(SignedTime { secs, nanos })
    }

    /// How long after `earlier` this time is, or `None` if `earlier` is later.
    pub fn duration_since(self, earlier: SignedTime) -> Option<Duration> {
        let a = Duration::new(self.secs, self.nanos);
        a.checked_sub(Duration::new(earlier.secs, earlier.nanos))
    }

    /// This time as a `Duration` since the epoch.
    pub fn since_epoch(self) -> Duration {
        Duration::new(self.secs, self.nanos)
    }

    /// The platform time. On Windows the nanoseconds are floored to 100 ns: use this
    /// for display and clock comparisons, never to rebuild signed bytes.
    pub fn to_system_time(self) -> SystemTime {
        UNIX_EPOCH + self.since_epoch()
    }
}

impl fmt::Debug for SignedTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SignedTime({}.{:09})", self.secs, self.nanos)
    }
}

impl Default for SignedTime {
    fn default() -> Self {
        Self::UNIX_EPOCH
    }
}

/// A time before the epoch becomes the epoch: `SystemTime` cannot serialize one either, so no
/// signable value is lost. A time past the `i64` seconds range saturates likewise.
impl From<SystemTime> for SignedTime {
    fn from(t: SystemTime) -> Self {
        match t.duration_since(UNIX_EPOCH) {
            Ok(d) => SignedTime::new(d.as_secs(), d.subsec_nanos()).unwrap_or(SignedTime {
                secs: i64::MAX as u64,
                nanos: NANOS_PER_SEC - 1,
            }),
            Err(_) => SignedTime::UNIX_EPOCH,
        }
    }
}

impl From<SignedTime> for SystemTime {
    fn from(t: SignedTime) -> Self {
        t.to_system_time()
    }
}

impl Add<Duration> for SignedTime {
    type Output = SignedTime;
    fn add(self, d: Duration) -> SignedTime {
        self.checked_add(d).expect("overflow adding to SignedTime")
    }
}

impl Sub<Duration> for SignedTime {
    type Output = SignedTime;
    fn sub(self, d: Duration) -> SignedTime {
        self.checked_sub(d)
            .expect("overflow subtracting from SignedTime")
    }
}

impl AddAssign<Duration> for SignedTime {
    fn add_assign(&mut self, d: Duration) {
        *self = *self + d;
    }
}

impl SubAssign<Duration> for SignedTime {
    fn sub_assign(&mut self, d: Duration) {
        *self = *self - d;
    }
}

impl Serialize for SignedTime {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("SystemTime", 2)?;
        state.serialize_field("secs_since_epoch", &self.secs)?;
        state.serialize_field("nanos_since_epoch", &self.nanos)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for SignedTime {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        enum Field {
            Secs,
            Nanos,
        }

        impl<'de> Deserialize<'de> for Field {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct FieldVisitor;
                impl<'de> Visitor<'de> for FieldVisitor {
                    type Value = Field;
                    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                        f.write_str("`secs_since_epoch` or `nanos_since_epoch`")
                    }
                    fn visit_str<E: de::Error>(self, v: &str) -> Result<Field, E> {
                        match v {
                            "secs_since_epoch" => Ok(Field::Secs),
                            "nanos_since_epoch" => Ok(Field::Nanos),
                            _ => Err(de::Error::unknown_field(v, FIELDS)),
                        }
                    }
                    fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Field, E> {
                        match v {
                            b"secs_since_epoch" => Ok(Field::Secs),
                            b"nanos_since_epoch" => Ok(Field::Nanos),
                            _ => {
                                let v = String::from_utf8_lossy(v);
                                Err(de::Error::unknown_field(&v, FIELDS))
                            }
                        }
                    }
                }
                deserializer.deserialize_identifier(FieldVisitor)
            }
        }

        const FIELDS: &[&str] = &["secs_since_epoch", "nanos_since_epoch"];

        fn build<E: de::Error>(secs: u64, nanos: u32) -> Result<SignedTime, E> {
            // `SystemTime` rejects an overflowing carry, then a sum past its range.
            if secs.checked_add(u64::from(nanos / NANOS_PER_SEC)).is_none() {
                return Err(E::custom("overflow deserializing SystemTime epoch offset"));
            }
            SignedTime::new(secs, nanos)
                .ok_or_else(|| E::custom("overflow deserializing SystemTime"))
        }

        struct TimeVisitor;
        impl<'de> Visitor<'de> for TimeVisitor {
            type Value = SignedTime;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("struct SystemTime")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<SignedTime, A::Error> {
                let secs: u64 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let nanos: u32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                build(secs, nanos)
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<SignedTime, A::Error> {
                let mut secs: Option<u64> = None;
                let mut nanos: Option<u32> = None;
                while let Some(key) = map.next_key()? {
                    match key {
                        Field::Secs => {
                            if secs.is_some() {
                                return Err(de::Error::duplicate_field("secs_since_epoch"));
                            }
                            secs = Some(map.next_value()?);
                        }
                        Field::Nanos => {
                            if nanos.is_some() {
                                return Err(de::Error::duplicate_field("nanos_since_epoch"));
                            }
                            nanos = Some(map.next_value()?);
                        }
                    }
                }
                let secs = secs.ok_or_else(|| de::Error::missing_field("secs_since_epoch"))?;
                let nanos = nanos.ok_or_else(|| de::Error::missing_field("nanos_since_epoch"))?;
                build(secs, nanos)
            }
        }

        deserializer.deserialize_struct("SystemTime", FIELDS, TimeVisitor)
    }
}
