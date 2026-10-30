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
/// `tests::sizes::{v2,legacy}_record_size_ceilings_hold`, which build the
/// largest record of each kind the retention function keeps, and
/// `tests::state_tests::largest_state_at_the_ceilings`.
pub const MAX_RECORD_BYTES: [usize; 24] = [
    9_300,  // Configuration (64 deputies, 64 protected, 64 exceptions)
    0,      // (tag 1, reserved)
    80,     // Clock
    19_000, // Checkpoint (keep list of max_links digests)
    120,    // Upgrade
    37_000, // Roster batch (1,000 entries)
    80,     // SecretVersion
    500,    // Ban
    320,    // LegacyBan
    400,    // Link (a link parent, the largest seq and clock)
    300,    // LegacyLink
    120,    // Activity
    2_500,  // LegacyActivity (a v1 message)
    3_200,  // Restore (a chain of up to 8 links)
    260,    // MemberInfo
    1_200,  // LegacyMemberInfo
    2_200,  // Message
    2_500,  // LegacyMessage
    4_300,  // DirectMessage
    5_500,  // LegacyDirectMessage
    17_200, // Purge (1,000 tokens)
    17_400, // LegacyPurge (1,000 tokens)
    700,    // SecretBlob
    1_450,  // LegacySecretBlob
];

/// Groups of kinds that share a cap, with the most records of those kinds a
/// state `f` emits can hold. Every count is a cap `f` enforces whatever the
/// configuration (a configuration's caps are within the ceilings).
fn kind_groups() -> [(&'static [usize], usize); 18] {
    use ceiling::*;
    let members = MAX_MEMBERS as usize + 1;
    let legacy = super::retain::legacy_link_cap() as usize;
    [
        (&[0], 1),
        (&[1], 0),
        (&[2], 1),
        (&[3], 1),
        (&[4], 1),
        (&[5], ROSTER_BATCHES),
        (&[6], MAX_SECRET_VERSIONS as usize),
        (
            &[7, 8],
            (MAX_OWNER_BANS + MAX_DEPUTY_BANS + MAX_MEMBER_BANS) as usize,
        ),
        (&[9, 10], MAX_LINKS as usize),
        (&[10], legacy),
        // One activity record per linked key; a legacy one only for a key
        // with an admitted legacy link.
        (&[11, 12], MAX_LINKS as usize),
        (&[12], legacy),
        (&[13], RESTORE_REQUESTS),
        // One member info and one purge envelope per kept key.
        (&[14, 15], members),
        (&[16, 17], MAX_RECENT_MESSAGES as usize),
        (&[18, 19], MAX_DIRECT_MESSAGES as usize),
        (&[20, 21], members),
        (&[22, 23], MAX_SECRET_BLOBS as usize),
    ]
}

/// Whether `records` hold no more of any kind than a state `f` emits can.
/// Checked before decoding any legacy record or checking any signature, so
/// an incoming state or delta costs at most what the largest valid state
/// does. A delta generated from a valid state is a subset of one, so this
/// never refuses an honest update.
pub fn within_kind_counts(records: &[Record]) -> bool {
    let mut n = [0usize; 24];
    for r in records {
        n[r.kind() as usize] += 1;
    }
    kind_groups()
        .iter()
        .all(|(ks, max)| ks.iter().map(|&k| n[k]).sum::<usize>() <= *max)
}

/// The byte ceiling of a state: each group's cap times its largest record,
/// with the activity group split so a legacy activity record (larger) is
/// counted only up to the legacy link cap. `f` enforces every cap, so it
/// never emits a state above this.
pub fn max_state_bytes() -> usize {
    let largest = |ks: &[usize]| ks.iter().map(|&k| MAX_RECORD_BYTES[k]).max().unwrap_or(0);
    let mut total = 0;
    for (ks, n) in kind_groups() {
        if ks == [11, 12] {
            let legacy = super::retain::legacy_link_cap() as usize;
            total += MAX_RECORD_BYTES[11] * (n - legacy.min(n)) + largest(ks) * legacy.min(n);
        } else if ks == [20, 21] {
            // Purge envelopes: each one's overhead, plus the room-wide token
            // budget.
            total += n * ceiling::PURGE_ENVELOPE_BYTES
                + super::retain::purge_token_budget() * ceiling::PURGE_TOKEN_BYTES;
        } else if ks == [10] || ks == [12] || ks == [5] {
            // [10] and [12] are sub-limits of groups counted above; the
            // roster is bounded by its entries, below.
        } else {
            total += largest(ks) * n;
        }
    }
    // A roster entry is 36 bytes, repeats across batches included; a batch
    // adds its header and signature.
    total + super::retain::roster_entry_budget() * 36 + ceiling::ROSTER_BATCHES * 100 + 16
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
    /// A record whose signature fails.
    BadSignature(Digest),
    /// More records of some kind than a valid state holds.
    TooManyRecords,
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
/// without duplicates, version 2, no more records of a kind than `f` keeps,
/// and `f(S) == S`. Every per-record rule is a filter or cap inside `f`, so
/// `f` never emits a state this refuses. Signature checks stop at the first
/// failure: a fixpoint has none.
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
    if !within_kind_counts(&s.records) {
        return Err(Invalid::TooManyRecords);
    }
    let r = refusing(v, |v| retain(owner, &s.records, v))?;
    let dropped = r.keep.iter().filter(|k| !**k).count();
    if dropped > 0 {
        return Err(Invalid::NotAFixpoint(dropped));
    }
    Ok(s)
}

/// Runs `run` with `v` stopping at the first failed signature check, and
/// refuses if one failed.
fn refusing<T>(v: &mut Verifier, run: impl FnOnce(&mut Verifier) -> T) -> Result<T, Invalid> {
    v.set_abort(true);
    v.reset_failure();
    let out = run(v);
    v.set_abort(false);
    match v.failed.take() {
        Some(d) => Err(Invalid::BadSignature(d)),
        None => Ok(out),
    }
}

/// `apply_delta(S, D) = f(S ∪ D)` at the contract boundary. `state` is a
/// state this peer validated, so its records are trusted. The delta is
/// refused if it holds more records of a kind than a valid state can
/// (before any signature is checked), or if a signature `f` checks fails
/// (decision (n)); records `f` drops without examining them are dropped
/// unchecked. Records already held are not checked again.
pub fn apply_delta(
    owner: &Key,
    state: &RoomStateV2,
    delta: Vec<Record>,
    v: &mut Verifier,
) -> Result<RoomStateV2, Invalid> {
    if !within_kind_counts(&delta) {
        return Err(Invalid::TooManyRecords);
    }
    let held: HashSet<Digest> = state.digests().into_iter().collect();
    v.trust(owner, held.iter().copied());
    let mut all = state.records.clone();
    all.extend(delta.into_iter().filter(|r| !held.contains(&r.digest())));
    refusing(v, |v| normalize(owner, all, v).0)
}

/// Whether a record's signature verifies, without the rest of the state.
pub fn record_signature_ok(owner: &Key, r: &Record) -> bool {
    signature_ok(owner, r)
}
