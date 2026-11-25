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

    /// The latest time that can be held.
    pub const MAX: SignedTime = SignedTime {
        secs: u64::MAX,
        nanos: NANOS_PER_SEC - 1,
    };

    /// Builds the time, carrying nanoseconds of a second or more into the seconds.
    ///
    /// Returns `None` only when the carry overflows `u64` seconds. Any `u64` of seconds is
    /// accepted, as serde's `SystemTime` decoder accepts it on wasm32 (where `SystemTime` is a
    /// plain `Duration`): the contract and UI run there, so a record the old build decoded
    /// must still decode. Native platforms hold fewer seconds, which only matters at
    /// [`SignedTime::to_system_time`].
    pub fn new(secs: u64, nanos: u32) -> Option<Self> {
        let secs = secs.checked_add(u64::from(nanos / NANOS_PER_SEC))?;
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
    ///
    /// Never panics: a value past what the platform clock can hold (Windows stops well
    /// before `i64::MAX` seconds) is clamped to the latest representable time. The signed
    /// value itself is unchanged.
    pub fn to_system_time(self) -> SystemTime {
        clamped_system_time(self.since_epoch(), |d| UNIX_EPOCH.checked_add(d))
    }
}

/// `add(d)` is the platform's checked "epoch plus `d`". If it refuses `d`, return the latest
/// time it accepts (to the nanosecond where it can).
fn clamped_system_time(d: Duration, add: impl Fn(Duration) -> Option<SystemTime>) -> SystemTime {
    if let Some(t) = add(d) {
        return t;
    }
    // Binary search for the largest whole-second offset the platform holds.
    let (mut lo, mut hi) = (0u64, d.as_secs());
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if add(Duration::from_secs(mid)).is_some() {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let whole = Duration::from_secs(lo);
    whole
        .checked_add(Duration::from_nanos(u64::from(NANOS_PER_SEC - 1)))
        .and_then(&add)
        .or_else(|| add(whole))
        .unwrap_or(UNIX_EPOCH)
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    fn t(secs: u64, nanos: u32) -> SignedTime {
        SignedTime::new(secs, nanos).unwrap()
    }

    #[test]
    fn new_carries_nanos_and_rejects_only_overflow() {
        assert_eq!(t(5, 2_500_000_000), t(7, 500_000_000));
        assert_eq!(t(5, 1_000_000_000).nanos(), 0);
        assert_eq!(t(5, 1_000_000_000).secs(), 6);
        assert_eq!(t(u64::MAX, 999_999_999), SignedTime::MAX);
        assert!(SignedTime::new(u64::MAX, 1_000_000_000).is_none());
        assert!(SignedTime::new(u64::MAX - 1, 2_000_000_000).is_none());
        assert_eq!(t(u64::MAX - 1, 1_999_999_999).secs(), u64::MAX);
        // Past i64::MAX is accepted, as SystemTime accepts it on wasm32.
        assert_eq!(t(1 << 63, 0).secs(), 1 << 63);
    }

    #[test]
    fn checked_add_carries_and_overflows() {
        assert_eq!(
            t(1, 900_000_000).checked_add(Duration::new(2, 200_000_000)),
            Some(t(4, 100_000_000))
        );
        assert_eq!(SignedTime::MAX.checked_add(Duration::new(0, 1)), None);
        assert_eq!(t(u64::MAX, 0).checked_add(Duration::new(1, 0)), None);
        assert_eq!(
            t(u64::MAX - 1, 999_999_999).checked_add(Duration::new(0, 1)),
            Some(t(u64::MAX, 0))
        );
    }

    #[test]
    fn checked_sub_borrows_and_underflows() {
        assert_eq!(
            t(4, 100_000_000).checked_sub(Duration::new(2, 200_000_000)),
            Some(t(1, 900_000_000))
        );
        assert_eq!(
            t(4, 300_000_000).checked_sub(Duration::new(2, 200_000_000)),
            Some(t(2, 100_000_000))
        );
        assert_eq!(t(0, 5).checked_sub(Duration::new(0, 6)), None);
        assert_eq!(t(1, 5).checked_sub(Duration::new(2, 0)), None);
        assert_eq!(t(1, 5).checked_sub(Duration::new(1, 5)), Some(t(0, 0)));
    }

    #[test]
    fn operators_saturate_instead_of_panicking() {
        assert_eq!(SignedTime::MAX + Duration::from_secs(1), SignedTime::MAX);
        assert_eq!(
            SignedTime::UNIX_EPOCH - Duration::from_secs(1),
            SignedTime::UNIX_EPOCH
        );
        let mut x = t(10, 0);
        x += Duration::from_secs(1);
        x -= Duration::from_secs(2);
        assert_eq!(x, t(9, 0));
    }

    #[test]
    fn duration_since_is_none_when_earlier_is_later() {
        assert_eq!(
            t(5, 0).duration_since(t(3, 500_000_000)),
            Some(Duration::new(1, 500_000_000))
        );
        assert_eq!(t(3, 0).duration_since(t(3, 1)), None);
    }

    #[test]
    fn whole_millis_truncates_and_is_idempotent() {
        let x = t(1_700_000_000, 123_456_789).whole_millis();
        assert_eq!(x, t(1_700_000_000, 123_000_000));
        assert_eq!(x.whole_millis(), x);
        assert_eq!(SignedTime::MAX.whole_millis(), t(u64::MAX, 999_000_000));
        assert_eq!(SignedTime::now().nanos() % 1_000_000, 0);
    }

    #[test]
    fn nothing_panics_at_the_extremes() {
        for x in [
            SignedTime::UNIX_EPOCH,
            t(1 << 63, 0),
            t(i64::MAX as u64 + 1, 1),
            SignedTime::MAX,
        ] {
            let _ = format!("{x:?}");
            let _ = SystemTime::from(x);
            let _ = x.cmp(&SignedTime::MAX);
            let _ = x + Duration::from_secs(1);
            let _ = x - Duration::from_secs(1);
            let _ = x.whole_millis();
            let _ = x.since_epoch();
            assert!(SignedTime::from(x.to_system_time()) <= x);
        }
    }
}

#[cfg(test)]
mod clamp_tests {
    use super::*;

    #[test]
    fn out_of_range_values_clamp_to_the_platform_limit() {
        // A platform that holds only up to 1000 s after the epoch.
        let limit = Duration::from_secs(1000);
        let add = |d: Duration| {
            if d <= limit {
                UNIX_EPOCH.checked_add(d)
            } else {
                None
            }
        };
        let t = clamped_system_time(Duration::new(5000, 7), add);
        assert_eq!(t, UNIX_EPOCH + limit);
        // In range: exact.
        let t = clamped_system_time(Duration::new(10, 7), add);
        assert_eq!(t, UNIX_EPOCH + Duration::new(10, 7));
        // A platform that refuses everything yields the epoch rather than panicking.
        assert_eq!(
            clamped_system_time(Duration::new(5, 0), |_| None),
            UNIX_EPOCH
        );
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
/// signable value is lost. A `SystemTime` always fits, so nothing else saturates.
impl From<SystemTime> for SignedTime {
    fn from(t: SystemTime) -> Self {
        match t.duration_since(UNIX_EPOCH) {
            Ok(d) => SignedTime::new(d.as_secs(), d.subsec_nanos()).unwrap_or(SignedTime::MAX),
            Err(_) => SignedTime::UNIX_EPOCH,
        }
    }
}

impl From<SignedTime> for SystemTime {
    fn from(t: SignedTime) -> Self {
        t.to_system_time()
    }
}

/// Saturates at [`SignedTime::MAX`] instead of panicking: times come from peers.
impl Add<Duration> for SignedTime {
    type Output = SignedTime;
    fn add(self, d: Duration) -> SignedTime {
        self.checked_add(d).unwrap_or(SignedTime::MAX)
    }
}

/// Saturates at the epoch instead of panicking: times come from peers.
impl Sub<Duration> for SignedTime {
    type Output = SignedTime;
    fn sub(self, d: Duration) -> SignedTime {
        self.checked_sub(d).unwrap_or(SignedTime::UNIX_EPOCH)
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
            // serde's `Duration` decoder rejects only an overflowing carry.
            SignedTime::new(secs, nanos)
                .ok_or_else(|| E::custom("overflow deserializing SystemTime epoch offset"))
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
