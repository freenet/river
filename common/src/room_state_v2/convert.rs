//! v1 to v2 conversion.
//!
//! A room migrates only when its owner converts (decision (p)): the owner
//! signs a v2 configuration, a `Clock`, a v2 record per v1 secret version and
//! roster batches ([`owner_payloads`]), and legacy records count only in a
//! state holding that configuration. [`legacy_records`] wraps every v1
//! record a v1 state holds as a legacy v2 record, naming its signer; any
//! client can run it, and while the legacy window is open the records are
//! valid unrostered. [`deputy_candidates`] lists the deputies the owner is
//! asked to confirm (decision (r)), and [`owner_payloads`] reports what will
//! not carry over.
//!
//! Records whose signer key cannot be found (a ban by an issuer the state no
//! longer holds) cannot be wrapped and are reported in [`Unconverted`].

use super::config::ConfigurationV2;
use super::encoding::{legacy_id, sign_payload, to_cbor, ByteBuf, Digest, Key, Nonce12};
use super::records::*;
use super::retain::{retain, Verifier};
use crate::room_state::member::MemberId;
use crate::room_state::privacy::SealedBytes;
use crate::room_state::ChatRoomStateV1;
use ed25519_dalek::SigningKey;
use std::collections::{BTreeSet, HashMap};

/// A v1 record that could not be wrapped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unconverted {
    pub kind: LegacyKind,
    /// The v1 id whose key the state does not hold.
    pub missing_signer: i64,
}

/// The v1 state's records as legacy v2 records.
pub struct LegacyConversion {
    pub records: Vec<Record>,
    pub unconverted: Vec<Unconverted>,
}

/// Every key the v1 state names, by v1 id: the owner, the members, and the
/// ban evidence (absent members a ban still references).
fn keys_by_id(owner: &Key, v1: &ChatRoomStateV1) -> HashMap<i64, Vec<Key>> {
    let mut m: HashMap<i64, Vec<Key>> = HashMap::new();
    let mut add = |k: Key| {
        let e = m.entry(legacy_id(&k)).or_default();
        if !e.contains(&k) {
            e.push(k);
        }
    };
    add(*owner);
    for r in v1
        .members
        .members
        .iter()
        .chain(v1.ban_evidence.members.iter())
    {
        add(Key::from(&r.member.member_vk));
    }
    m
}

fn wrap(signer: Key, v1_bytes: Vec<u8>) -> Legacy {
    Legacy {
        signer,
        v1: ByteBuf(v1_bytes),
    }
}

/// Wraps every record of `v1` as a legacy v2 record. A record whose v1 id
/// matches several keys is wrapped once per key; `f` keeps only those whose
/// key verifies it.
pub fn legacy_records(owner: &Key, v1: &ChatRoomStateV1) -> LegacyConversion {
    let keys = keys_by_id(owner, v1);
    let mut records = vec![];
    let mut unconverted = vec![];
    let mut emit = |kind: LegacyKind,
                    id: i64,
                    bytes: Vec<u8>,
                    make: fn(Legacy) -> Record,
                    records: &mut Vec<Record>| {
        match keys.get(&id) {
            Some(ks) => {
                for k in ks {
                    records.push(make(wrap(*k, bytes.clone())));
                }
            }
            None => unconverted.push(Unconverted {
                kind,
                missing_signer: id,
            }),
        }
    };
    for m in &v1.members.members {
        emit(
            LegacyKind::Link,
            m.member.invited_by.0 .0,
            to_cbor(m),
            Record::LegacyLink,
            &mut records,
        );
    }
    for b in &v1.bans.0 {
        emit(
            LegacyKind::Ban,
            b.banned_by.0 .0,
            to_cbor(b),
            Record::LegacyBan,
            &mut records,
        );
    }
    for i in &v1.member_info.member_info {
        emit(
            LegacyKind::MemberInfo,
            i.member_info.member_id.0 .0,
            to_cbor(i),
            Record::LegacyMemberInfo,
            &mut records,
        );
    }
    let owner_id = legacy_id(owner);
    // Each author's newest message within the legacy content bound is also
    // its activity record.
    let mut newest: HashMap<i64, usize> = HashMap::new();
    for (n, m) in v1.recent_messages.messages.iter().enumerate() {
        emit(
            LegacyKind::Message,
            m.message.author.0 .0,
            to_cbor(m),
            Record::LegacyMessage,
            &mut records,
        );
        let a = m.message.author.0 .0;
        // The owner needs no activity record; nor does a message over the
        // legacy content bound carry one.
        if a == owner_id
            || m.message.content.content_len() > super::config::ceiling::LEGACY_MESSAGE_SIZE
        {
            continue;
        }
        let newer = newest
            .get(&a)
            .is_none_or(|&o| v1.recent_messages.messages[o].message.time < m.message.time);
        if newer {
            newest.insert(a, n);
        }
    }
    let mut newest: Vec<(i64, usize)> = newest.into_iter().collect();
    newest.sort();
    for (a, n) in newest {
        let m = &v1.recent_messages.messages[n];
        emit(
            LegacyKind::Activity,
            a,
            to_cbor(m),
            Record::LegacyActivity,
            &mut records,
        );
    }
    for d in &v1.direct_messages.messages {
        emit(
            LegacyKind::DirectMessage,
            d.message.sender.0 .0,
            to_cbor(d),
            Record::LegacyDirectMessage,
            &mut records,
        );
    }
    for p in &v1.direct_messages.purges {
        emit(
            LegacyKind::Purge,
            p.recipient_id.0 .0,
            to_cbor(p),
            Record::LegacyPurge,
            &mut records,
        );
    }
    for b in &v1.secrets.encrypted_secrets {
        emit(
            LegacyKind::SecretBlob,
            owner_id,
            to_cbor(b),
            Record::LegacySecretBlob,
            &mut records,
        );
    }
    LegacyConversion {
        records,
        unconverted,
    }
}

fn sealed(s: &SealedBytes) -> Sealed {
    match s {
        SealedBytes::Public { value } => Sealed::Public(ByteBuf(value.clone())),
        SealedBytes::Private {
            ciphertext,
            nonce,
            secret_version,
            declared_len_bytes,
        } => Sealed::Private(SealedPrivate {
            ciphertext: ByteBuf(ciphertext.clone()),
            nonce: Nonce12(*nonce),
            secret_version: *secret_version,
            declared_len: *declared_len_bytes,
        }),
    }
}

/// What the converting owner signs.
#[derive(Debug, Clone)]
pub struct OwnerPayloads {
    pub configuration: ConfigurationV2,
    pub clock: ClockP,
    pub secret_versions: Vec<SecretVersionP>,
    pub roster: Vec<RosterP>,
}

impl OwnerPayloads {
    pub fn sign(&self, owner: &SigningKey) -> Vec<Record> {
        let o = Key::from(&owner.verifying_key());
        let mut out = vec![
            Record::Configuration(Signed {
                sig: sign_payload(tag::CONFIGURATION, &o, &self.configuration, owner),
                payload: self.configuration.clone(),
            }),
            Record::Clock(Signed {
                sig: sign_payload(tag::CLOCK, &o, &self.clock, owner),
                payload: self.clock.clone(),
            }),
        ];
        for p in &self.secret_versions {
            out.push(Record::SecretVersion(Signed {
                sig: sign_payload(tag::SECRET_VERSION, &o, p, owner),
                payload: p.clone(),
            }));
        }
        for p in &self.roster {
            out.push(Record::Roster(Signed {
                sig: sign_payload(tag::ROSTER, &o, p, owner),
                payload: p.clone(),
            }));
        }
        out
    }
}

/// A key the owner may confirm as a v2 deputy, and who named it in v1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeputyCandidate {
    pub key: Key,
    /// The keys whose v1 deputy list names it: the owner (appointed room
    /// wide in v1) or a member (appointed for that member's subtree in v1).
    pub listed_by: Vec<Key>,
}

/// The deputies to propose to the converting owner: every current, unbanned
/// member named by the owner's own v1 deputy list or by the list of a
/// current, unbanned member. v2 deputies act room wide, so none is made one
/// without the owner's confirmation (decision (r)).
pub fn deputy_candidates(owner: &Key, v1: &ChatRoomStateV1) -> Vec<DeputyCandidate> {
    let banned: BTreeSet<MemberId> = v1.bans.0.iter().map(|b| b.ban.banned_user).collect();
    let members: HashMap<MemberId, Key> = v1
        .members
        .members
        .iter()
        .filter(|m| !banned.contains(&m.member.id()))
        .map(|m| (m.member.id(), Key::from(&m.member.member_vk)))
        .collect();
    let owner_id = MemberId(freenet_scaffold::util::FastHash(legacy_id(owner)));
    let mut out: std::collections::BTreeMap<Key, BTreeSet<Key>> = Default::default();
    for i in &v1.member_info.member_info {
        let appointer = if i.member_info.member_id == owner_id {
            *owner
        } else {
            match members.get(&i.member_info.member_id) {
                Some(k) => *k,
                None => continue,
            }
        };
        for d in &i.member_info.deputies {
            if let Some(k) = members.get(d).filter(|k| *k != owner) {
                out.entry(*k).or_default().insert(appointer);
            }
        }
    }
    out.into_iter()
        .map(|(key, by)| DeputyCandidate {
            key,
            listed_by: by.into_iter().collect(),
        })
        .collect()
}

/// Why a conversion cannot go ahead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvertError {
    /// The v1 room name is longer than v2 allows (`max`).
    NameTooLong { len: usize, max: usize },
    /// The v1 room description is longer than v2 allows (`max`).
    DescriptionTooLong { len: usize, max: usize },
    /// More confirmed deputies or protected keys than a configuration holds.
    TooManyAuthorityKeys,
    /// The owner was listed as a deputy.
    OwnerAsDeputy,
    /// The configuration fails [`ConfigurationV2::check`] for another reason.
    InvalidConfiguration,
}

/// The owner's v2 configuration for `v1`: [`ConfigurationV2::from_v1`],
/// version 1, the v1 room name and description, the legacy window open, and
/// exactly the `deputies` and `protected` keys the owner confirmed (deputy
/// bans cannot target a protected key; put the Invite Bot there). A v1 name
/// or description longer than v2 allows is an error, never truncated: a
/// private one is sealed and cannot be.
pub fn owner_configuration(
    owner: &Key,
    v1: &ChatRoomStateV1,
    deputies: &[Key],
    protected: &[Key],
) -> Result<ConfigurationV2, ConvertError> {
    use super::config::ceiling::*;
    let cfg = &v1.configuration.configuration;
    let mut c = ConfigurationV2::from_v1(cfg);
    c.version = 1;
    c.display_name = sealed(&cfg.display.name);
    c.display_description = cfg.display.description.as_ref().map(sealed);
    c.legacy_window_open = true;
    let name = c.display_name.declared_len();
    if name > c.max_room_name as usize {
        return Err(ConvertError::NameTooLong {
            len: name,
            max: MAX_ROOM_NAME as usize,
        });
    }
    if let Some(d) = &c.display_description {
        if d.declared_len() > c.max_room_description as usize {
            return Err(ConvertError::DescriptionTooLong {
                len: d.declared_len(),
                max: MAX_ROOM_DESCRIPTION as usize,
            });
        }
    }
    let sorted = |v: &[Key]| -> Vec<Key> {
        let s: BTreeSet<Key> = v.iter().copied().collect();
        s.into_iter().collect()
    };
    c.deputies = sorted(deputies);
    c.protected = sorted(protected);
    if c.deputies.contains(owner) {
        return Err(ConvertError::OwnerAsDeputy);
    }
    if c.deputies.len() > AUTHORITY_KEYS || c.protected.len() > AUTHORITY_KEYS {
        return Err(ConvertError::TooManyAuthorityKeys);
    }
    if !c.check(owner) {
        return Err(ConvertError::InvalidConfiguration);
    }
    Ok(c)
}

/// Why a v1 record does not carry over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DropReason {
    /// Its signer's key is not in the v1 state ([`Unconverted`]).
    MissingSigner,
    /// Its v1 bytes do not decode as a record of this room.
    Undecodable,
    /// Its v1 signature fails the strict check (decision (k)).
    Signature,
    /// Larger than a legacy bound (content, nickname, purge tokens, blob).
    TooLarge,
    /// A legacy link beyond the 300 the owner rosters (decision (q)).
    LegacyLinkCap,
    /// Not kept by retention: a cap, a ban, membership, an invite chain
    /// that no longer reaches the owner, or a newer record superseding it.
    Retention,
}

/// Per kind: how many v1 records were offered, how many carry over, and
/// why the rest do not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KindReport {
    pub total: usize,
    pub kept: usize,
    pub dropped: std::collections::BTreeMap<DropReason, usize>,
}

/// What a conversion keeps, for the owner's client to show before it signs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConversionReport {
    pub kinds: std::collections::BTreeMap<LegacyKind, KindReport>,
}

/// The owner's records and what they keep.
#[derive(Debug, Clone)]
pub struct Conversion {
    pub payloads: OwnerPayloads,
    pub report: ConversionReport,
}

/// The legacy links to roster: at most the legacy link cap, ranked as the
/// member cap ranks members (depth, then the newest v1 message, then link
/// digest), and only links whose inviter is the owner or an inviter whose
/// own link is chosen.
fn ranked_legacy_links(owner: &Key, v1: &ChatRoomStateV1, legacy: &[Record]) -> BTreeSet<Digest> {
    let mut by_member: HashMap<Key, (Key, Digest)> = HashMap::new();
    for r in legacy {
        if let Record::LegacyLink(l) = r {
            if let Some(v) = super::legacy::decode_link(owner, l) {
                let d = r.digest();
                by_member
                    .entry(v.member)
                    .and_modify(|e| {
                        if d < e.1 {
                            *e = (v.inviter, d);
                        }
                    })
                    .or_insert((v.inviter, d));
            }
        }
    }
    let mut newest: HashMap<i64, u64> = HashMap::new();
    for m in &v1.recent_messages.messages {
        let t = m
            .message
            .time
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let e = newest.entry(m.message.author.0 .0).or_default();
        *e = (*e).max(t);
    }
    // Depth by walking inviters to the owner (None: no chain to the owner).
    let mut depth: HashMap<Key, Option<u32>> = HashMap::new();
    fn depth_of(
        k: &Key,
        owner: &Key,
        by_member: &HashMap<Key, (Key, Digest)>,
        depth: &mut HashMap<Key, Option<u32>>,
    ) -> Option<u32> {
        let mut path = vec![];
        let mut cur = *k;
        let base = loop {
            if cur == *owner {
                break Some(0);
            }
            if let Some(d) = depth.get(&cur) {
                break *d;
            }
            if path.contains(&cur) || path.len() > by_member.len() {
                break None;
            }
            path.push(cur);
            match by_member.get(&cur) {
                Some((inv, _)) => cur = *inv,
                None => break None,
            }
        };
        let mut d = base;
        for x in path.iter().rev() {
            d = d.map(|v| v + 1);
            depth.insert(*x, d);
        }
        depth.get(k).copied().flatten()
    }
    let mut ranked: Vec<(u32, std::cmp::Reverse<u64>, Digest, Key, Key)> = vec![];
    for (m, (inv, d)) in &by_member {
        if let Some(dep) = depth_of(m, owner, &by_member, &mut depth) {
            let act = newest.get(&legacy_id(m)).copied().unwrap_or(0);
            ranked.push((dep, std::cmp::Reverse(act), *d, *m, *inv));
        }
    }
    ranked.sort();
    let cap = super::retain::legacy_link_cap() as usize;
    let mut chosen_members: BTreeSet<Key> = BTreeSet::new();
    let mut out = BTreeSet::new();
    for (_, _, d, m, inv) in ranked {
        if out.len() >= cap {
            break;
        }
        if inv == *owner || chosen_members.contains(&inv) {
            chosen_members.insert(m);
            out.insert(d);
        }
    }
    out
}

/// The owner's records for `v1` at `clock_hours`, with the deputies and
/// protected keys the owner confirmed ([`owner_configuration`]): its
/// configuration and `Clock`, a v2 record per v1 secret version, and roster
/// batches (one kind per batch) attesting exactly the legacy records `f`
/// keeps under them. The legacy links attested are the
/// [`legacy_link_cap`](super::retain::legacy_link_cap) best ranked by depth
/// and activity, so the owner's roster, not admission order, decides who
/// stays when a room holds more. Also reports what does not carry over.
pub fn owner_payloads(
    owner: &Key,
    v1: &ChatRoomStateV1,
    legacy: &[Record],
    clock_hours: u32,
    deputies: &[Key],
    protected: &[Key],
) -> Result<Conversion, ConvertError> {
    let configuration = owner_configuration(owner, v1, deputies, protected)?;
    let clock = ClockP { hours: clock_hours };
    let secret_versions: Vec<SecretVersionP> = v1
        .secrets
        .versions
        .iter()
        .map(|v| SecretVersionP {
            version: v.record.version,
            cipher: 0,
        })
        .collect();
    let links = ranked_legacy_links(owner, v1, legacy);
    // Decide which legacy records survive with the owner's records and the
    // chosen links rostered. The owner's records are signed later; here
    // they are trusted.
    let draft = OwnerPayloads {
        configuration: configuration.clone(),
        clock: clock.clone(),
        secret_versions: secret_versions.clone(),
        roster: batches(
            LegacyKind::Link,
            links
                .iter()
                .map(|d| RosterEntry {
                    kind: LegacyKind::Link,
                    digest: *d,
                })
                .collect(),
            0,
        ),
    };
    let stand_in = SigningKey::from_bytes(&[0x42; 32]);
    let mut all = draft.sign(&stand_in);
    let n_owner = all.len();
    all.extend(legacy.iter().cloned());
    let mut v = Verifier::new();
    v.trust(owner, all[..n_owner].iter().map(|r| r.digest()));
    let kept = retain(owner, &all, &mut v);
    let mut by_kind: HashMap<LegacyKind, Vec<RosterEntry>> = HashMap::new();
    let mut report = ConversionReport::default();
    for (r, k) in all[n_owner..].iter().zip(kept.keep[n_owner..].iter()) {
        let Some(kind) = r.legacy_kind() else {
            continue;
        };
        let e = report.kinds.entry(kind).or_default();
        e.total += 1;
        if *k {
            e.kept += 1;
            by_kind.entry(kind).or_default().push(RosterEntry {
                kind,
                digest: r.digest(),
            });
        } else {
            *e.dropped.entry(drop_reason(owner, r, &links)).or_default() += 1;
        }
    }
    let mut roster = vec![];
    for kind in LEGACY_KINDS {
        let mut entries = by_kind.remove(&kind).unwrap_or_default();
        entries.sort_by_key(|e| e.digest);
        let seq = roster.len() as u32;
        roster.extend(batches(kind, entries, seq));
    }
    Ok(Conversion {
        payloads: OwnerPayloads {
            configuration,
            clock,
            secret_versions,
            roster,
        },
        report,
    })
}

/// Roster batches of one kind, numbered from `seq`.
fn batches(_kind: LegacyKind, entries: Vec<RosterEntry>, seq: u32) -> Vec<RosterP> {
    entries
        .chunks(super::config::ceiling::ROSTER_BATCH_ENTRIES)
        .enumerate()
        .map(|(n, chunk)| RosterP {
            seq: seq + n as u32,
            batch: chunk.to_vec(),
        })
        .collect()
}

/// Adds the [`Unconverted`] records to a report (they never reach `f`).
pub fn add_unconverted(report: &mut ConversionReport, unconverted: &[Unconverted]) {
    for u in unconverted {
        let e = report.kinds.entry(u.kind).or_default();
        e.total += 1;
        *e.dropped.entry(DropReason::MissingSigner).or_default() += 1;
    }
}

/// Why `f` dropped a legacy record, as far as its own bytes tell.
fn drop_reason(owner: &Key, r: &Record, chosen_links: &BTreeSet<Digest>) -> DropReason {
    use super::config::ceiling::*;
    use super::legacy::*;
    let large = match r {
        Record::LegacyLink(l) => decode_link(owner, l).map(|_| false),
        Record::LegacyBan(l) => decode_ban(owner, l).map(|_| false),
        Record::LegacyActivity(l) | Record::LegacyMessage(l) => {
            decode_message(owner, l).map(|v| v.body.content_len() > LEGACY_MESSAGE_SIZE)
        }
        Record::LegacyMemberInfo(l) => {
            decode_member_info(owner, l).map(|v| v.declared_len > MAX_NICKNAME_SIZE as usize)
        }
        Record::LegacyDirectMessage(l) => {
            decode_dm(owner, l).map(|v| v.ciphertext_len > LEGACY_DM_CIPHERTEXT)
        }
        Record::LegacyPurge(l) => {
            decode_purge(owner, l).map(|v| v.tokens.len() > LEGACY_PURGE_TOKENS)
        }
        Record::LegacySecretBlob(l) => {
            decode_blob(owner, l).map(|v| v.ciphertext_len > SECRET_BLOB_CIPHERTEXT)
        }
        _ => None,
    };
    match large {
        None => DropReason::Undecodable,
        Some(_) if !verify(owner, r) => DropReason::Signature,
        Some(true) => DropReason::TooLarge,
        Some(false)
            if matches!(r, Record::LegacyLink(_)) && !chosen_links.contains(&r.digest()) =>
        {
            DropReason::LegacyLinkCap
        }
        Some(false) => DropReason::Retention,
    }
}
