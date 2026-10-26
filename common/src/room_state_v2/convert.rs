//! v1 to v2 conversion.
//!
//! [`legacy_records`] wraps every v1 record a v1 state holds as a legacy v2
//! record, naming its signer. Any client can run it on the v1 state it holds;
//! while the legacy window is open the records are valid without the owner.
//! [`owner_payloads`] adds what the owner signs when it converts first: a v2
//! configuration from the v1 one, a `Clock`, a v2 record per v1 secret
//! version, and roster batches attesting the legacy records `f` keeps.
//!
//! Records whose signer key cannot be found (a ban by an issuer the state no
//! longer holds) cannot be wrapped and are reported in [`Unconverted`].

use super::config::ConfigurationV2;
use super::encoding::{legacy_id, sign_payload, to_cbor, ByteBuf, Key, Nonce12};
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
    // Each author's newest message is also its activity record.
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
    let owner_id = legacy_id(owner);
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

/// The v1 configuration as a legacy record, for a room whose owner does not
/// convert (it then decides the room's configuration through
/// [`ConfigurationV2::from_v1`]).
pub fn legacy_configuration(owner: &Key, v1: &ChatRoomStateV1) -> Record {
    Record::LegacyConfiguration(wrap(*owner, to_cbor(&v1.configuration)))
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

/// The owner's v2 configuration for `v1`: [`ConfigurationV2::from_v1`],
/// version 1, the legacy window open, and as deputies every key a v1
/// member-info deputy list names (the owner's or a member's) that is a
/// current member.
pub fn owner_configuration(owner: &Key, v1: &ChatRoomStateV1) -> ConfigurationV2 {
    let cfg = &v1.configuration.configuration;
    let mut c = ConfigurationV2::from_v1(cfg);
    c.version = 1;
    c.display_name = sealed(&cfg.display.name);
    c.display_description = cfg.display.description.as_ref().map(sealed);
    c.legacy_window_open = true;
    let members: HashMap<MemberId, Key> = v1
        .members
        .members
        .iter()
        .map(|m| (m.member.id(), Key::from(&m.member.member_vk)))
        .collect();
    let deputies: BTreeSet<Key> = v1
        .member_info
        .member_info
        .iter()
        .flat_map(|i| i.member_info.deputies.iter())
        .filter_map(|id| members.get(id).copied())
        .filter(|k| k != owner)
        .collect();
    c.deputies = deputies
        .into_iter()
        .take(super::config::ceiling::AUTHORITY_KEYS)
        .collect();
    c
}

/// The owner's records for `v1` at `clock_hours`: its configuration and
/// `Clock`, a v2 record per v1 secret version, and roster batches (one kind
/// per batch) attesting exactly the legacy records `f` keeps under that
/// configuration, so no batch can exceed its kind's ceiling.
pub fn owner_payloads(
    owner: &Key,
    v1: &ChatRoomStateV1,
    legacy: &[Record],
    clock_hours: u32,
) -> OwnerPayloads {
    let configuration = owner_configuration(owner, v1);
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
    // Decide which legacy records survive with the owner's records in place
    // (window open, so no roster is needed yet). The owner's records are
    // signed later; here they are trusted.
    let draft = OwnerPayloads {
        configuration: configuration.clone(),
        clock: clock.clone(),
        secret_versions: secret_versions.clone(),
        roster: vec![],
    };
    let stand_in = SigningKey::from_bytes(&[0x42; 32]);
    let mut all = draft.sign(&stand_in);
    let n_owner = all.len();
    all.extend(legacy.iter().cloned());
    let mut v = Verifier::new();
    v.trust(all[..n_owner].iter().map(|r| r.digest()));
    let kept = retain(owner, &all, &mut v);
    let mut by_kind: HashMap<LegacyKind, Vec<RosterEntry>> = HashMap::new();
    for (r, k) in all.iter().zip(kept.keep.iter()) {
        if let (true, Some(kind)) = (*k, r.legacy_kind()) {
            by_kind.entry(kind).or_default().push(RosterEntry {
                kind,
                digest: r.digest(),
            });
        }
    }
    let mut roster = vec![];
    for kind in LEGACY_KINDS {
        let mut entries = by_kind.remove(&kind).unwrap_or_default();
        entries.sort_by_key(|e| e.digest);
        for chunk in entries.chunks(super::config::ceiling::ROSTER_BATCH_ENTRIES) {
            roster.push(RosterP {
                seq: roster.len() as u32,
                batch: chunk.to_vec(),
            });
        }
    }
    OwnerPayloads {
        configuration,
        clock,
        secret_versions,
        roster,
    }
}
