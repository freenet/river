//! Timestamps that get signed.
//!
//! River signs the CBOR of structs holding a `SystemTime` (`MessageV1.time`, `UserBan.banned_at`,
//! `SecretVersionRecordV1.created_at`). A verifier decodes the struct and serialises it again to
//! check the signature. On Windows a `SystemTime` has 100 ns resolution, so a timestamp whose
//! nanosecond field is not a multiple of 100 changes when it is decoded there, the bytes differ and
//! the signature fails (freenet/river#971). Every time this crate signs is therefore a whole
//! millisecond, the precision the web UI stamps and one every platform keeps exactly.
//!
//! Never apply this to a timestamp that is already signed: it would change the signed bytes.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// `t` truncated to a whole millisecond.
pub fn whole_millis(t: SystemTime) -> SystemTime {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => UNIX_EPOCH + trunc(d),
        Err(e) => UNIX_EPOCH - trunc(e.duration()),
    }
}

fn trunc(d: Duration) -> Duration {
    Duration::new(d.as_secs(), d.subsec_millis() * 1_000_000)
}

/// The current time, as a whole millisecond. Use for every signed timestamp.
// The one raw clock read for signed times; it is rounded on the next line.
#[allow(clippy::disallowed_methods)]
pub fn now() -> SystemTime {
    whole_millis(SystemTime::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use river_core::room_state::ban::{AuthorizedUserBan, UserBan};
    use river_core::room_state::member::MemberId;
    use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};

    /// A decode on Windows: the nanosecond field keeps only whole 100 ns units.
    fn windows_decode(t: SystemTime) -> SystemTime {
        let d = t.duration_since(UNIX_EPOCH).unwrap();
        UNIX_EPOCH + Duration::new(d.as_secs(), d.subsec_nanos() / 100 * 100)
    }

    /// Not a multiple of 100 ns, as `SystemTime::now()` is on Linux and macOS.
    fn awkward() -> SystemTime {
        UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789)
    }

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    #[derive(serde::Serialize)]
    struct RawTime {
        secs_since_epoch: u64,
        nanos_since_epoch: u32,
    }

    /// `MessageV1` as the signer's CBOR holds it, with the time written as raw numbers so it is
    /// not floored to this platform's clock resolution before it is signed.
    #[derive(serde::Serialize)]
    struct RawMessage {
        room_owner: MemberId,
        author: MemberId,
        time: RawTime,
        content: RoomMessageBody,
    }

    #[derive(serde::Serialize)]
    struct RawBan {
        owner_member_id: MemberId,
        banned_at: RawTime,
        banned_user: MemberId,
    }

    fn raw(nanos: u32) -> RawTime {
        RawTime {
            secs_since_epoch: 1_700_000_000,
            nanos_since_epoch: nanos,
        }
    }

    fn cbor<T: serde::Serialize>(v: &T) -> Vec<u8> {
        let mut out = Vec::new();
        ciborium::ser::into_writer(v, &mut out).unwrap();
        out
    }

    /// What a verifier does: decode the signed bytes into the real struct. On Windows that decode
    /// floors the time to 100 ns by itself; elsewhere `windows_decode` does what it does, so the
    /// check fails for the same reason on every platform.
    fn decoded_message(bytes: &[u8], sig: ed25519_dalek::Signature) -> AuthorizedMessageV1 {
        let mut m: MessageV1 = ciborium::de::from_reader(bytes).unwrap();
        m.time = windows_decode(m.time.into()).into();
        AuthorizedMessageV1::with_signature(m, sig)
    }

    fn decoded_ban(bytes: &[u8], by: MemberId, sig: ed25519_dalek::Signature) -> AuthorizedUserBan {
        let mut b: UserBan = ciborium::de::from_reader(bytes).unwrap();
        b.banned_at = windows_decode(b.banned_at.into()).into();
        AuthorizedUserBan::with_signature(b, by, sig)
    }

    fn signed_raw(
        nanos: u32,
    ) -> (
        SigningKey,
        Vec<u8>,
        ed25519_dalek::Signature,
        Vec<u8>,
        ed25519_dalek::Signature,
    ) {
        use ed25519_dalek::Signer;
        let sk = key();
        let me = MemberId::from(sk.verifying_key());
        let other = MemberId::from(&SigningKey::from_bytes(&[9u8; 32]).verifying_key());
        let m = cbor(&RawMessage {
            room_owner: me,
            author: me,
            time: raw(nanos),
            content: RoomMessageBody::public("hi".to_string()),
        });
        let b = cbor(&RawBan {
            owner_member_id: me,
            banned_at: raw(nanos),
            banned_user: other,
        });
        let (ms, bs) = (sk.sign(&m), sk.sign(&b));
        (sk, m, ms, b, bs)
    }

    #[test]
    fn a_time_with_nanoseconds_signed_by_another_platform_fails_after_decode() {
        // 123_456_789 ns is what `SystemTime::now()` gives on Linux, not a multiple of 100.
        let (sk, m, ms, b, bs) = signed_raw(123_456_789);
        let me = MemberId::from(sk.verifying_key());
        assert!(decoded_message(&m, ms)
            .validate(&sk.verifying_key())
            .is_err());
        assert!(decoded_ban(&b, me, bs)
            .verify_signature(&sk.verifying_key())
            .is_err());
    }

    #[test]
    fn a_whole_millisecond_signed_the_same_way_verifies_after_decode() {
        let (sk, m, ms, b, bs) = signed_raw(123_000_000);
        let me = MemberId::from(sk.verifying_key());
        decoded_message(&m, ms)
            .validate(&sk.verifying_key())
            .unwrap();
        decoded_ban(&b, me, bs)
            .verify_signature(&sk.verifying_key())
            .unwrap();
    }

    #[test]
    fn rounding_truncates_and_is_idempotent() {
        let t = whole_millis(awkward());
        assert_eq!(t, UNIX_EPOCH + Duration::new(1_700_000_000, 123_000_000));
        assert_eq!(whole_millis(t), t);
        let before = UNIX_EPOCH - Duration::new(5, 123_456_789);
        // Truncation is toward the epoch, as for a time after it.
        assert_eq!(
            whole_millis(before),
            UNIX_EPOCH - Duration::new(5, 123_000_000)
        );
        assert_eq!(whole_millis(whole_millis(before)), whole_millis(before));
    }

    #[test]
    fn now_is_a_whole_millisecond() {
        for _ in 0..1000 {
            let d = now().duration_since(UNIX_EPOCH).unwrap();
            assert_eq!(d.subsec_nanos() % 1_000_000, 0);
        }
    }
}
