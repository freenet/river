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

    fn message_at(t: SystemTime, sk: &SigningKey) -> AuthorizedMessageV1 {
        AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: MemberId::from(sk.verifying_key()),
                author: MemberId::from(sk.verifying_key()),
                time: t,
                content: RoomMessageBody::public("hi".to_string()),
            },
            sk,
        )
    }

    fn ban_at(t: SystemTime, sk: &SigningKey) -> AuthorizedUserBan {
        let me = MemberId::from(sk.verifying_key());
        AuthorizedUserBan::new(
            UserBan {
                owner_member_id: me,
                banned_at: t,
                banned_user: MemberId::from(&SigningKey::from_bytes(&[9u8; 32]).verifying_key()),
            },
            me,
            sk,
        )
    }

    #[test]
    fn unrounded_time_fails_after_a_windows_decode() {
        let sk = key();
        let mut m = message_at(awkward(), &sk);
        m.message.time = windows_decode(m.message.time);
        assert!(m.validate(&sk.verifying_key()).is_err());

        let mut b = ban_at(awkward(), &sk);
        b.ban.banned_at = windows_decode(b.ban.banned_at);
        assert!(b.verify_signature(&sk.verifying_key()).is_err());
    }

    #[test]
    fn rounded_time_survives_a_windows_decode() {
        let sk = key();
        let t = whole_millis(awkward());
        assert_eq!(windows_decode(t), t);

        let mut m = message_at(t, &sk);
        m.message.time = windows_decode(m.message.time);
        m.validate(&sk.verifying_key()).unwrap();

        let mut b = ban_at(t, &sk);
        b.ban.banned_at = windows_decode(b.ban.banned_at);
        b.verify_signature(&sk.verifying_key()).unwrap();
    }

    #[test]
    fn rounding_truncates_and_is_idempotent() {
        let t = whole_millis(awkward());
        assert_eq!(t, UNIX_EPOCH + Duration::new(1_700_000_000, 123_000_000));
        assert_eq!(whole_millis(t), t);
        let before = UNIX_EPOCH - Duration::new(5, 123_456_789);
        assert!(whole_millis(before).duration_since(UNIX_EPOCH).is_err());
    }

    #[test]
    fn now_is_a_whole_millisecond() {
        for _ in 0..1000 {
            let d = now().duration_since(UNIX_EPOCH).unwrap();
            assert_eq!(d.subsec_nanos() % 1_000_000, 0);
        }
    }
    /// The part of `src` before its test modules.
    fn production(src: &str) -> &str {
        let cut = src
            .find("\n#[cfg(test)]\nmod ")
            .or_else(|| src.find("\n#[cfg(test)]\n#[path"))
            .unwrap_or(src.len());
        &src[..cut]
    }

    /// Every signed-time constructor in production code goes through this module. A new one that
    /// stamps `SystemTime::now()` directly fails here rather than on a Windows user's machine.
    #[test]
    fn every_signed_time_in_production_code_is_rounded() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = vec![root.join("api.rs"), root.join("admission.rs")];
        for e in std::fs::read_dir(root.join("commands")).unwrap() {
            files.push(e.unwrap().path());
        }
        let mut checked = 0;
        for f in files {
            let src = std::fs::read_to_string(&f).unwrap();
            for (n, line) in production(&src).lines().enumerate() {
                let t = line.trim_start();
                let signed_field = ["time:", "banned_at:", "created_at:"]
                    .iter()
                    .any(|p| t.starts_with(p));
                if signed_field {
                    checked += 1;
                    assert!(
                        t.contains("signed_time::") || t == "time: message_time,",
                        "{}:{}: a signed time not from signed_time: {t}",
                        f.display(),
                        n + 1
                    );
                }
                if f.ends_with("api.rs") || f.ends_with("admission.rs") {
                    assert!(
                        !line.contains("SystemTime::now()"),
                        "{}:{}: use signed_time::now(): {t}",
                        f.display(),
                        n + 1
                    );
                }
                if t.starts_with("let message_time =") {
                    assert!(
                        t.contains("signed_time::now()"),
                        "{}:{}",
                        f.display(),
                        n + 1
                    );
                }
            }
        }
        // 1 admission join + 8 messages + secret record + ban in api.rs: the scan found them.
        assert!(checked >= 10, "scan found only {checked} signed times");
    }
}
