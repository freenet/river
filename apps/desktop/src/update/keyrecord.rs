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
        Authority::new(vk, field("app_id")?)
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

/// Who is trusted to sign manifests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effective {
    /// No record has been accepted: the key compiled into this build.
    Compiled,
    /// The key the highest-version verified record names.
    Key([u8; 32]),
    /// The record withdrew the key: nobody is trusted.
    Withdrawn,
}

/// The floor a kept record gives, or `None` when the text is not a record `authority` signed
/// naming a usable key (or a withdrawal). A kept record is judged again every time, so a
/// state file someone else wrote changes nothing.
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

/// The trusted key given the record kept in the state file.
pub fn effective(authority: Option<&Authority>, kept: Option<&str>) -> Effective {
    let Some(floor) = authority.zip(kept).and_then(|(a, kept)| floor_of(a, kept)) else {
        return Effective::Compiled;
    };
    match floor.code_hash() {
        _ if floor.is_withdrawn() => Effective::Withdrawn,
        Some(key) => Effective::Key(key),
        None => Effective::Compiled,
    }
}

/// What to keep after `fetched`: `Some(new record, as hex)` when it is a verified record above
/// the kept one (naming a usable key, or withdrawing), otherwise `None` and nothing changes.
///
/// Older, competing, forged, for another app or naming a key nobody can use all count as
/// no news, as does an unreachable node or an absent record: none of them may move the key
/// or, once a record was kept, bring the compiled-in key back.
pub fn judge(authority: &Authority, kept: Option<&str>, fetched: &KeyRecord) -> Option<String> {
    let KeyRecord::State(bytes) = fetched else {
        return None;
    };
    let floor = kept
        .and_then(|k| floor_of(authority, k))
        .unwrap_or_else(PointerFloor::never_resolved);
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
        let mut non_canonical = [0xff; 32];
        non_canonical[0] = 0xee;
        non_canonical[31] = 0x7f; // y = p + 1
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
        assert_eq!(effective(Some(&a), Some(&forged)), Effective::Compiled);
        let mut good = record(1, key_of(2));
        good[10] ^= 1; // one byte of the key
        assert_eq!(
            effective(Some(&a), Some(&hex::encode(good))),
            Effective::Compiled
        );
        assert_eq!(effective(Some(&a), Some("not hex")), Effective::Compiled);
    }
}
