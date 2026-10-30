//! The layered retention function `f`. `merge(A, B) = f(A ∪ B)` and
//! `apply_delta(S, D) = f(S ∪ D)`.
//!
//! Each layer is a chooser: it keeps the subset of its records that are
//! valid (judged from the record's own bytes and the outputs of layers
//! above), then admits them greedily in a fixed order over their own fields
//! and fields fixed above, with per-group limits. No layer reads a layer
//! below it, and no layer deletes a record an earlier choice admitted: that
//! is what makes every state `f` emits a fixpoint and every merge
//! commutative, idempotent and absorbing. Each layer's comment names what it
//! reads; a change that makes a layer read a lower one, or remove a record
//! after admitting it, breaks the laws (the mutants in `tests` show how).
//!
//! Signature checks are a validity filter evaluated lazily in admission
//! order, after every structural check: a record whose signature fails is
//! skipped and the next is tried, which gives the same result as checking
//! every record first. [`Verifier`] memoizes by digest.

use super::config::{ceiling, ConfigurationV2, EPOCH_2026_SECS};
use super::encoding::{legacy_id, verify_payload, Digest, Key, Sig};
use super::legacy;
use super::records::*;
use crate::room_state::content::CONTENT_TYPE_EVENT;
use crate::room_state::message::RoomMessageBody;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

/// Memoized signature checks, keyed by record digest, for one room owner.
#[derive(Default)]
pub struct Verifier {
    verified: HashSet<Digest>,
    /// The owner the memo holds checks for: signing bytes include the owner,
    /// so a check under one owner says nothing under another.
    owner: Option<Key>,
    /// Signature checks actually run (memo misses), for measurement.
    pub checks: usize,
    /// Stop checking after the first failure: every later check fails
    /// without running. For callers that refuse on any failure.
    abort: bool,
    /// The first record whose check failed, since [`Self::reset_failure`].
    pub failed: Option<Digest>,
}

impl Verifier {
    pub fn new() -> Self {
        Self::default()
    }

    /// Binds the memo to `owner`, clearing it if it held another owner's.
    pub fn bind(&mut self, owner: &Key) {
        if self.owner != Some(*owner) {
            if self.owner.is_some() {
                self.verified.clear();
            }
            self.owner = Some(*owner);
        }
    }

    /// Marks records as already verified (records of a state this peer
    /// validated before), for `owner`.
    pub fn trust(&mut self, owner: &Key, digests: impl IntoIterator<Item = Digest>) {
        self.bind(owner);
        self.verified.extend(digests);
    }

    /// Whether later checks stop at the first failure.
    pub fn set_abort(&mut self, abort: bool) {
        self.abort = abort;
    }

    /// Forgets the recorded failure.
    pub fn reset_failure(&mut self) {
        self.failed = None;
    }

    pub fn ok(&mut self, owner: &Key, rec: &Record, digest: &Digest) -> bool {
        self.bind(owner);
        self.ok_with(digest, || signature_ok(owner, rec))
    }

    /// As [`Self::ok`], with the check supplied (a legacy record's v1
    /// signature, from its structural decode). The caller has bound the
    /// owner.
    pub fn ok_with(&mut self, digest: &Digest, check: impl FnOnce() -> bool) -> bool {
        if self.verified.contains(digest) {
            return true;
        }
        if self.abort && self.failed.is_some() {
            return false;
        }
        self.checks += 1;
        let ok = check();
        if ok {
            self.verified.insert(*digest);
        } else if self.failed.is_none() {
            self.failed = Some(*digest);
        }
        ok
    }
}

fn accepted_ok(owner: &Key, a: &AcceptedLink) -> bool {
    let inv = &a.invitation.payload;
    verify_payload(tag::INVITATION, owner, inv, &inv.inviter, &a.invitation.sig)
        && verify_payload(
            tag::ACCEPTANCE,
            owner,
            &a.acceptance,
            &inv.invite_key,
            &a.invite_key_sig,
        )
        && verify_payload(
            tag::ACCEPTANCE,
            owner,
            &a.acceptance,
            &a.acceptance.member,
            &a.member_sig,
        )
}

fn chain_member(owner: &Key, c: &ChainLink) -> Option<Key> {
    match c {
        ChainLink::Accepted(a) => Some(a.acceptance.member),
        ChainLink::Legacy(l) => legacy::decode_link(owner, l).map(|v| v.member),
    }
}

/// Whether every signature a record carries verifies under the key its own
/// bytes name (or the owner's). Context-free, so it can be checked at the
/// contract boundary without the rest of the state.
pub fn signature_ok(owner: &Key, rec: &Record) -> bool {
    let o = owner;
    match rec {
        Record::Configuration(s) => verify_payload(tag::CONFIGURATION, o, &s.payload, o, &s.sig),
        Record::Clock(s) => verify_payload(tag::CLOCK, o, &s.payload, o, &s.sig),
        Record::Checkpoint(s) => verify_payload(tag::CHECKPOINT, o, &s.payload, o, &s.sig),
        Record::Upgrade(s) => verify_payload(tag::UPGRADE, o, &s.payload, o, &s.sig),
        Record::Roster(s) => verify_payload(tag::ROSTER, o, &s.payload, o, &s.sig),
        Record::SecretVersion(s) => verify_payload(tag::SECRET_VERSION, o, &s.payload, o, &s.sig),
        Record::SecretBlob(s) => verify_payload(tag::SECRET_BLOB, o, &s.payload, o, &s.sig),
        Record::Ban(s) => {
            let signer = match &s.payload.issuer {
                Issuer::Owner(()) => *o,
                Issuer::Deputy(k) => *k,
                Issuer::Member(m) => m.key,
            };
            verify_payload(tag::BAN, o, &s.payload, &signer, &s.sig)
        }
        Record::Link(a) => accepted_ok(o, a),
        Record::Activity(s) => {
            verify_payload(tag::ACTIVITY, o, &s.payload, &s.payload.member, &s.sig)
        }
        Record::Restore(s) => {
            // `f` never keeps a longer chain; refuse it before verifying any.
            if s.payload.chain.len() > ceiling::MAX_CHAIN_DEPTH as usize {
                return false;
            }
            let Some(first) = s.payload.chain.first().and_then(|c| chain_member(o, c)) else {
                return false;
            };
            verify_payload(tag::RESTORE, o, &s.payload, &first, &s.sig)
                && s.payload.chain.iter().all(|c| match c {
                    ChainLink::Accepted(a) => accepted_ok(o, a),
                    ChainLink::Legacy(l) => legacy::verify(o, &Record::LegacyLink(l.clone())),
                })
        }
        Record::MemberInfo(s) => {
            verify_payload(tag::MEMBER_INFO, o, &s.payload, &s.payload.member, &s.sig)
        }
        Record::Message(s) => {
            verify_payload(tag::MESSAGE, o, &s.payload, &s.payload.author, &s.sig)
        }
        Record::DirectMessage(s) => verify_payload(
            tag::DIRECT_MESSAGE,
            o,
            &s.payload,
            &s.payload.sender,
            &s.sig,
        ),
        Record::Purge(s) => verify_payload(tag::PURGE, o, &s.payload, &s.payload.recipient, &s.sig),
        Record::LegacyBan(_)
        | Record::LegacyLink(_)
        | Record::LegacyActivity(_)
        | Record::LegacyMemberInfo(_)
        | Record::LegacyMessage(_)
        | Record::LegacyDirectMessage(_)
        | Record::LegacyPurge(_)
        | Record::LegacySecretBlob(_) => legacy::verify(o, rec),
    }
}

/// How many distinct records of each attested kind the roster may list.
/// Links: the legacy link cap, which the roster's count is part of (L2).
/// The rest: twice the kind's cap ceiling, so the owner can attest what the
/// room holds at conversion and again what it holds when the window closes
/// (records written during the window are new digests).
pub fn roster_ceiling(k: LegacyKind) -> usize {
    use ceiling::*;
    if k == LegacyKind::Link {
        return legacy_link_cap() as usize;
    }
    2 * (match k {
        LegacyKind::Link => unreachable!(),
        LegacyKind::Ban => MAX_OWNER_BANS + MAX_DEPUTY_BANS + MAX_MEMBER_BANS,
        LegacyKind::Activity => MAX_LEGACY_LINKS,
        LegacyKind::MemberInfo => MAX_MEMBERS + 1,
        LegacyKind::Message => MAX_RECENT_MESSAGES,
        LegacyKind::DirectMessage => MAX_DIRECT_MESSAGES,
        LegacyKind::Purge => MAX_MEMBERS + 1,
        LegacyKind::SecretBlob => MAX_SECRET_BLOBS,
    }) as usize
}

/// Entries of all kept roster batches together, repeats included: twice
/// the distinct entries the roster may hold, so the owner can list what the
/// room holds at conversion and all of it again at window close.
pub fn roster_entry_budget() -> usize {
    2 * super::records::LEGACY_KINDS
        .iter()
        .map(|k| roster_ceiling(*k))
        .sum::<usize>()
}

/// The legacy link cap: [`ceiling::MAX_LEGACY_LINKS`], or a smaller value
/// tests set to make it bind in small universes.
pub fn legacy_link_cap() -> u32 {
    #[cfg(test)]
    if let Some(n) = test_cap::get() {
        return n;
    }
    ceiling::MAX_LEGACY_LINKS
}

/// The room-wide purge token budget: [`ceiling::PURGE_TOKEN_BUDGET`], or a
/// smaller value tests set to make it bind in small universes.
pub fn purge_token_budget() -> usize {
    #[cfg(test)]
    if let Some(n) = test_cap::purge() {
        return n;
    }
    ceiling::PURGE_TOKEN_BUDGET
}

/// Test overrides of the legacy link cap and the purge token budget (per
/// thread).
#[cfg(test)]
pub(crate) mod test_cap {
    thread_local! {
        static CAP: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
        static PURGE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    }
    pub fn get() -> Option<u32> {
        CAP.with(|c| c.get())
    }
    pub fn set(n: Option<u32>) {
        CAP.with(|c| c.set(n))
    }
    pub fn purge() -> Option<usize> {
        PURGE.with(|c| c.get())
    }
    pub fn set_purge(n: Option<usize>) {
        PURGE.with(|c| c.set(n))
    }
}

/// What `f` decided, beyond the kept set: the layer outputs tests inspect.
#[derive(Debug, Clone)]
pub struct Retained {
    /// `keep[i]`: input record `i` is in the output.
    pub keep: Vec<bool>,
    pub config: ConfigurationV2,
    pub clock: Option<u32>,
    /// Admitted links, in BFS admission order, as (record index, member key, depth).
    pub links: Vec<(usize, Key, u32)>,
    /// Kept members (not the owner), sorted.
    pub members: Vec<Key>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum TargetView {
    Key(Key),
    Inv(Digest, Key),
    Id(i64),
}

impl TargetView {
    fn of(t: &BanTarget) -> Self {
        match t {
            BanTarget::Key(k) => TargetView::Key(*k),
            BanTarget::Invitation(i) => TargetView::Inv(i.invitation, i.inviter),
            BanTarget::LegacyId(x) => TargetView::Id(*x),
        }
    }
}

struct Ctx<'a> {
    owner: Key,
    recs: &'a [&'a Record],
    digests: Vec<Digest>,
    by_kind: Vec<Vec<usize>>,
    keep: Vec<bool>,
    v: &'a mut Verifier,
    /// v1 signatures of legacy records decoded so far, by record index.
    pending: std::cell::RefCell<HashMap<usize, legacy::V1Sig>>,
}

impl Ctx<'_> {
    fn ok(&mut self, i: usize) -> bool {
        let owner = self.owner;
        let rec = self.recs[i];
        match self.pending.get_mut().remove(&i) {
            Some(v1) => {
                let signer = match rec {
                    Record::LegacyBan(l)
                    | Record::LegacyLink(l)
                    | Record::LegacyActivity(l)
                    | Record::LegacyMemberInfo(l)
                    | Record::LegacyMessage(l)
                    | Record::LegacyDirectMessage(l)
                    | Record::LegacyPurge(l)
                    | Record::LegacySecretBlob(l) => l.signer,
                    _ => unreachable!("only legacy records are pending"),
                };
                self.v
                    .ok_with(&self.digests[i], || v1.verify(&owner, &signer))
            }
            None => self.v.ok(&owner, rec, &self.digests[i]),
        }
    }

    /// Keeps a decoded legacy record's v1 signature for its check.
    fn hold(&self, i: usize, v1: &mut Option<legacy::V1Sig>) {
        if let Some(x) = v1.take() {
            self.pending.borrow_mut().insert(i, x);
        }
    }
    fn kind(&self, k: Kind) -> Vec<usize> {
        self.by_kind[k as usize].clone()
    }
}

/// The timing rules of section 2 of the design: `clock_ref` validity, and
/// the effective time of v2 and legacy records.
struct Time {
    clock: Option<u32>,
    legacy_cap: Option<u64>,
}

impl Time {
    fn ref_ok(&self, r: u32) -> bool {
        self.clock.map_or(r == 0, |c| r <= c)
    }
    /// `min(time, clock_ref + 48h)`; `time` with no `Clock`.
    fn eff(&self, time: u64, clock_ref: u32) -> u64 {
        match self.clock {
            Some(_) => time.min(EPOCH_2026_SECS + (clock_ref as u64 + 48) * 3600),
            None => time,
        }
    }
    fn eff_legacy(&self, time: u64) -> u64 {
        self.legacy_cap.map_or(time, |c| time.min(c))
    }
    fn eff_hours(&self, hours: u32, clock_ref: u32) -> u32 {
        match self.clock {
            Some(_) => hours.min(clock_ref.saturating_add(48)),
            None => hours,
        }
    }
}

struct Adm {
    rec: usize,
    digest: Digest,
    member: Key,
    depth: u32,
    parent: Option<usize>,
}

fn admissible_v1(body: &RoomMessageBody, private: bool, versions: &HashSet<u32>) -> bool {
    if !private {
        return true;
    }
    match body {
        RoomMessageBody::Private { secret_version, .. } => versions.contains(secret_version),
        RoomMessageBody::Public { content_type, .. } => *content_type == CONTENT_TYPE_EVENT,
    }
}

fn admissible(body: &MessageBody, private: bool, versions: &HashSet<u32>) -> bool {
    if !private {
        return true;
    }
    match body {
        MessageBody::Private(b) => versions.contains(&b.secret_version),
        MessageBody::Public(b) => b.content_type == CONTENT_TYPE_EVENT,
    }
}

fn sorted_unique<T: Ord>(v: &[T]) -> bool {
    v.windows(2).all(|w| w[0] < w[1])
}

/// `f` over a set of records (no two with one digest). Returns which survive.
pub fn retain(owner: &Key, recs: &[Record], v: &mut Verifier) -> Retained {
    let refs: Vec<&Record> = recs.iter().collect();
    let digests: Vec<Digest> = recs.iter().map(|r| r.digest()).collect();
    retain_refs(owner, &refs, digests, v)
}

/// [`retain`] over borrowed records whose digests are already known.
#[allow(clippy::type_complexity)]
pub fn retain_refs(
    owner: &Key,
    recs: &[&Record],
    digests: Vec<Digest>,
    v: &mut Verifier,
) -> Retained {
    v.bind(owner);
    let mut by_kind = vec![vec![]; 24];
    for (i, r) in recs.iter().enumerate() {
        by_kind[r.kind() as usize].push(i);
    }
    let mut c = Ctx {
        owner: *owner,
        recs,
        digests,
        by_kind,
        keep: vec![false; recs.len()],
        v,
        pending: Default::default(),
    };
    let owner = *owner;

    // ---------------------------------------------------------------- L1
    // Reads: own records only.

    // Configuration: highest valid version, ties by lower signature bytes.
    let mut cands: Vec<(Reverse<u32>, Sig, usize)> = c
        .kind(0)
        .into_iter()
        .filter_map(|i| match &recs[i] {
            Record::Configuration(s) if s.payload.check(&owner) => {
                Some((Reverse(s.payload.version), s.sig, i))
            }
            _ => None,
        })
        .collect();
    cands.sort();
    let mut config = None;
    for (_, _, i) in cands {
        if c.ok(i) {
            c.keep[i] = true;
            if let Record::Configuration(s) = &recs[i] {
                config = Some(s.payload.clone());
            }
            break;
        }
    }
    // Legacy records count only in a room whose owner has converted, that
    // is, signed a valid v2 configuration (decision (p)). Without one the
    // defaults apply to v2 records alone.
    let v2_configured = config.is_some();
    let cfg = config.unwrap_or_else(|| ConfigurationV2::defaults(200));

    // Clock: the maximum, ties by lower signature bytes.
    let mut cands: Vec<(Reverse<u32>, Sig, usize)> = c
        .kind(2)
        .into_iter()
        .filter_map(|i| match &recs[i] {
            Record::Clock(s) => Some((Reverse(s.payload.hours), s.sig, i)),
            _ => None,
        })
        .collect();
    cands.sort();
    let mut clock = None;
    for (Reverse(h), _, i) in cands {
        if c.ok(i) {
            c.keep[i] = true;
            clock = Some(h);
            break;
        }
    }
    let time = Time {
        clock,
        legacy_cap: if cfg.legacy_window_open {
            clock.map(|h| EPOCH_2026_SECS + (h as u64 + 24) * 3600)
        } else {
            Some(EPOCH_2026_SECS + cfg.legacy_window_closed_at as u64 * 3600)
        },
    };

    // Link checkpoint: highest version, ties by lower signature bytes. One
    // keeping more links than the configuration's `max_links` is not valid,
    // so lowering `max_links` below a checkpoint's list drops that
    // checkpoint (and its retirements) until the owner signs a smaller one.
    let mut cands: Vec<(Reverse<u32>, Sig, usize)> = c
        .kind(3)
        .into_iter()
        .filter_map(|i| match &recs[i] {
            Record::Checkpoint(s)
                if s.payload.keep.len() <= cfg.max_links as usize
                    && sorted_unique(&s.payload.keep) =>
            {
                Some((Reverse(s.payload.version), s.sig, i))
            }
            _ => None,
        })
        .collect();
    cands.sort();
    let mut retire_before = 0u32;
    let mut ck_keep: HashSet<Digest> = HashSet::new();
    for (_, _, i) in cands {
        if c.ok(i) {
            c.keep[i] = true;
            if let Record::Checkpoint(s) = &recs[i] {
                retire_before = s.payload.retire_before;
                ck_keep = s.payload.keep.iter().copied().collect();
            }
            break;
        }
    }

    // Upgrade pointer: highest version, ties by lower signature bytes.
    let mut cands: Vec<(Reverse<u32>, Sig, usize)> = c
        .kind(4)
        .into_iter()
        .filter_map(|i| match &recs[i] {
            Record::Upgrade(s) => Some((Reverse(s.payload.version), s.sig, i)),
            _ => None,
        })
        .collect();
    cands.sort();
    for (_, _, i) in cands {
        if c.ok(i) {
            c.keep[i] = true;
            break;
        }
    }

    // Legacy roster: batches by (seq, digest); a batch that would take any
    // kind past that kind's ceiling of distinct attested records, or the
    // kept batches' entries (repeats included) past `roster_entry_budget`,
    // is skipped whole; at most `ROSTER_BATCHES` kept.
    let mut cands: Vec<(u32, Digest, usize)> = c
        .kind(5)
        .into_iter()
        .filter_map(|i| match &recs[i] {
            Record::Roster(s)
                if !s.payload.batch.is_empty()
                    && s.payload.batch.len() <= ceiling::ROSTER_BATCH_ENTRIES
                    && s.payload
                        .batch
                        .windows(2)
                        .all(|w| (w[0].kind, w[0].digest) < (w[1].kind, w[1].digest)) =>
            {
                Some((s.payload.seq, c.digests[i], i))
            }
            _ => None,
        })
        .collect();
    cands.sort();
    let mut roster: HashSet<(LegacyKind, Digest)> = HashSet::new();
    let mut roster_counts: HashMap<LegacyKind, usize> = HashMap::new();
    let mut batches = 0;
    let mut entries = 0usize;
    let entry_budget = roster_entry_budget();
    for (_, _, i) in cands {
        if batches >= ceiling::ROSTER_BATCHES {
            break;
        }
        let Record::Roster(s) = &recs[i] else {
            unreachable!()
        };
        if entries + s.payload.batch.len() > entry_budget {
            continue;
        }
        // Entries already attested by a kept batch cost nothing again.
        let mut add: HashMap<LegacyKind, usize> = HashMap::new();
        for e in &s.payload.batch {
            if !roster.contains(&(e.kind, e.digest)) {
                *add.entry(e.kind).or_default() += 1;
            }
        }
        if add
            .iter()
            .any(|(k, n)| roster_counts.get(k).copied().unwrap_or(0) + n > roster_ceiling(*k))
        {
            continue;
        }
        if !c.ok(i) {
            continue;
        }
        for (k, n) in add {
            *roster_counts.entry(k).or_default() += n;
        }
        roster.extend(s.payload.batch.iter().map(|e| (e.kind, e.digest)));
        entries += s.payload.batch.len();
        batches += 1;
        c.keep[i] = true;
    }
    let legacy_ok = |kind: LegacyKind, d: &Digest| {
        v2_configured && (cfg.legacy_window_open || roster.contains(&(kind, *d)))
    };
    let rostered = |kind: LegacyKind, d: &Digest| roster.contains(&(kind, *d));

    // Secret versions: one per version, the newest `max_secret_versions`.
    let mut cands: Vec<(Reverse<u32>, Sig, usize)> = c
        .kind(6)
        .into_iter()
        .filter_map(|i| match &recs[i] {
            Record::SecretVersion(s) => Some((Reverse(s.payload.version), s.sig, i)),
            _ => None,
        })
        .collect();
    cands.sort();
    let mut versions: HashSet<u32> = HashSet::new();
    for (Reverse(ver), _, i) in cands {
        if versions.len() >= cfg.max_secret_versions as usize {
            break;
        }
        if versions.contains(&ver) || !c.ok(i) {
            continue;
        }
        versions.insert(ver);
        c.keep[i] = true;
    }

    // ---------------------------------------------------------------- L1b
    // Owner bans. Reads: L1 and own records.
    struct BanCand {
        rec: usize,
        target: TargetView,
        eff: u64,
    }
    let reason_ok = |b: &BanP| b.reason.0.len() <= ceiling::BAN_REASON;
    let ban_v2: Vec<usize> = c.kind(7);
    let ban_legacy: Vec<(usize, Key, legacy::LegacyBanView)> = c
        .kind(8)
        .into_iter()
        .filter_map(|i| match &recs[i] {
            Record::LegacyBan(l) if legacy_ok(LegacyKind::Ban, &c.digests[i]) => {
                legacy::decode_ban(&owner, l).map(|mut v| {
                    c.hold(i, &mut v.v1);
                    (i, l.signer, v)
                })
            }
            _ => None,
        })
        .collect();
    let mut ob: Vec<BanCand> = vec![];
    for &i in &ban_v2 {
        let Record::Ban(s) = &recs[i] else { continue };
        let b = &s.payload;
        if !matches!(b.issuer, Issuer::Owner(())) || !reason_ok(b) || !time.ref_ok(b.clock_ref) {
            continue;
        }
        // A v1 id target is for legacy bans only.
        if b.target == BanTarget::Key(owner) || matches!(b.target, BanTarget::LegacyId(_)) {
            continue;
        }
        ob.push(BanCand {
            rec: i,
            target: TargetView::of(&b.target),
            eff: time.eff(b.time, b.clock_ref),
        });
    }
    for (i, signer, v) in &ban_legacy {
        if *signer == owner {
            ob.push(BanCand {
                rec: *i,
                target: TargetView::Id(v.target),
                eff: time.eff_legacy(v.time),
            });
        }
    }
    ob.sort_by(|a, b| (Reverse(a.eff), c.digests[a.rec]).cmp(&(Reverse(b.eff), c.digests[b.rec])));
    let mut banned_keys: HashSet<Key> = HashSet::new();
    let mut banned_ids: HashSet<i64> = HashSet::new();
    let mut banned_invs: HashSet<(Digest, Key)> = HashSet::new();
    let mut kept_ob = 0;
    for bc in ob {
        if kept_ob >= cfg.max_owner_bans {
            break;
        }
        if !c.ok(bc.rec) {
            continue;
        }
        kept_ob += 1;
        c.keep[bc.rec] = true;
        match bc.target {
            TargetView::Key(k) => {
                banned_keys.insert(k);
            }
            TargetView::Id(x) => {
                banned_ids.insert(x);
            }
            TargetView::Inv(d, k) => {
                banned_invs.insert((d, k));
            }
        }
    }

    // ---------------------------------------------------------------- L1c
    // Deputy bans. Reads: L1, L1b and own records.
    let deputy_keys: HashSet<Key> = cfg
        .deputies
        .iter()
        .copied()
        .filter(|k| *k != owner && !banned_keys.contains(k) && !banned_ids.contains(&legacy_id(k)))
        .collect();
    let deputies = |k: &Key| deputy_keys.contains(k);
    let shielded: Vec<Key> = cfg
        .protected
        .iter()
        .chain(cfg.deputies.iter())
        .copied()
        .chain(std::iter::once(owner))
        .collect();
    let shielded_ids: HashSet<i64> = shielded.iter().map(legacy_id).collect();
    let target_ok_for_deputy = |t: &TargetView| match t {
        TargetView::Key(k) => !shielded.contains(k),
        TargetView::Inv(..) => false,
        TargetView::Id(x) => !shielded_ids.contains(x),
    };
    let mut db: Vec<(BanCand, Key)> = vec![];
    for &i in &ban_v2 {
        let Record::Ban(s) = &recs[i] else { continue };
        let b = &s.payload;
        let Issuer::Deputy(d) = b.issuer else {
            continue;
        };
        let t = TargetView::of(&b.target);
        if !deputies(&d)
            || !reason_ok(b)
            || !time.ref_ok(b.clock_ref)
            || !target_ok_for_deputy(&t)
            || matches!(t, TargetView::Id(_))
        {
            continue;
        }
        db.push((
            BanCand {
                rec: i,
                target: t,
                eff: time.eff(b.time, b.clock_ref),
            },
            d,
        ));
    }
    for (i, signer, v) in &ban_legacy {
        let t = TargetView::Id(v.target);
        if deputies(signer) && target_ok_for_deputy(&t) {
            db.push((
                BanCand {
                    rec: *i,
                    target: t,
                    eff: time.eff_legacy(v.time),
                },
                *signer,
            ));
        }
    }
    db.sort_by(|a, b| {
        (Reverse(a.0.eff), c.digests[a.0.rec]).cmp(&(Reverse(b.0.eff), c.digests[b.0.rec]))
    });
    let mut per_deputy: HashMap<Key, u32> = HashMap::new();
    let mut kept_db = 0;
    for (bc, d) in db {
        if kept_db >= cfg.max_deputy_bans {
            break;
        }
        if per_deputy.get(&d).copied().unwrap_or(0) >= cfg.max_bans_per_deputy || !c.ok(bc.rec) {
            continue;
        }
        *per_deputy.entry(d).or_default() += 1;
        kept_db += 1;
        c.keep[bc.rec] = true;
        match bc.target {
            TargetView::Key(k) => {
                banned_keys.insert(k);
            }
            TargetView::Id(x) => {
                banned_ids.insert(x);
            }
            TargetView::Inv(..) => unreachable!("deputies cannot ban invitations"),
        }
    }
    let key_banned = |k: &Key| banned_keys.contains(k) || banned_ids.contains(&legacy_id(k));

    // ---------------------------------------------------------------- L2
    // Links, decided depth by depth from the owner down. Reads: L1, L1b,
    // L1c and links admitted at shallower depths (which stay admitted).
    struct LinkCand {
        rec: usize,
        member: Key,
        inviter: Key,
        /// Accepted: the parent link's digest (None: the owner).
        parent: Option<Digest>,
        seq: u32,
        legacy: bool,
        rostered: bool,
    }
    let retired = |clk: u32, d: &Digest| clk < retire_before && !ck_keep.contains(d);
    let mut lc: Vec<LinkCand> = vec![];
    for i in c.kind(9) {
        let Record::Link(a) = &recs[i] else { continue };
        let inv = &a.invitation.payload;
        let member = a.acceptance.member;
        let inv_d = a.invitation_digest();
        let parent = match &inv.parent {
            Parent::Owner(()) => {
                if inv.inviter != owner {
                    continue;
                }
                None
            }
            #[cfg(test)]
            Parent::Link(d)
                if mutant::get() == mutant::Mutant::NoSelfContained && !c.digests.contains(d) =>
            {
                None
            }
            Parent::Link(d) => Some(*d),
        };
        if a.acceptance.invitation != inv_d
            || !time.clock.map_or(inv.clock == 0, |h| inv.clock <= h)
            || member == owner
            || member == inv.inviter
            || key_banned(&member)
            || banned_invs.contains(&(inv_d, inv.inviter))
            || retired(inv.clock, &c.digests[i])
        {
            continue;
        }
        lc.push(LinkCand {
            rec: i,
            member,
            inviter: inv.inviter,
            parent,
            seq: inv.seq,
            legacy: false,
            rostered: false,
        });
    }
    for i in c.kind(10) {
        let Record::LegacyLink(l) = &recs[i] else {
            continue;
        };
        let d = c.digests[i];
        // The window and the roster first: a legacy link that cannot count
        // is never decoded.
        if !legacy_ok(LegacyKind::Link, &d) {
            continue;
        }
        let Some(mut v) = legacy::decode_link(&owner, l) else {
            continue;
        };
        c.hold(i, &mut v.v1);
        if v.member == owner || v.member == v.inviter || key_banned(&v.member) || retired(0, &d) {
            continue;
        }
        lc.push(LinkCand {
            rec: i,
            member: v.member,
            inviter: v.inviter,
            parent: None,
            seq: 0,
            legacy: true,
            rostered: rostered(LegacyKind::Link, &d),
        });
    }
    let mut acc_children: HashMap<Option<Digest>, Vec<usize>> = HashMap::new();
    let mut leg_children: HashMap<Key, Vec<usize>> = HashMap::new();
    for (j, l) in lc.iter().enumerate() {
        if l.legacy {
            leg_children.entry(l.inviter).or_default().push(j);
        } else {
            acc_children.entry(l.parent).or_default().push(j);
        }
    }
    let mut adm: Vec<Adm> = vec![];
    let mut adm_by_digest: HashMap<Digest, usize> = HashMap::new();
    let mut canonical: HashMap<Key, usize> = HashMap::new();
    let mut children: HashMap<Option<usize>, u32> = HashMap::new();
    let mut per_member: HashMap<Key, u32> = HashMap::new();
    // Legacy links: at most MAX_LEGACY_LINKS in all. Rostered ones (the
    // owner attested them) have the roster's own count; unrostered ones,
    // minted while the window is open, share what the roster leaves, so they
    // can never displace an attested link (decision (q)).
    let legacy_cap = legacy_link_cap();
    let rostered_listed = roster_counts.get(&LegacyKind::Link).copied().unwrap_or(0) as u32;
    let unrostered_budget = legacy_cap.saturating_sub(rostered_listed);
    // Every link but a rostered legacy one (accepted links and unrostered
    // legacy links) shares what `max_links` leaves after the roster's link
    // count, so no link, at any depth, can push an attested legacy link out
    // through the total cap (decision (o)).
    let other_budget = cfg.max_links.saturating_sub(rostered_listed);
    let mut rostered_admitted = 0u32;
    let mut unrostered_admitted = 0u32;
    let mut other_admitted = 0u32;
    let mut frontier: Vec<usize> = vec![]; // admitted at depth d-1, in position order
                                           // Accepted links stop at `max_chain_depth`; legacy links (v1 had no
                                           // depth limit) go as deep as their chains, bounded by their count.
    for depth in 1u32.. {
        // (rostered legacy first, parent position + 1 or 0 for the owner,
        // seq, link digest), candidate, parent
        let mut cands: Vec<((u8, u32, u32, Digest), usize, Option<usize>)> = vec![];
        let accepted_ok_here = depth <= cfg.max_chain_depth;
        let push = |j: usize, parent: Option<usize>, cands: &mut Vec<_>| {
            let l = &lc[j];
            let pp = parent.map_or(0, |p| p as u32 + 1);
            let first = if l.legacy && l.rostered { 0 } else { 1 };
            cands.push(((first, pp, l.seq, c.digests[l.rec]), j, parent));
        };
        if depth == 1 {
            if accepted_ok_here {
                for &j in acc_children.get(&None).map(|v| v.as_slice()).unwrap_or(&[]) {
                    push(j, None, &mut cands);
                }
            }
            for &j in leg_children
                .get(&owner)
                .map(|v| v.as_slice())
                .unwrap_or(&[])
            {
                push(j, None, &mut cands);
            }
        } else {
            for &p in &frontier {
                if accepted_ok_here {
                    let pd = Some(adm[p].digest);
                    for &j in acc_children.get(&pd).map(|v| v.as_slice()).unwrap_or(&[]) {
                        if lc[j].inviter == adm[p].member {
                            push(j, Some(p), &mut cands);
                        }
                    }
                }
                if canonical.get(&adm[p].member) == Some(&p) {
                    for &j in leg_children
                        .get(&adm[p].member)
                        .map(|v| v.as_slice())
                        .unwrap_or(&[])
                    {
                        push(j, Some(p), &mut cands);
                    }
                }
            }
        }
        if cands.is_empty() {
            break;
        }
        cands.sort();
        let mut next = vec![];
        for (_, j, parent) in cands {
            if adm.len() >= cfg.max_links as usize {
                break;
            }
            let l = &lc[j];
            let exempt = l.legacy && l.rostered;
            // Only the links the owner rosters go deeper than
            // `max_chain_depth` (decision (c)).
            if !exempt && !accepted_ok_here {
                continue;
            }
            // The legacy link budgets, in the same admission order as every
            // other L2 limit: group limits of the greedy chooser over the
            // links' own fields, the roster (L1) and shallower links, so
            // they are lawful as the total cap is.
            if l.legacy
                && (if l.rostered {
                    rostered_admitted >= legacy_cap
                } else {
                    unrostered_admitted >= unrostered_budget
                })
            {
                continue;
            }
            if !exempt && other_admitted >= other_budget {
                continue;
            }
            if !exempt {
                let quota = match parent {
                    None => cfg.max_owner_children,
                    Some(p) => cfg.child_quota(&adm[p].digest),
                };
                if children.get(&parent).copied().unwrap_or(0) >= quota
                    || per_member.get(&l.member).copied().unwrap_or(0) >= cfg.max_links_per_member
                {
                    continue;
                }
            }
            if !c.ok(l.rec) {
                continue;
            }
            if l.legacy {
                if l.rostered {
                    rostered_admitted += 1;
                } else {
                    unrostered_admitted += 1;
                }
            }
            if !exempt {
                other_admitted += 1;
                *children.entry(parent).or_default() += 1;
                *per_member.entry(l.member).or_default() += 1;
            }
            let a = adm.len();
            adm_by_digest.insert(c.digests[l.rec], a);
            adm.push(Adm {
                rec: l.rec,
                digest: c.digests[l.rec],
                member: l.member,
                depth,
                parent,
            });
            c.keep[l.rec] = true;
            next.push(a);
        }
        // A key's canonical link is its least (depth, digest) admitted link.
        for &a in &next {
            let k = adm[a].member;
            match canonical.get(&k) {
                Some(&b) if adm[b].depth < depth || adm[b].digest < adm[a].digest => {}
                _ => {
                    canonical.insert(k, a);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    let linked: HashSet<Key> = adm.iter().map(|a| a.member).collect();
    let legacy_linked: HashSet<Key> = adm
        .iter()
        .filter(|a| matches!(recs[a.rec], Record::LegacyLink(_)))
        .map(|a| a.member)
        .collect();

    // ---------------------------------------------------------------- L2b
    // Activity. Reads: L1, L2. Kept for every linked key.
    let mut acts: Vec<(Key, Reverse<u32>, u8, Sig, usize)> = vec![];
    let hours_ok = |h: u32| time.clock.is_none_or(|c| h <= c.saturating_add(24));
    for i in c.kind(11) {
        let Record::Activity(s) = &recs[i] else {
            continue;
        };
        let p = &s.payload;
        if linked.contains(&p.member) && hours_ok(p.hours) {
            acts.push((p.member, Reverse(p.hours), 0, s.sig, i));
        }
    }
    for i in c.kind(12) {
        let Record::LegacyActivity(l) = &recs[i] else {
            continue;
        };
        if !legacy_ok(LegacyKind::Activity, &c.digests[i]) {
            continue;
        }
        let Some(mut v) = legacy::decode_message(&owner, l) else {
            continue;
        };
        c.hold(i, &mut v.v1);
        if v.body.content_len() > ceiling::LEGACY_MESSAGE_SIZE {
            continue;
        }
        let h = (time.eff_legacy(v.time).saturating_sub(EPOCH_2026_SECS) / 3600)
            .min(u32::MAX as u64) as u32;
        // A legacy activity record counts only for a key with an admitted
        // legacy link, so their number is bounded by MAX_LEGACY_LINKS.
        if legacy_linked.contains(&v.author) && hours_ok(h) {
            acts.push((v.author, Reverse(h), 1, v.sig, i));
        }
    }
    acts.sort();
    let mut activity: HashMap<Key, u32> = HashMap::new();
    #[cfg(test)]
    let mut act_rec: HashMap<Key, usize> = HashMap::new();
    for (k, Reverse(h), _, _, i) in acts {
        if activity.contains_key(&k) || !c.ok(i) {
            continue;
        }
        activity.insert(k, h);
        #[cfg(test)]
        act_rec.insert(k, i);
        c.keep[i] = true;
    }
    let rank_act = |k: &Key| match (time.clock, activity.get(k)) {
        (Some(c), Some(h)) => (*h).min(c),
        _ => 0,
    };

    // Restore requests. Reads: L1, L2.
    // Each request attaches to the room where its chain ends: the owner, or
    // an admitted link (its inviter's link there). A member can sign chains
    // of its own keys under its own link, and admit keys of its own to do the
    // same, so requests attached anywhere in the subtree of one of the
    // owner's direct invitees share `RESTORES_PER_SUBTREE` slots; requests
    // attached to the owner (whose invitations only the owner signs) share
    // the rest.
    let subtree_root = |mut a: usize| {
        while let Some(p) = adm[a].parent {
            a = p;
        }
        adm[a].digest
    };
    let mut rr: Vec<(Reverse<u32>, Digest, Digest, Option<Digest>, usize)> = vec![];
    for i in c.kind(13) {
        let Record::Restore(s) = &recs[i] else {
            continue;
        };
        let p = &s.payload;
        if p.chain.is_empty()
            || p.chain.len() > ceiling::MAX_CHAIN_DEPTH as usize
            || !time.ref_ok(p.clock_ref)
        {
            continue;
        }
        let digest_of_link = |cl: &ChainLink| match cl {
            ChainLink::Accepted(a) => Record::Link(a.clone()).digest(),
            ChainLink::Legacy(l) => Record::LegacyLink(l.clone()).digest(),
        };
        let mut ok = true;
        let mut attach: Option<Digest> = None;
        for (n, cl) in p.chain.iter().enumerate() {
            let next = p.chain.get(n + 1);
            if next.is_none() {
                // The admitted link the chain ends under (None: the owner).
                let at = match cl {
                    ChainLink::Accepted(a) => match &a.invitation.payload.parent {
                        Parent::Link(d) => adm_by_digest.get(d).copied(),
                        Parent::Owner(()) => None,
                    },
                    ChainLink::Legacy(l) => canonical.get(&l.signer).copied(),
                };
                attach = at.map(subtree_root);
            }
            ok &= match cl {
                ChainLink::Accepted(a) => {
                    let inv = &a.invitation.payload;
                    a.acceptance.invitation == a.invitation_digest()
                        && match (&inv.parent, next) {
                            (Parent::Link(d), Some(nx)) => {
                                *d == digest_of_link(nx)
                                    && chain_member(&owner, nx) == Some(inv.inviter)
                            }
                            (Parent::Link(d), None) => adm_by_digest
                                .get(d)
                                .is_some_and(|&a| adm[a].member == inv.inviter),
                            (Parent::Owner(()), None) => inv.inviter == owner,
                            (Parent::Owner(()), Some(_)) => false,
                        }
                }
                ChainLink::Legacy(l) => match legacy::decode_link(&owner, l) {
                    None => false,
                    Some(v) => match next {
                        Some(nx) => chain_member(&owner, nx) == Some(v.inviter),
                        None => v.inviter == owner || linked.contains(&v.inviter),
                    },
                },
            };
        }
        if ok {
            rr.push((
                Reverse(time.eff_hours(p.hours, p.clock_ref)),
                c.digests[i],
                digest_of_link(&p.chain[0]),
                attach,
                i,
            ));
        }
    }
    rr.sort();
    let mut restored: HashSet<Digest> = HashSet::new();
    let mut per_attach: HashMap<Digest, usize> = HashMap::new();
    for (_, _, link, attach, i) in rr {
        if restored.len() >= ceiling::RESTORE_REQUESTS {
            break;
        }
        let full = attach.is_some_and(|k| {
            per_attach.get(&k).copied().unwrap_or(0) >= ceiling::RESTORES_PER_SUBTREE
        });
        if full || restored.contains(&link) || !c.ok(i) {
            continue;
        }
        if let Some(k) = attach {
            *per_attach.entry(k).or_default() += 1;
        }
        restored.insert(link);
        c.keep[i] = true;
    }

    // ---------------------------------------------------------------- L3
    // Member bans. Reads: L1, L2, L2b.
    let mut mb: Vec<((u32, Reverse<u64>, Digest), usize, usize, Key, TargetView)> = vec![];
    for &i in &ban_v2 {
        let Record::Ban(s) = &recs[i] else { continue };
        let b = &s.payload;
        let Issuer::Member(m) = &b.issuer else {
            continue;
        };
        let BanTarget::Key(t) = b.target else {
            continue;
        };
        let Some(&a) = adm_by_digest.get(&m.link) else {
            continue;
        };
        if adm[a].member != m.key || t == owner || !reason_ok(b) || !time.ref_ok(b.clock_ref) {
            continue;
        }
        if let Some(c) = time.clock {
            let active = activity
                .get(&m.key)
                .is_some_and(|h| h.saturating_add(cfg.member_ban_activity_window) >= c);
            if !active {
                continue;
            }
        }
        mb.push((
            (
                adm[a].depth,
                Reverse(time.eff(b.time, b.clock_ref)),
                c.digests[i],
            ),
            i,
            a,
            m.key,
            TargetView::Key(t),
        ));
    }
    for (i, signer, v) in &ban_legacy {
        if *signer == owner || deputies(signer) {
            continue;
        }
        let Some(&a) = canonical.get(signer) else {
            continue;
        };
        mb.push((
            (
                adm[a].depth,
                Reverse(time.eff_legacy(v.time)),
                c.digests[*i],
            ),
            *i,
            a,
            *signer,
            TargetView::Id(v.target),
        ));
    }
    mb.sort_by_key(|x| x.0);
    let mut per_issuer: HashMap<Key, u32> = HashMap::new();
    let mut kept_mb: Vec<(usize, TargetView)> = vec![];
    for (_, i, a, k, t) in mb {
        if kept_mb.len() >= cfg.max_member_bans as usize {
            break;
        }
        #[cfg(test)]
        if mutant::get() == mutant::Mutant::PresenceMsgBans
            && !recs
                .iter()
                .any(|r| matches!(r, Record::Message(x) if x.payload.author == k))
        {
            continue;
        }
        if per_issuer.get(&k).copied().unwrap_or(0) >= cfg.max_bans_per_issuer || !c.ok(i) {
            continue;
        }
        *per_issuer.entry(k).or_default() += 1;
        kept_mb.push((a, t));
        c.keep[i] = true;
    }
    // A kept ban removes every entry whose chain passes through its issuer
    // link and then, strictly below it, a link of its target.
    let mut root = vec![false; adm.len()];
    if !kept_mb.is_empty() {
        let mut by_key: HashMap<Key, Vec<usize>> = HashMap::new();
        let mut by_id: HashMap<i64, Vec<usize>> = HashMap::new();
        for (a, x) in adm.iter().enumerate() {
            by_key.entry(x.member).or_default().push(a);
            by_id.entry(legacy_id(&x.member)).or_default().push(a);
        }
        for (issuer, t) in &kept_mb {
            let hits = match t {
                TargetView::Key(k) => by_key.get(k),
                TargetView::Id(x) => by_id.get(x),
                TargetView::Inv(..) => None,
            };
            for &d in hits.map(|v| v.as_slice()).unwrap_or(&[]) {
                let mut cur = adm[d].parent;
                while let Some(p) = cur {
                    if p == *issuer {
                        root[d] = true;
                        break;
                    }
                    cur = adm[p].parent;
                }
            }
        }
    }
    let mut removed = vec![false; adm.len()];
    for a in 0..adm.len() {
        removed[a] = root[a] || adm[a].parent.is_some_and(|p| removed[p]);
    }

    // ---------------------------------------------------------------- L4
    // Members. Reads: L1, L2, L2b, L3. Entries are derived, never records.
    let mut best: HashMap<Key, usize> = HashMap::new();
    for a in 0..adm.len() {
        if !removed[a] {
            best.entry(adm[a].member).or_insert(a);
        }
    }
    let mut cands: Vec<(u32, Reverse<u32>, usize, Key)> = best
        .iter()
        .map(|(k, &a)| (adm[a].depth, Reverse(rank_act(k)), a, *k))
        .collect();
    cands.sort();
    #[cfg(test)]
    if mutant::get() == mutant::Mutant::UpwardRetention {
        cands = mutant::upward_retention(&owner, recs, &cfg, &time, cands);
    }
    cands.truncate(cfg.max_members as usize);
    let mut members: Vec<Key> = cands.iter().map(|x| x.3).collect();
    members.sort();
    let kept: HashSet<Key> = members
        .iter()
        .copied()
        .chain(std::iter::once(owner))
        .collect();
    let mut kept_by_id: HashMap<i64, Vec<Key>> = HashMap::new();
    for k in &kept {
        kept_by_id.entry(legacy_id(k)).or_default().push(*k);
    }
    let no_keys: Vec<Key> = vec![];
    let matching = |x: i64| kept_by_id.get(&x).unwrap_or(&no_keys);

    // ---------------------------------------------------------------- L5
    // Member-dependent records. Reads: L1, L4.
    #[cfg(test)]
    if mutant::get() == mutant::Mutant::SweepAfterL4 {
        // Keep a link only on the chain of a kept member's entry or of a
        // kept member ban's issuer; drop activity of keys left unlinked.
        let kept_keys: HashSet<Key> = cands.iter().map(|x| x.3).collect();
        let mut on_chain = vec![false; adm.len()];
        let mark = |mut a: Option<usize>, on_chain: &mut Vec<bool>| {
            while let Some(x) = a {
                on_chain[x] = true;
                a = adm[x].parent;
            }
        };
        for a in 0..adm.len() {
            if !removed[a] && kept_keys.contains(&adm[a].member) {
                mark(Some(a), &mut on_chain);
            }
        }
        for (a, _) in &kept_mb {
            mark(Some(*a), &mut on_chain);
        }
        let mut linked_after = HashSet::new();
        for a in 0..adm.len() {
            c.keep[adm[a].rec] = on_chain[a];
            if on_chain[a] {
                linked_after.insert(adm[a].member);
            }
        }
        for (k, i) in &act_rec {
            if !linked_after.contains(k) {
                c.keep[*i] = false;
            }
        }
    }
    let private = cfg.privacy_mode == 1;

    // Member info: highest version per kept key, ties by lower signature.
    let mut mi: Vec<(Key, Reverse<u32>, Sig, usize)> = vec![];
    for i in c.kind(14) {
        let Record::MemberInfo(s) = &recs[i] else {
            continue;
        };
        let p = &s.payload;
        if kept.contains(&p.member)
            && p.nickname.size_ok()
            && p.nickname.declared_len() <= cfg.max_nickname_size as usize
        {
            mi.push((p.member, Reverse(p.version), s.sig, i));
        }
    }
    for i in c.kind(15) {
        let Record::LegacyMemberInfo(l) = &recs[i] else {
            continue;
        };
        if !legacy_ok(LegacyKind::MemberInfo, &c.digests[i]) {
            continue;
        }
        if let Some(mut v) = legacy::decode_member_info(&owner, l) {
            c.hold(i, &mut v.v1);
            // v1 bounded nicknames only by a client rule, so a legacy one is
            // held to the ceiling rather than to the room's configured size.
            if kept.contains(&v.member) && v.declared_len <= ceiling::MAX_NICKNAME_SIZE as usize {
                mi.push((v.member, Reverse(v.version), v.sig, i));
            }
        }
    }
    mi.sort();
    let mut with_info: HashSet<Key> = HashSet::new();
    for (k, _, _, i) in mi {
        if with_info.contains(&k) || !c.ok(i) {
            continue;
        }
        with_info.insert(k);
        c.keep[i] = true;
    }

    // Messages (actions included): newest `max_recent_messages` by
    // (effective time, id).
    let mut msgs: Vec<(Reverse<u64>, Digest, usize)> = vec![];
    for i in c.kind(16) {
        let Record::Message(s) = &recs[i] else {
            continue;
        };
        let p = &s.payload;
        if kept.contains(&p.author)
            && time.ref_ok(p.clock_ref)
            && p.body.content_len() <= cfg.max_message_size as usize
            && admissible(&p.body, private, &versions)
        {
            msgs.push((Reverse(time.eff(p.time, p.clock_ref)), c.digests[i], i));
        }
    }
    for i in c.kind(17) {
        let Record::LegacyMessage(l) = &recs[i] else {
            continue;
        };
        if !legacy_ok(LegacyKind::Message, &c.digests[i]) {
            continue;
        }
        if let Some(mut v) = legacy::decode_message(&owner, l) {
            c.hold(i, &mut v.v1);
            if kept.contains(&v.author)
                && v.body.content_len()
                    <= (cfg.max_message_size as usize).min(ceiling::LEGACY_MESSAGE_SIZE)
                && admissible_v1(&v.body, private, &versions)
            {
                msgs.push((Reverse(time.eff_legacy(v.time)), c.digests[i], i));
            }
        }
    }
    msgs.sort();
    let mut n_msgs = 0;
    for (_, _, i) in msgs {
        if n_msgs >= cfg.max_recent_messages {
            break;
        }
        if c.ok(i) {
            n_msgs += 1;
            c.keep[i] = true;
        }
    }

    // DMs. (1) purge envelopes, highest version per kept recipient.
    let mut pe: Vec<(Key, Reverse<u64>, Sig, usize)> = vec![];
    let mut legacy_tokens: HashMap<usize, Vec<[u8; 16]>> = HashMap::new();
    for i in c.kind(20) {
        let Record::Purge(s) = &recs[i] else { continue };
        let p = &s.payload;
        if kept.contains(&p.recipient)
            && p.version >= 1
            && p.purged.len() <= ceiling::PURGE_TOKENS
            && sorted_unique(&p.purged)
        {
            pe.push((p.recipient, Reverse(p.version), s.sig, i));
        }
    }
    for i in c.kind(21) {
        let Record::LegacyPurge(l) = &recs[i] else {
            continue;
        };
        if !legacy_ok(LegacyKind::Purge, &c.digests[i]) {
            continue;
        }
        if let Some(mut v) = legacy::decode_purge(&owner, l) {
            c.hold(i, &mut v.v1);
            if v.tokens.len() > ceiling::LEGACY_PURGE_TOKENS {
                continue;
            }
            // Applies to every kept key its 64-bit id matches.
            for k in matching(v.recipient_id) {
                pe.push((*k, Reverse(v.version), v.sig, i));
            }
            legacy_tokens.insert(i, v.tokens);
        }
    }
    // Admitted per recipient in key order, newest first, within a budget of
    // tokens across all kept envelopes: an envelope that does not fit is
    // skipped and the recipient's next older one is tried (decision (t)).
    pe.sort();
    let budget = purge_token_budget();
    let mut spent = 0usize;
    let mut purged: HashMap<Key, HashSet<[u8; 16]>> = HashMap::new();
    for (k, _, _, i) in pe {
        if purged.contains_key(&k) {
            continue;
        }
        let n = match &recs[i] {
            Record::Purge(s) => s.payload.purged.len(),
            _ => legacy_tokens[&i].len(),
        };
        if spent + n > budget || !c.ok(i) {
            continue;
        }
        spent += n;
        let tokens: HashSet<[u8; 16]> = match &recs[i] {
            Record::Purge(s) => s.payload.purged.iter().map(|t| t.0).collect(),
            _ => legacy_tokens[&i].iter().copied().collect(),
        };
        purged.insert(k, tokens);
        c.keep[i] = true;
    }
    let is_purged = |k: &Key, t: &[u8; 16]| purged.get(k).is_some_and(|s| s.contains(t));

    // (2) drop purged DMs, (3) per-pair cap, (4) global cap, both by
    // (effective time descending, signature).
    #[derive(PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Copy)]
    enum Recipient {
        Key(Key),
        Id(i64),
    }
    let mut dms: Vec<(Reverse<u64>, Sig, Digest, usize, (Key, Recipient))> = vec![];
    for i in c.kind(18) {
        let Record::DirectMessage(s) = &recs[i] else {
            continue;
        };
        let p = &s.payload;
        let token = token_of(&s.sig);
        if kept.contains(&p.sender)
            && kept.contains(&p.recipient)
            && p.sender != p.recipient
            && p.ciphertext.0.len() <= ceiling::DM_CIPHERTEXT
            && time.ref_ok(p.clock_ref)
            && !is_purged(&p.recipient, &token)
        {
            dms.push((
                Reverse(time.eff(p.time, p.clock_ref)),
                s.sig,
                c.digests[i],
                i,
                (p.sender, Recipient::Key(p.recipient)),
            ));
        }
    }
    for i in c.kind(19) {
        let Record::LegacyDirectMessage(l) = &recs[i] else {
            continue;
        };
        if !legacy_ok(LegacyKind::DirectMessage, &c.digests[i]) {
            continue;
        }
        let Some(mut v) = legacy::decode_dm(&owner, l) else {
            continue;
        };
        c.hold(i, &mut v.v1);
        let recips = matching(v.recipient_id);
        if kept.contains(&v.sender)
            && !recips.is_empty()
            && v.sender_id != v.recipient_id
            && v.ciphertext_len <= ceiling::LEGACY_DM_CIPHERTEXT
            && !recips.iter().any(|k| is_purged(k, &v.token))
        {
            dms.push((
                Reverse(time.eff_legacy(v.time)),
                v.sig,
                c.digests[i],
                i,
                (v.sender, Recipient::Id(v.recipient_id)),
            ));
        }
    }
    dms.sort_by_key(|a| (a.0, a.1, a.2));
    let mut per_pair: HashMap<(Key, Recipient), usize> = HashMap::new();
    let mut n_dms = 0;
    for (_, _, _, i, pair) in dms {
        if n_dms >= cfg.max_direct_messages {
            break;
        }
        if per_pair.get(&pair).copied().unwrap_or(0) >= ceiling::DMS_PER_PAIR || !c.ok(i) {
            continue;
        }
        *per_pair.entry(pair).or_default() += 1;
        n_dms += 1;
        c.keep[i] = true;
    }

    // Secret blobs: one per (recipient, kept version), by (version
    // descending, v2 before legacy, recipient bytes, signature), up to
    // `max_secret_blobs`. A v2 blob for a key takes precedence over a legacy
    // blob matching it; a legacy blob is kept while some matching kept key
    // has no v2 blob at its version.
    let mut blobs: Vec<((Reverse<u32>, u8, Vec<u8>, Sig), usize)> = vec![];
    let mut legacy_blob_id: HashMap<usize, i64> = HashMap::new();
    for i in c.kind(22) {
        let Record::SecretBlob(s) = &recs[i] else {
            continue;
        };
        let p = &s.payload;
        if kept.contains(&p.recipient)
            && versions.contains(&p.version)
            && p.ciphertext.0.len() <= ceiling::SECRET_BLOB_CIPHERTEXT
        {
            blobs.push(((Reverse(p.version), 0, p.recipient.0.to_vec(), s.sig), i));
        }
    }
    for i in c.kind(23) {
        let Record::LegacySecretBlob(l) = &recs[i] else {
            continue;
        };
        if !legacy_ok(LegacyKind::SecretBlob, &c.digests[i]) {
            continue;
        }
        if let Some(mut v) = legacy::decode_blob(&owner, l) {
            c.hold(i, &mut v.v1);
            if versions.contains(&v.version)
                && !matching(v.recipient_id).is_empty()
                && v.ciphertext_len <= ceiling::SECRET_BLOB_CIPHERTEXT
            {
                legacy_blob_id.insert(i, v.recipient_id);
                blobs.push((
                    (
                        Reverse(v.version),
                        1,
                        v.recipient_id.to_be_bytes().to_vec(),
                        v.sig,
                    ),
                    i,
                ));
            }
        }
    }
    blobs.sort();
    let mut v2_blob: HashSet<(Key, u32)> = HashSet::new();
    let mut legacy_blob: HashSet<(i64, u32)> = HashSet::new();
    let mut n_blobs = 0;
    for ((Reverse(ver), _, _, _), i) in blobs {
        if n_blobs >= cfg.max_secret_blobs {
            break;
        }
        let ok = match &recs[i] {
            Record::SecretBlob(s) => !v2_blob.contains(&(s.payload.recipient, ver)),
            _ => {
                let x = legacy_blob_id[&i];
                !legacy_blob.contains(&(x, ver))
                    && matching(x).iter().any(|k| !v2_blob.contains(&(*k, ver)))
            }
        };
        if !ok || !c.ok(i) {
            continue;
        }
        match &recs[i] {
            Record::SecretBlob(s) => {
                v2_blob.insert((s.payload.recipient, ver));
            }
            _ => {
                legacy_blob.insert((legacy_blob_id[&i], ver));
            }
        }
        n_blobs += 1;
        c.keep[i] = true;
    }

    Retained {
        keep: c.keep,
        config: cfg,
        clock,
        links: adm.iter().map(|a| (a.rec, a.member, a.depth)).collect(),
        members,
    }
}

/// A DM's purge token: the first 16 bytes of BLAKE3 of its signature, as v1.
pub fn token_of(sig: &Sig) -> [u8; 16] {
    let d = blake3::hash(&sig.0);
    let mut out = [0u8; 16];
    out.copy_from_slice(&d.as_bytes()[..16]);
    out
}

/// Variants of `f` that break a design rule, for the negative law tests.
/// Each makes a lower step delete or hide something an upper step read.
#[cfg(test)]
pub(crate) mod mutant {
    use super::*;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Mutant {
        None,
        /// A member ban counts only if its issuer has a message in the input
        /// (a record the message cap and member eviction delete).
        PresenceMsgBans,
        /// Members are kept only while a kept message or DM needs them.
        UpwardRetention,
        /// An accepted link whose parent is absent is admitted as a root.
        NoSelfContained,
        /// A link sweep after the member cap.
        SweepAfterL4,
    }

    thread_local! {
        static CURRENT: std::cell::Cell<Mutant> = const { std::cell::Cell::new(Mutant::None) };
    }

    pub fn get() -> Mutant {
        CURRENT.with(|c| c.get())
    }

    pub fn set(m: Mutant) {
        CURRENT.with(|c| c.set(m))
    }

    /// The member cap re-run until the members are exactly those a kept
    /// message or DM needs (v1's prune loop, reduced to v2 messages and DMs).
    pub(super) fn upward_retention(
        owner: &Key,
        recs: &[&Record],
        cfg: &ConfigurationV2,
        time: &Time,
        cands: Vec<(u32, Reverse<u32>, usize, Key)>,
    ) -> Vec<(u32, Reverse<u32>, usize, Key)> {
        let cap = |v: &[(u32, Reverse<u32>, usize, Key)]| {
            v.iter()
                .take(cfg.max_members as usize)
                .cloned()
                .collect::<Vec<_>>()
        };
        // As v1's loop: requirements are first read with every candidate kept.
        let mut members = cands;
        loop {
            let k: HashSet<Key> = members.iter().map(|x| x.3).chain([*owner]).collect();
            let mut msgs: Vec<(Reverse<u64>, Key)> = vec![];
            let mut dms: Vec<(Reverse<u64>, Sig, Key, Key)> = vec![];
            for r in recs {
                match r {
                    Record::Message(s) if k.contains(&s.payload.author) => msgs.push((
                        Reverse(time.eff(s.payload.time, s.payload.clock_ref)),
                        s.payload.author,
                    )),
                    Record::DirectMessage(s)
                        if k.contains(&s.payload.sender) && k.contains(&s.payload.recipient) =>
                    {
                        dms.push((
                            Reverse(time.eff(s.payload.time, s.payload.clock_ref)),
                            s.sig,
                            s.payload.sender,
                            s.payload.recipient,
                        ))
                    }
                    _ => {}
                }
            }
            msgs.sort();
            dms.sort();
            let mut req: HashSet<Key> = HashSet::new();
            for (_, a) in msgs.iter().take(cfg.max_recent_messages as usize) {
                req.insert(*a);
            }
            for (_, _, a, b) in dms.iter().take(cfg.max_direct_messages as usize) {
                req.insert(*a);
                req.insert(*b);
            }
            let pruned: Vec<_> = members
                .iter()
                .filter(|x| req.contains(&x.3))
                .cloned()
                .collect();
            let next = cap(&pruned);
            if next == members {
                return members;
            }
            members = next;
        }
    }
}
