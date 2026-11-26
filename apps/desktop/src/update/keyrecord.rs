//! The update-key record: how River learns that its update-signing key was replaced or
//! withdrawn (freenet/river#927). The design and its limits are in `# Trust` in [`super`];
//! `update-authority.toml` is the trust anchor and says how to publish a record.
//!
//! This module is the only place that decides what a served record means. It judges a record
//! with freenet-migrate's [`PointerResolver`] against a floor rebuilt from the record the last
//! check kept, as `apps/river-app/src/rooms/generation.rs` does for its own records.

use ed25519_dalek::VerifyingKey;
use freenet_migrate::{PointerFloor, PointerOutcome, PointerRecord, PointerResolver};
use river_app::KeyRecord;

/// The compiled-in trust anchor.
pub const AUTHORITY_TOML: &str = include_str!("../../update-authority.toml");

const AUTHOR_VK_PREFIX: &str = "river:v1:vk:";

/// What a damaged kept record says.
pub const DAMAGED: &str = "River's note of its update key is damaged, so River will not update \
     itself. Download a new build from https://github.com/freenet/river/releases";

/// What a withdrawn key says.
pub const WITHDRAWN: &str = "River's update key was withdrawn, so River will not update itself. \
     Download a new build from https://github.com/freenet/river/releases";

/// Who signs the record, and which record it is.
#[derive(Debug, Clone)]
pub struct Authority {
    vk: VerifyingKey,
    app_id: String,
    params: Vec<u8>,
    pointer_id: [u8; 32],
    /// The newest record this build knew when it was made (hex): the floor a fresh install
    /// starts from, so a record older than that cannot be adopted over it.
    pinned: Option<String>,
}

impl Authority {
    /// The one this build ships with.
    pub fn shipped() -> Result<Authority, String> {
        Authority::parse(AUTHORITY_TOML)
    }

    pub fn parse(text: &str) -> Result<Authority, String> {
        let doc: toml::Table = text.parse().map_err(|e| format!("not TOML: {e}"))?;
        let field = |name: &str| {
            doc.get(name)
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("no {name}"))
        };
        let b58 = field("authority_verifying_key")?
            .strip_prefix(AUTHOR_VK_PREFIX)
            .ok_or("authority_verifying_key does not start with river:v1:vk:")?;
        let bytes: [u8; 32] = bs58::decode(b58)
            .into_vec()
            .ok()
            .and_then(|v| v.try_into().ok())
            .ok_or("authority_verifying_key is not 32 bytes of base58")?;
        let vk = VerifyingKey::from_bytes(&bytes)
            .map_err(|_| "authority_verifying_key is not a valid key".to_string())?;
        let authority = Authority::new(vk, field("app_id")?)?;
        match doc.get("pinned_record") {
            None => Ok(authority),
            Some(v) => authority.with_pinned(v.as_str().ok_or("pinned_record is not text")?),
        }
    }

    /// This authority with `record` (hex of the 100 signed bytes) as the build's floor. Errs
    /// unless it is a record this authority signed, naming a usable key or withdrawing.
    pub fn with_pinned(mut self, record: &str) -> Result<Authority, String> {
        if floor_of(&self, record).is_none() {
            return Err(
                "update-authority.toml: pinned_record is not a valid record signed by \
                 the authority key for this app id"
                    .into(),
            );
        }
        self.pinned = Some(record.to_owned());
        Ok(self)
    }

    pub fn new(vk: VerifyingKey, app_id: &str) -> Result<Authority, String> {
        let params = freenet_migrate::pointer_params(&vk, app_id.as_bytes())
            .map_err(|e| format!("update-authority.toml: {e}"))?;
        let id = freenet_migrate::pointer_contract_id(&vk, app_id.as_bytes())
            .map_err(|e| format!("update-authority.toml: {e}"))?;
        let pointer_id: [u8; 32] = id.as_bytes().try_into().map_err(|_| "bad contract id")?;
        Ok(Authority {
            vk,
            app_id: app_id.to_owned(),
            params,
            pointer_id,
            pinned: None,
        })
    }

    /// The address of the record, for the node fetch.
    pub fn pointer_id(&self) -> [u8; 32] {
        self.pointer_id
    }
}

/// Whether `key` may verify manifests: a canonical point that is not weak (a small-order key
/// verifies forged signatures) and not all zero (the placeholder, which is also the tombstone).
pub fn usable_key(key: &[u8; 32]) -> bool {
    !super::is_placeholder(key)
        && VerifyingKey::from_bytes(key).is_ok_and(|k| !k.is_weak())
        && freenet_migrate::is_canonical_field_element(key)
}

/// The floor a kept record gives, or `None` when the text is not a record `authority` signed
/// naming a usable key (or a withdrawal). A kept record is judged again every time, so a
/// state file someone else wrote cannot name a key.
fn floor_of(authority: &Authority, kept: &str) -> Option<PointerFloor> {
    let record = PointerRecord::decode(&hex::decode(kept).ok()?).ok()?;
    record.verify(&authority.params, &authority.vk).ok()?;
    if record.is_tombstone() {
        PointerFloor::withdrawn_at(record.version).ok()
    } else if usable_key(&record.code_hash) {
        PointerFloor::at(record.version, record.code_hash).ok()
    } else {
        None
    }
}

/// Who is trusted to sign manifests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effective {
    /// No record is known (none kept, none pinned): the key compiled into this build.
    Compiled,
    /// The key the highest-version verified record names.
    Key([u8; 32]),
    /// The record withdrew the key: nobody is trusted.
    Withdrawn,
    /// A record is kept but does not verify: nobody is trusted until a served record replaces
    /// it. Falling back to the compiled-in key here would let a damaged or edited file bring a
    /// revoked key back.
    Damaged,
}

/// The floor in force: the higher of the kept record and the one pinned into this build.
enum Base {
    Floor(PointerFloor),
    Nothing,
    Damaged,
}

fn base(authority: &Authority, kept: Option<&str>) -> Base {
    let pinned = authority
        .pinned
        .as_deref()
        .and_then(|p| floor_of(authority, p));
    let Some(kept) = kept else {
        return pinned.map_or(Base::Nothing, Base::Floor);
    };
    let Some(kept) = floor_of(authority, kept) else {
        return Base::Damaged;
    };
    match pinned {
        // The build knows a newer record than the one kept (the kept one came from an older
        // build's state): the pinned one stands. Equal versions: the kept one, which only ever
        // got there by winning the tiebreak.
        Some(pinned) if pinned.version() > kept.version() => Base::Floor(pinned),
        _ => Base::Floor(kept),
    }
}

/// The trusted key given the record kept in the state file.
pub fn effective(authority: Option<&Authority>, kept: Option<&str>) -> Effective {
    let Some(authority) = authority else {
        return Effective::Compiled;
    };
    match base(authority, kept) {
        Base::Nothing => Effective::Compiled,
        Base::Damaged => Effective::Damaged,
        Base::Floor(floor) if floor.is_withdrawn() => Effective::Withdrawn,
        Base::Floor(floor) => floor
            .code_hash()
            .map_or(Effective::Compiled, Effective::Key),
    }
}

/// What to keep after `fetched`: `Some(new record, as hex)` when it is a verified record above
/// the kept one (naming a usable key, or withdrawing), otherwise `None` and nothing changes.
///
/// Older, competing, forged, for another app or naming a key nobody can use all count as
/// no news, as does an unreachable node or an absent record: none of them may move the key.
pub fn judge(authority: &Authority, kept: Option<&str>, fetched: &KeyRecord) -> Option<String> {
    let KeyRecord::State(bytes) = fetched else {
        return None;
    };
    // A damaged kept record is not a floor, and a served record above the pinned one repairs it.
    let floor = match base(authority, kept) {
        Base::Floor(floor) => floor,
        Base::Nothing | Base::Damaged => PointerFloor::never_resolved(),
    };
    let mut resolver =
        PointerResolver::new(&authority.vk, authority.app_id.as_bytes(), floor).ok()?;
    // Arms the resolver's event gate; the fetch itself already happened.
    let _ = resolver.next_action();
    if !resolver.on_response(resolver.pointer_id(), bytes) {
        return None;
    }
    match resolver.take_outcome()? {
        Ok(PointerOutcome::Resolved(r)) if usable_key(&r.code_hash()) => Some(hex::encode(bytes)),
        Ok(PointerOutcome::Withdrawn { .. }) => Some(hex::encode(bytes)),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    pub const APP_ID: &str = "river-desktop-update-key";

    pub fn authority_key() -> SigningKey {
        SigningKey::from_bytes(&[200; 32])
    }

    pub fn authority() -> Authority {
        Authority::new(authority_key().verifying_key(), APP_ID).unwrap()
    }

    /// A record `signer` signed for `app_id` under the authority key `vk`, naming `payload`.
    pub fn record_for(
        signer: &SigningKey,
        app_id: &str,
        version: u32,
        payload: [u8; 32],
    ) -> Vec<u8> {
        let params =
            freenet_migrate::pointer_params(&authority_key().verifying_key(), app_id.as_bytes())
                .unwrap();
        let msg = freenet_migrate::pointer_signing_message(&params, version, &payload);
        PointerRecord {
            version,
            code_hash: payload,
            signature: signer.sign(&msg).to_bytes(),
        }
        .encode()
        .to_vec()
    }

    /// A record the real authority signed for the real app id.
    pub fn record(version: u32, payload: [u8; 32]) -> Vec<u8> {
        record_for(&authority_key(), APP_ID, version, payload)
    }

    pub fn key_of(seed: u8) -> [u8; 32] {
        SigningKey::from_bytes(&[seed; 32])
            .verifying_key()
            .to_bytes()
    }

    fn state(bytes: Vec<u8>) -> KeyRecord {
        KeyRecord::State(bytes)
    }

    #[test]
    fn the_shipped_anchor_parses_and_its_address_is_the_recorded_one() {
        let a = Authority::shipped().unwrap();
        let doc: toml::Table = AUTHORITY_TOML.parse().unwrap();
        let recorded = doc["pointer_key"].as_str().unwrap();
        assert_eq!(bs58::encode(a.pointer_id()).into_string(), recorded);
        // The authority key is River's author key, which pointer-records.toml publishes.
        let records: toml::Table = include_str!("../../../../pointer-records.toml")
            .parse()
            .unwrap();
        assert_eq!(
            doc["authority_verifying_key"],
            records["author_verifying_key"]
        );
        assert_eq!(doc["app_id"].as_str(), Some("river-desktop-update-key"));
    }

    #[test]
    fn nothing_kept_means_the_compiled_key() {
        let a = authority();
        assert_eq!(effective(Some(&a), None), Effective::Compiled);
        assert_eq!(effective(None, Some("00")), Effective::Compiled);
    }

    #[test]
    fn a_verified_record_names_the_key_and_is_kept() {
        let a = authority();
        let kept = judge(&a, None, &state(record(1, key_of(2)))).unwrap();
        assert_eq!(effective(Some(&a), Some(&kept)), Effective::Key(key_of(2)));
    }

    #[test]
    fn a_higher_version_rotates_and_a_lower_one_is_ignored() {
        let a = authority();
        let v2 = judge(&a, None, &state(record(2, key_of(3)))).unwrap();
        assert_eq!(judge(&a, Some(&v2), &state(record(1, key_of(2)))), None);
        assert_eq!(judge(&a, Some(&v2), &state(record(2, key_of(3)))), None);
        let v3 = judge(&a, Some(&v2), &state(record(3, key_of(4)))).unwrap();
        assert_eq!(effective(Some(&a), Some(&v3)), Effective::Key(key_of(4)));
    }

    /// The author signing twice at one version converges, as the network does, on the lower
    /// key: the higher one changes nothing, and the lower one replaces a higher one kept.
    #[test]
    fn a_competing_record_at_the_kept_version_resolves_to_the_lower_key() {
        let a = authority();
        let (lo, hi) = if key_of(3) < key_of(4) {
            (3, 4)
        } else {
            (4, 3)
        };
        let kept_lo = judge(&a, None, &state(record(2, key_of(lo)))).unwrap();
        assert_eq!(
            judge(&a, Some(&kept_lo), &state(record(2, key_of(hi)))),
            None
        );
        let kept_hi = judge(&a, None, &state(record(2, key_of(hi)))).unwrap();
        let won = judge(&a, Some(&kept_hi), &state(record(2, key_of(lo)))).unwrap();
        assert_eq!(effective(Some(&a), Some(&won)), Effective::Key(key_of(lo)));
    }

    #[test]
    fn records_from_another_signer_or_app_are_no_news() {
        let a = authority();
        let stranger = SigningKey::from_bytes(&[9; 32]);
        for bad in [
            record_for(&stranger, APP_ID, 1, key_of(2)),
            record_for(&authority_key(), "river.room-contract", 1, key_of(2)),
            vec![0; 100],
            vec![1, 2, 3],
        ] {
            assert_eq!(judge(&a, None, &state(bad)), None);
        }
    }

    #[test]
    fn unreachable_and_absent_change_nothing() {
        let a = authority();
        let kept = judge(&a, None, &state(record(1, key_of(2)))).unwrap();
        for fetched in [KeyRecord::Unavailable, KeyRecord::Absent] {
            assert_eq!(judge(&a, Some(&kept), &fetched), None);
        }
        assert_eq!(effective(Some(&a), Some(&kept)), Effective::Key(key_of(2)));
    }

    #[test]
    fn a_tombstone_withdraws_and_only_a_higher_record_lifts_it() {
        let a = authority();
        let v2 = judge(&a, None, &state(record(2, [0; 32]))).unwrap();
        assert_eq!(effective(Some(&a), Some(&v2)), Effective::Withdrawn);
        // Replaying the real record from before the withdrawal does not undo it.
        assert_eq!(judge(&a, Some(&v2), &state(record(1, key_of(2)))), None);
        assert_eq!(judge(&a, Some(&v2), &state(record(2, key_of(2)))), None);
        let v3 = judge(&a, Some(&v2), &state(record(3, key_of(5)))).unwrap();
        assert_eq!(effective(Some(&a), Some(&v3)), Effective::Key(key_of(5)));
    }

    #[test]
    fn an_unusable_key_is_never_adopted() {
        let a = authority();
        let mut identity = [0u8; 32];
        identity[0] = 1; // the identity point: valid, and verifies anything
        // A point that is NOT weak, written with y + p instead of y: dalek accepts it, so only
        // the canonical check refuses it.
        let non_canonical = (2u8..19)
            .find_map(|y| {
                let mut bytes = [0xff; 32];
                bytes[0] = 0xed + y; // p = 2^255 - 19, so y + p ends 0xed + y
                bytes[31] = 0x7f;
                let k = VerifyingKey::from_bytes(&bytes).ok()?;
                (!k.is_weak()).then_some(bytes)
            })
            .expect("some small y is a point");
        let dalek_accepts = VerifyingKey::from_bytes(&non_canonical).is_ok();
        assert!(
            dalek_accepts,
            "this test is about a key only the canonical check refuses"
        );
        for bad in [identity, non_canonical] {
            assert!(!usable_key(&bad));
            assert_eq!(judge(&a, None, &state(record(1, bad))), None);
        }
        // All zero is the tombstone encoding: it withdraws, it never becomes a key.
        let kept = judge(&a, None, &state(record(1, [0; 32]))).unwrap();
        assert_eq!(effective(Some(&a), Some(&kept)), Effective::Withdrawn);
        assert!(usable_key(&key_of(2)));
    }

    #[test]
    fn a_kept_record_is_judged_again() {
        let a = authority();
        let stranger = SigningKey::from_bytes(&[9; 32]);
        let forged = hex::encode(record_for(&stranger, APP_ID, 1, key_of(2)));
        assert_eq!(effective(Some(&a), Some(&forged)), Effective::Damaged);
        let mut good = record(1, key_of(2));
        good[10] ^= 1; // one byte of the key
        assert_eq!(
            effective(Some(&a), Some(&hex::encode(good))),
            Effective::Damaged
        );
        assert_eq!(effective(Some(&a), Some("not hex")), Effective::Damaged);
        assert_eq!(effective(Some(&a), Some("")), Effective::Damaged);
        // A served record replaces a damaged one.
        let fixed = judge(&a, Some("not hex"), &state(record(1, key_of(2)))).unwrap();
        assert_eq!(effective(Some(&a), Some(&fixed)), Effective::Key(key_of(2)));
    }

    #[test]
    fn a_pinned_record_is_the_floor_of_a_fresh_install() {
        // v1 named K2, K2 leaked, v2 names K3 and is pinned into this build.
        let a = authority()
            .with_pinned(&hex::encode(record(2, key_of(3))))
            .unwrap();
        assert_eq!(effective(Some(&a), None), Effective::Key(key_of(3)));
        // A node that has not merged v2 serves v1: it must not replace K3.
        assert_eq!(judge(&a, None, &state(record(1, key_of(2)))), None);
        assert_eq!(judge(&a, None, &state(record(2, key_of(3)))), None);
        // A newer record is adopted, and outranks the pin from then on.
        let v4 = judge(&a, None, &state(record(4, key_of(5)))).unwrap();
        assert_eq!(effective(Some(&a), Some(&v4)), Effective::Key(key_of(5)));
        // A kept record older than the pin (an older build's state file) does not win.
        let old = hex::encode(record(1, key_of(2)));
        assert_eq!(effective(Some(&a), Some(&old)), Effective::Key(key_of(3)));
        // A pinned tombstone withdraws a fresh install.
        let w = authority()
            .with_pinned(&hex::encode(record(2, [0; 32])))
            .unwrap();
        assert_eq!(effective(Some(&w), None), Effective::Withdrawn);
    }

    #[test]
    fn a_bad_pin_is_refused() {
        let stranger = SigningKey::from_bytes(&[9; 32]);
        for bad in [
            hex::encode(record_for(&stranger, APP_ID, 1, key_of(2))),
            "00".to_string(),
            hex::encode(record(1, {
                let mut weak = [0u8; 32];
                weak[0] = 1; // signed, but the identity point
                weak
            })),
        ] {
            assert!(authority().with_pinned(&bad).is_err(), "{bad}");
        }
        let text = format!(
            "{}\npinned_record = \"{}\"\n",
            AUTHORITY_TOML
                .lines()
                .filter(|l| !l.starts_with('#'))
                .collect::<Vec<_>>()
                .join("\n"),
            "zz"
        );
        assert!(Authority::parse(&text).is_err());
        // The shipped file pins nothing yet, or pins a record that verifies (parse checks).
        assert!(Authority::shipped().is_ok());
    }
}
