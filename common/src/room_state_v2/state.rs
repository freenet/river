//! The v2 state as a whole: canonical encoding, `validate_state`, merge,
//! summary and delta.

use super::config::ceiling;
use super::encoding::{compact_struct, from_cbor_canonical, to_cbor, Digest, Key};
use super::records::{Kind, Record};
use super::retain::{retain, signature_ok, Retained, Verifier};
use std::collections::{BTreeMap, BTreeSet, HashSet};

pub const STATE_VERSION: u32 = 2;

compact_struct! {
    /// A room state: records sorted by digest, no duplicates.
    pub struct RoomStateV2 { pub version: u32, pub records: Vec<Record> }
}

/// The exact set of record digests, per kind.
pub type Summary = BTreeMap<Kind, BTreeSet<Digest>>;

/// Largest encoded size of one record of each kind, pinned by
/// `tests::record_size_ceilings_hold`, which builds the largest record of
/// each kind the retention function keeps.
pub const MAX_RECORD_BYTES: [usize; 24] = [
    13_000,  // Configuration
    14_000,  // LegacyConfiguration
    80,      // Clock
    560_000, // Checkpoint (keep list of max_links digests)
    120,     // Upgrade
    40_000,  // Roster batch (1,000 entries)
    80,      // SecretVersion
    560,     // Ban
    360,     // LegacyBan
    420,     // Link
    360,     // LegacyLink
    120,     // Activity
    21_000,  // LegacyActivity (a v1 message)
    8_000,   // Restore (a chain of up to 16 links)
    1_200,   // MemberInfo
    3_300,   // LegacyMemberInfo
    10_300,  // Message
    21_000,  // LegacyMessage
    33_100,  // DirectMessage
    66_000,  // LegacyDirectMessage
    17_200,  // Purge
    17_400,  // LegacyPurge
    720,     // SecretBlob
    1_500,   // LegacySecretBlob
];

/// The byte ceiling of a state: for each group of kinds that share a cap,
/// the cap ceiling times the group's largest record. `f` enforces every cap,
/// so it never emits a state above this.
pub fn max_state_bytes() -> usize {
    use ceiling::*;
    let largest = |ks: &[usize]| ks.iter().map(|&k| MAX_RECORD_BYTES[k]).max().unwrap();
    let members = MAX_MEMBERS as usize + 1;
    let groups: [(&[usize], usize); 15] = [
        (&[0, 1], 1),
        (&[2], 1),
        (&[3], 1),
        (&[4], 1),
        (&[6], MAX_SECRET_VERSIONS as usize),
        (
            &[7, 8],
            (MAX_OWNER_BANS + MAX_DEPUTY_BANS + MAX_MEMBER_BANS) as usize,
        ),
        (&[9, 10], MAX_LINKS as usize),
        (&[11, 12], MAX_LINKS as usize),
        (&[13], RESTORE_REQUESTS),
        (&[14, 15], members),
        (&[16, 17], MAX_RECENT_MESSAGES as usize),
        (&[18, 19], MAX_DIRECT_MESSAGES as usize),
        (&[20, 21], members),
        (&[22, 23], MAX_SECRET_BLOBS as usize),
        // Roster batches are bounded by their entries, below.
        (&[5], 0),
    ];
    // A roster entry is 36 bytes; a batch adds its header and signature.
    let roster_entries: usize = super::records::LEGACY_KINDS
        .iter()
        .map(|k| super::retain::roster_ceiling(*k))
        .sum();
    let roster = roster_entries * 36 + ROSTER_BATCHES * 100;
    groups.iter().map(|(ks, n)| largest(ks) * n).sum::<usize>() + roster + 16
}

/// Why `validate_state` refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Invalid {
    TooLarge(usize),
    NotCanonical,
    WrongVersion(u32),
    NotSortedUnique,
    /// `f(S) != S`: the number of records `f` drops.
    NotAFixpoint(usize),
    /// A record whose signature fails (contract-boundary rule for deltas).
    BadSignature(Digest),
}

impl RoomStateV2 {
    /// The state holding exactly these records (sorted, deduplicated). Not
    /// normalized: see [`normalize`].
    pub fn from_records(mut records: Vec<Record>) -> Self {
        let mut keyed: Vec<(Digest, Record)> = records.drain(..).map(|r| (r.digest(), r)).collect();
        keyed.sort_by_key(|a| a.0);
        keyed.dedup_by(|a, b| a.0 == b.0);
        RoomStateV2 {
            version: STATE_VERSION,
            records: keyed.into_iter().map(|(_, r)| r).collect(),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        to_cbor(self)
    }

    pub fn digests(&self) -> Vec<Digest> {
        self.records.iter().map(|r| r.digest()).collect()
    }

    pub fn summarize(&self) -> Summary {
        let mut s = Summary::new();
        for r in &self.records {
            s.entry(r.kind()).or_default().insert(r.digest());
        }
        s
    }

    /// Every record the holder of `summary` lacks. A valid state holds every
    /// record its records depend on, so this is closed under dependencies.
    /// `None` when there is nothing to send.
    pub fn delta(&self, summary: &Summary) -> Option<Vec<Record>> {
        let out: Vec<Record> = self
            .records
            .iter()
            .filter(|r| {
                !summary
                    .get(&r.kind())
                    .is_some_and(|s| s.contains(&r.digest()))
            })
            .cloned()
            .collect();
        (!out.is_empty()).then_some(out)
    }
}

/// `f` applied to a set of records: the normalized state and what `f` decided.
pub fn normalize(owner: &Key, records: Vec<Record>, v: &mut Verifier) -> (RoomStateV2, Retained) {
    let s = RoomStateV2::from_records(records);
    let r = retain(owner, &s.records, v);
    let kept = s
        .records
        .into_iter()
        .zip(r.keep.iter())
        .filter(|(_, k)| **k)
        .map(|(rec, _)| rec)
        .collect();
    (
        RoomStateV2 {
            version: STATE_VERSION,
            records: kept,
        },
        r,
    )
}

/// `merge(A, B) = f(A ∪ B)`.
pub fn merge(owner: &Key, a: &RoomStateV2, b: &RoomStateV2, v: &mut Verifier) -> RoomStateV2 {
    let mut all = a.records.clone();
    all.extend(b.records.iter().cloned());
    normalize(owner, all, v).0
}

/// `validate_state`: canonical encoding within the byte ceiling, sorted
/// without duplicates, version 2, and `f(S) == S`. Every per-record rule is
/// a filter or cap inside `f`, so `f` never emits a state this refuses.
pub fn validate_state(owner: &Key, bytes: &[u8], v: &mut Verifier) -> Result<RoomStateV2, Invalid> {
    if bytes.len() > max_state_bytes() {
        return Err(Invalid::TooLarge(bytes.len()));
    }
    let s: RoomStateV2 = from_cbor_canonical(bytes).ok_or(Invalid::NotCanonical)?;
    if s.version != STATE_VERSION {
        return Err(Invalid::WrongVersion(s.version));
    }
    let digests = s.digests();
    if !digests.windows(2).all(|w| w[0] < w[1]) {
        return Err(Invalid::NotSortedUnique);
    }
    let r = retain(owner, &s.records, v);
    let dropped = r.keep.iter().filter(|k| !**k).count();
    if dropped > 0 {
        return Err(Invalid::NotAFixpoint(dropped));
    }
    Ok(s)
}

/// `apply_delta(S, D) = f(S ∪ D)` at the contract boundary. `state` is a
/// state this peer validated, so its records are trusted. The delta is
/// refused if it holds a record whose signature fails (no valid state or
/// generated delta holds one); records already held are not checked again.
pub fn apply_delta(
    owner: &Key,
    state: &RoomStateV2,
    delta: Vec<Record>,
    v: &mut Verifier,
) -> Result<RoomStateV2, Invalid> {
    let held: HashSet<Digest> = state.digests().into_iter().collect();
    v.trust(held.iter().copied());
    for r in &delta {
        let d = r.digest();
        if !held.contains(&d) && !v.ok(owner, r, &d) {
            return Err(Invalid::BadSignature(d));
        }
    }
    let mut all = state.records.clone();
    all.extend(delta);
    Ok(normalize(owner, all, v).0)
}

/// Whether a record's signature verifies, without the rest of the state.
pub fn record_signature_ok(owner: &Key, r: &Record) -> bool {
    signature_ok(owner, r)
}
