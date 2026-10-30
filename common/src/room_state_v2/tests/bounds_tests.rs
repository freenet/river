//! Negative and cost tests: every clause of `ConfigurationV2::check` fails
//! when broken, `from_v1` clamps, every `validate_state` refusal fires, and
//! the signature-check counts of decision (n) hold (`Verifier::checks`).

use crate::room_state::configuration::Configuration as V1Configuration;
use crate::room_state::privacy::PrivacyMode;
use crate::room_state_v2::bench::{largest_state, owner_key, Caps};
use crate::room_state_v2::config::{ceiling, ConfigurationV2};
use crate::room_state_v2::encoding::{sign_payload, ByteBuf, Bytes32, Digest, Key, Nonce12, Sig};
use crate::room_state_v2::records::*;
use crate::room_state_v2::retain::{retain, signature_ok};
use crate::room_state_v2::state::{
    apply_delta, max_state_bytes, normalize, validate_state, within_kind_counts, Invalid,
    RoomStateV2,
};
use crate::room_state_v2::Verifier;
use ed25519_dalek::SigningKey;

// ------------------------------------------------------------------ helpers

fn owner() -> Key {
    Key::from(&owner_key().verifying_key())
}

/// A deterministic key that is not the owner's or any other domain's.
fn skey(domain: u8, i: u32) -> SigningKey {
    let mut seed = [0xB7u8; 32];
    seed[0] = domain;
    seed[1..5].copy_from_slice(&i.to_le_bytes());
    SigningKey::from_bytes(&seed)
}

fn kof(s: &SigningKey) -> Key {
    Key::from(&s.verifying_key())
}

fn zero_sig() -> Sig {
    Sig([0; 64])
}

fn signed<P: serde::Serialize>(t: u8, payload: P, by: &SigningKey) -> Signed<P> {
    Signed {
        sig: sign_payload(t, &owner(), &payload, by),
        payload,
    }
}

fn config_record(cfg: &ConfigurationV2) -> Record {
    Record::Configuration(signed(tag::CONFIGURATION, cfg.clone(), &owner_key()))
}

fn valid_config(version: u32) -> ConfigurationV2 {
    let mut c = ConfigurationV2::defaults(100);
    c.version = version;
    c
}

/// A link from `inviter` to `member`. With `sign` false every signature is
/// zero but the acceptance still names the invitation's digest, so the link
/// is structurally valid and costs nothing to build.
fn link(
    inviter: &SigningKey,
    parent: Parent,
    seq: u32,
    member: &SigningKey,
    sign: bool,
) -> AcceptedLink {
    let o = owner();
    let k0 = skey(0xE0, seq);
    let ip = InvitationP {
        inviter: kof(inviter),
        invite_key: kof(&k0),
        parent,
        seq,
        clock: 0,
    };
    let invitation = if sign {
        signed(tag::INVITATION, ip, inviter)
    } else {
        Signed {
            payload: ip,
            sig: zero_sig(),
        }
    };
    let acceptance = AcceptanceP {
        invitation: crate::room_state_v2::encoding::digest_of(&invitation),
        member: kof(member),
    };
    if sign {
        AcceptedLink {
            invite_key_sig: sign_payload(tag::ACCEPTANCE, &o, &acceptance, &k0),
            member_sig: sign_payload(tag::ACCEPTANCE, &o, &acceptance, member),
            invitation,
            acceptance,
        }
    } else {
        AcceptedLink {
            invite_key_sig: zero_sig(),
            member_sig: zero_sig(),
            invitation,
            acceptance,
        }
    }
}

fn activity(member: &SigningKey, hours: u32) -> Record {
    Record::Activity(signed(
        tag::ACTIVITY,
        ActivityP {
            member: kof(member),
            hours,
        },
        member,
    ))
}

fn clock_record(hours: u32, sig: Sig) -> Record {
    Record::Clock(Signed {
        payload: ClockP { hours },
        sig,
    })
}

/// A restore request over `chain` (retired link first), signed by `by`.
fn restore(chain: Vec<AcceptedLink>, hours: u32, by: &SigningKey) -> Record {
    let p = RestoreP {
        chain: chain.into_iter().map(ChainLink::Accepted).collect(),
        hours,
        clock_ref: 0,
    };
    Record::Restore(signed(tag::RESTORE, p, by))
}

/// A restore chain of `n` real, validly signed links ending at the owner.
fn owner_chain(n: usize, salt: u32) -> (Vec<AcceptedLink>, SigningKey) {
    let osk = owner_key();
    let mut chain = vec![];
    let mut inviter = osk;
    let mut parent = Parent::Owner(());
    for j in 0..n as u32 {
        let m = skey(0xC0, salt * 100 + j);
        let l = link(&inviter, parent, j, &m, true);
        parent = Parent::Link(Record::Link(l.clone()).digest());
        chain.push(l);
        inviter = m;
    }
    chain.reverse();
    (chain, inviter)
}

fn empty_state() -> RoomStateV2 {
    RoomStateV2::from_records(vec![])
}

fn validate(bytes: &[u8]) -> (Result<RoomStateV2, Invalid>, usize) {
    let mut v = Verifier::new();
    let r = validate_state(&owner(), bytes, &mut v);
    (r, v.checks)
}

// ------------------------------------------------------- 1. check negatives

type Break = fn(&mut ConfigurationV2);

fn case(name: &'static str, f: Break) -> (&'static str, Break) {
    (name, f)
}

fn sorted_keys(n: usize) -> Vec<Key> {
    let mut v: Vec<Key> = (0..n as u32).map(|i| kof(&skey(0xD0, i))).collect();
    v.sort();
    v
}

fn sorted_digests(n: usize) -> Vec<(Digest, u32)> {
    (0..n).map(|i| (Bytes32([i as u8 + 1; 32]), 1)).collect()
}

fn private_sealed(ciphertext: usize, declared: u32) -> Sealed {
    Sealed::Private(SealedPrivate {
        ciphertext: ByteBuf(vec![1; ciphertext]),
        nonce: Nonce12([0; 12]),
        secret_version: 1,
        declared_len: declared,
    })
}

#[test]
fn check_accepts_the_base_and_the_boundaries() {
    let o = owner();
    assert!(valid_config(1).check(&o));
    // Every ceiling met exactly, and the list limits met exactly.
    let mut c = valid_config(1);
    c.max_members = ceiling::MAX_MEMBERS;
    c.max_links = ceiling::MAX_LINKS;
    c.max_links_per_member = ceiling::MAX_LINKS_PER_MEMBER;
    c.max_children_per_link = ceiling::MAX_CHILDREN_PER_LINK;
    c.max_owner_children = ceiling::MAX_CHILDREN_PER_LINK;
    c.max_chain_depth = ceiling::MAX_CHAIN_DEPTH;
    c.max_owner_bans = ceiling::MAX_OWNER_BANS;
    c.max_deputy_bans = ceiling::MAX_DEPUTY_BANS;
    c.max_bans_per_deputy = ceiling::MAX_BANS_PER_DEPUTY;
    c.max_member_bans = ceiling::MAX_MEMBER_BANS;
    c.max_bans_per_issuer = ceiling::MAX_BANS_PER_ISSUER;
    c.max_secret_blobs = ceiling::MAX_SECRET_BLOBS;
    c.max_secret_versions = ceiling::MAX_SECRET_VERSIONS;
    c.max_recent_messages = ceiling::MAX_RECENT_MESSAGES;
    c.max_message_size = ceiling::MAX_MESSAGE_SIZE;
    c.max_direct_messages = ceiling::MAX_DIRECT_MESSAGES;
    c.max_room_description = ceiling::MAX_ROOM_DESCRIPTION;
    c.max_room_name = ceiling::MAX_ROOM_NAME;
    c.max_nickname_size = ceiling::MAX_NICKNAME_SIZE;
    c.deputies = sorted_keys(ceiling::AUTHORITY_KEYS)
        .into_iter()
        .filter(|k| *k != o)
        .collect();
    c.protected = sorted_keys(ceiling::AUTHORITY_KEYS);
    let mut ex = sorted_digests(ceiling::CHILD_EXCEPTIONS);
    ex[0].1 = ceiling::MAX_CHILDREN_PER_LINK;
    c.child_exceptions = ex;
    assert!(
        c.check(&o),
        "a configuration exactly at every limit is valid"
    );
    // Names exactly at their configured maximum.
    let mut c = valid_config(1);
    c.display_name = Sealed::Public(ByteBuf(vec![b'n'; c.max_room_name as usize]));
    c.display_description = Some(Sealed::Public(ByteBuf(vec![
        b'd';
        c.max_room_description as usize
    ])));
    assert!(c.check(&o));
    // Controls for the sealed cases below: a well-formed private room.
    let mut c = valid_config(1);
    c.privacy_mode = 1;
    c.display_name = private_sealed(10 + ceiling_tag(), 10);
    assert!(c.check(&o), "ciphertext = declared + AEAD tag is allowed");
}

fn ceiling_tag() -> usize {
    AEAD_TAG
}

#[test]
fn check_refuses_each_broken_clause() {
    let o = owner();
    use ceiling::*;
    let cases: Vec<(&str, Break)> = vec![
        case("max_members over", |c| c.max_members = MAX_MEMBERS + 1),
        case("max_links over", |c| c.max_links = MAX_LINKS + 1),
        case("max_links_per_member over", |c| {
            c.max_links_per_member = MAX_LINKS_PER_MEMBER + 1
        }),
        case("max_children_per_link over", |c| {
            c.max_children_per_link = MAX_CHILDREN_PER_LINK + 1
        }),
        case("max_owner_children over", |c| {
            c.max_owner_children = MAX_CHILDREN_PER_LINK + 1
        }),
        case("max_chain_depth over", |c| {
            c.max_chain_depth = MAX_CHAIN_DEPTH + 1
        }),
        case("max_owner_bans over", |c| {
            c.max_owner_bans = MAX_OWNER_BANS + 1
        }),
        case("max_deputy_bans over", |c| {
            c.max_deputy_bans = MAX_DEPUTY_BANS + 1
        }),
        case("max_bans_per_deputy over", |c| {
            c.max_bans_per_deputy = MAX_BANS_PER_DEPUTY + 1
        }),
        case("max_member_bans over", |c| {
            c.max_member_bans = MAX_MEMBER_BANS + 1
        }),
        case("max_bans_per_issuer over", |c| {
            c.max_bans_per_issuer = MAX_BANS_PER_ISSUER + 1
        }),
        case("max_secret_blobs over", |c| {
            c.max_secret_blobs = MAX_SECRET_BLOBS + 1
        }),
        case("max_secret_versions over", |c| {
            c.max_secret_versions = MAX_SECRET_VERSIONS + 1
        }),
        case("max_recent_messages over", |c| {
            c.max_recent_messages = MAX_RECENT_MESSAGES + 1
        }),
        case("max_message_size over", |c| {
            c.max_message_size = MAX_MESSAGE_SIZE + 1
        }),
        case("max_direct_messages over", |c| {
            c.max_direct_messages = MAX_DIRECT_MESSAGES + 1
        }),
        case("max_room_description over", |c| {
            c.max_room_description = MAX_ROOM_DESCRIPTION + 1
        }),
        case("max_room_name over", |c| {
            c.max_room_name = MAX_ROOM_NAME + 1
        }),
        case("max_nickname_size over", |c| {
            c.max_nickname_size = MAX_NICKNAME_SIZE + 1
        }),
        case("max_recent_messages zero", |c| c.max_recent_messages = 0),
        case("max_message_size zero", |c| c.max_message_size = 0),
        case("max_nickname_size zero", |c| c.max_nickname_size = 0),
        case("max_members zero", |c| c.max_members = 0),
        case("max_room_name zero", |c| c.max_room_name = 0),
        case("max_room_description zero", |c| c.max_room_description = 0),
        case("max_direct_messages zero", |c| c.max_direct_messages = 0),
        case("privacy_mode 2", |c| c.privacy_mode = 2),
        case("privacy_mode 255", |c| c.privacy_mode = 255),
        case("private room with a public name", |c| {
            c.privacy_mode = 1;
            c.display_name = Sealed::Public(ByteBuf(vec![]));
        }),
        case("sealed name size not ok", |c| {
            c.privacy_mode = 1;
            c.display_name = private_sealed(10 + AEAD_TAG + 1, 10);
        }),
        case("sealed description size not ok", |c| {
            c.privacy_mode = 1;
            c.display_name = private_sealed(10, 10);
            c.display_description = Some(private_sealed(10 + AEAD_TAG + 1, 10));
        }),
        case("name over the configured max (public)", |c| {
            c.display_name = Sealed::Public(ByteBuf(vec![b'n'; c.max_room_name as usize + 1]))
        }),
        case("name over the configured max (sealed)", |c| {
            c.privacy_mode = 1;
            c.display_name = private_sealed(0, c.max_room_name + 1);
        }),
        case("description over the configured max (public)", |c| {
            c.display_description = Some(Sealed::Public(ByteBuf(vec![
                b'd';
                c.max_room_description
                    as usize
                    + 1
            ])))
        }),
        case("description over the configured max (sealed)", |c| {
            c.privacy_mode = 1;
            c.display_name = private_sealed(0, 0);
            c.display_description = Some(private_sealed(0, c.max_room_description + 1));
        }),
        case("65 deputies", |c| {
            c.deputies = sorted_keys(AUTHORITY_KEYS + 1)
        }),
        case("65 protected", |c| {
            c.protected = sorted_keys(AUTHORITY_KEYS + 1)
        }),
        case("unsorted deputies", |c| {
            let mut d = sorted_keys(2);
            d.reverse();
            c.deputies = d;
        }),
        case("duplicate deputies", |c| {
            let k = sorted_keys(1)[0];
            c.deputies = vec![k, k];
        }),
        case("unsorted protected", |c| {
            let mut d = sorted_keys(2);
            d.reverse();
            c.protected = d;
        }),
        case("duplicate protected", |c| {
            let k = sorted_keys(1)[0];
            c.protected = vec![k, k];
        }),
        case("owner among the deputies", |c| {
            c.deputies = vec![kof(&owner_key())]
        }),
        case("65 child exceptions", |c| {
            c.child_exceptions = sorted_digests(CHILD_EXCEPTIONS + 1)
        }),
        case("unsorted child exceptions", |c| {
            let mut e = sorted_digests(2);
            e.reverse();
            c.child_exceptions = e;
        }),
        case("duplicate child exceptions", |c| {
            c.child_exceptions = vec![(Bytes32([5; 32]), 1), (Bytes32([5; 32]), 2)]
        }),
        case("child exception quota over the ceiling", |c| {
            c.child_exceptions = vec![(Bytes32([5; 32]), MAX_CHILDREN_PER_LINK + 1)]
        }),
    ];
    for (name, break_it) in cases {
        let mut c = valid_config(1);
        break_it(&mut c);
        assert!(!c.check(&o), "check accepted: {name}");
    }
}

#[test]
fn a_forged_higher_version_invalid_configuration_does_not_displace_a_valid_one() {
    let o = owner();
    let mut good = valid_config(1);
    good.max_message_size = 777;
    let mut bad = valid_config(5);
    bad.max_members = ceiling::MAX_MEMBERS + 1;
    bad.max_message_size = 999;
    assert!(good.check(&o) && !bad.check(&o));
    let (rg, rb) = (config_record(&good), config_record(&bad));
    // Both are signed by the owner, so the signature gives no reason to
    // prefer either; only `check` separates them.
    assert!(signature_ok(&o, &rb));
    let recs = RoomStateV2::from_records(vec![rg.clone(), rb.clone()]).records;
    let r = retain(&o, &recs, &mut Verifier::new());
    assert_eq!(r.config.version, 1);
    assert_eq!(r.config.max_message_size, 777);
    let kept: Vec<bool> = recs
        .iter()
        .zip(&r.keep)
        .map(|(x, k)| *k && *x == rb)
        .collect();
    assert!(
        !kept.contains(&true),
        "the invalid configuration is dropped"
    );
    // The same through a delta onto a state that holds the valid one.
    let base = normalize(&o, vec![rg.clone()], &mut Verifier::new()).0;
    let next = apply_delta(&o, &base, vec![rb.clone()], &mut Verifier::new()).unwrap();
    assert_eq!(next, base);
    // As a state, holding both is refused on the kind count (one
    // configuration at most); holding the invalid one alone is not a
    // fixpoint.
    let both = RoomStateV2::from_records(vec![rg, rb.clone()]);
    assert_eq!(validate(&both.to_bytes()).0, Err(Invalid::TooManyRecords));
    let alone = RoomStateV2::from_records(vec![rb]);
    assert_eq!(validate(&alone.to_bytes()).0, Err(Invalid::NotAFixpoint(1)));
}

#[test]
fn an_invalid_configuration_alone_leaves_the_defaults() {
    let o = owner();
    let mut bad = valid_config(5);
    bad.max_members = 0;
    let recs = RoomStateV2::from_records(vec![config_record(&bad)]).records;
    let r = retain(&o, &recs, &mut Verifier::new());
    assert_eq!(r.config, ConfigurationV2::defaults(200));
    assert_eq!(r.keep, vec![false]);
}

// ------------------------------------------------------------ 2. from_v1

#[test]
fn from_v1_clamps_each_value_to_its_ceiling() {
    use ceiling::*;
    let v1 = V1Configuration {
        max_recent_messages: usize::MAX,
        max_user_bans: usize::MAX,
        max_message_size: 1 << 40,
        max_nickname_size: 1 << 40,
        max_members: usize::MAX,
        max_room_name: 1 << 40,
        max_room_description: 1 << 40,
        max_direct_messages: Some(1 << 40),
        ..Default::default()
    };
    let c = ConfigurationV2::from_v1(&v1);
    assert_eq!(c.max_members, MAX_MEMBERS);
    assert_eq!(c.max_recent_messages, MAX_RECENT_MESSAGES);
    assert_eq!(c.max_message_size, MAX_MESSAGE_SIZE);
    assert_eq!(c.max_nickname_size, MAX_NICKNAME_SIZE);
    assert_eq!(c.max_room_name, MAX_ROOM_NAME);
    assert_eq!(c.max_room_description, MAX_ROOM_DESCRIPTION);
    assert_eq!(c.max_direct_messages, MAX_DIRECT_MESSAGES);
    assert_eq!(c.max_owner_bans, MAX_OWNER_BANS);
    assert_eq!(c.max_deputy_bans, MAX_DEPUTY_BANS);
    assert_eq!(c.max_bans_per_deputy, MAX_BANS_PER_DEPUTY);
    assert_eq!(c.max_member_bans, MAX_MEMBER_BANS);
    // Decision (o): the default link quotas; the total and the depth at
    // their ceilings.
    assert_eq!(c.max_links, MAX_LINKS);
    assert_eq!(c.max_links_per_member, 2);
    assert_eq!(c.max_children_per_link, 50);
    assert_eq!(c.max_chain_depth, MAX_CHAIN_DEPTH);
    assert!(
        c.check(&owner()),
        "an all-clamped public configuration is valid"
    );
}

#[test]
fn from_v1_leaves_values_below_the_ceilings_alone() {
    let v1 = V1Configuration {
        max_recent_messages: 17,
        max_user_bans: 3,
        max_message_size: 321,
        max_nickname_size: 12,
        max_members: 42,
        max_room_name: 33,
        max_room_description: 44,
        max_direct_messages: Some(55),
        ..Default::default()
    };
    let c = ConfigurationV2::from_v1(&v1);
    assert_eq!(
        (
            c.max_recent_messages,
            c.max_message_size,
            c.max_nickname_size,
            c.max_members,
            c.max_room_name,
            c.max_room_description,
            c.max_direct_messages,
            c.max_owner_bans,
            c.max_deputy_bans,
            c.max_bans_per_deputy,
            c.max_member_bans,
        ),
        (17, 321, 12, 42, 33, 44, 55, 3, 3, 3, 3)
    );
    // Each ceiling is hit exactly at the boundary, one over is clamped.
    let at = V1Configuration {
        max_members: ceiling::MAX_MEMBERS as usize + 1,
        ..Default::default()
    };
    assert_eq!(
        ConfigurationV2::from_v1(&at).max_members,
        ceiling::MAX_MEMBERS
    );
}

/// A zero v1 value stays zero (`min(0, ceiling)`). The seven settings
/// `check` requires nonzero (messages, message size, nickname size, members,
/// room name, room description, DMs) then make the configuration invalid, so
/// a zero-valued v1 room's starting point is not signable as it is; ban caps
/// of zero are valid. A private v1 room also starts with a public (empty)
/// name, which `check` refuses until the owner supplies a sealed one.
#[test]
fn from_v1_zero_values_stay_zero_and_fail_check() {
    let o = owner();
    let zero = V1Configuration {
        max_recent_messages: 0,
        max_user_bans: 0,
        max_message_size: 0,
        max_nickname_size: 0,
        max_members: 0,
        max_room_name: 0,
        max_room_description: 0,
        max_direct_messages: Some(0),
        ..Default::default()
    };
    let c = ConfigurationV2::from_v1(&zero);
    assert_eq!(c.max_recent_messages, 0);
    assert_eq!(c.max_message_size, 0);
    assert_eq!(c.max_nickname_size, 0);
    assert_eq!(c.max_members, 0);
    assert_eq!(c.max_room_name, 0);
    assert_eq!(c.max_room_description, 0);
    assert_eq!(c.max_direct_messages, 0);
    assert_eq!(
        (
            c.max_owner_bans,
            c.max_deputy_bans,
            c.max_bans_per_deputy,
            c.max_member_bans
        ),
        (0, 0, 0, 0)
    );
    assert!(!c.check(&o), "zero in a nonzero-required field is refused");
    // Each zero field alone is enough to refuse.
    let zeroers: [fn(&mut V1Configuration); 7] = [
        |v| v.max_recent_messages = 0,
        |v| v.max_message_size = 0,
        |v| v.max_nickname_size = 0,
        |v| v.max_members = 0,
        |v| v.max_room_name = 0,
        |v| v.max_room_description = 0,
        |v| v.max_direct_messages = Some(0),
    ];
    for f in zeroers {
        let mut v1 = V1Configuration::default();
        f(&mut v1);
        assert!(!ConfigurationV2::from_v1(&v1).check(&o));
    }
    // Only the ban caps at zero: valid.
    let bans0 = V1Configuration {
        max_user_bans: 0,
        ..Default::default()
    };
    assert!(ConfigurationV2::from_v1(&bans0).check(&o));
    // `None` DMs: v1's default applies.
    let none = V1Configuration {
        max_direct_messages: None,
        ..Default::default()
    };
    assert_eq!(
        ConfigurationV2::from_v1(&none).max_direct_messages as usize,
        V1Configuration::default().effective_max_direct_messages()
    );
    // Private v1 room: privacy_mode 1 with the empty public name.
    let private = V1Configuration {
        privacy_mode: PrivacyMode::Private,
        ..Default::default()
    };
    let c = ConfigurationV2::from_v1(&private);
    assert_eq!(c.privacy_mode, 1);
    assert!(!c.check(&o));
}

// ------------------------------------------------- 3. validate_state refusals

fn one_clock_bytes() -> (Vec<u8>, Record) {
    let p = ClockP { hours: 7 };
    let r = Record::Clock(signed(tag::CLOCK, p, &owner_key()));
    (RoomStateV2::from_records(vec![r.clone()]).to_bytes(), r)
}

#[test]
fn validate_refuses_a_state_over_the_byte_ceiling() {
    let max = max_state_bytes();
    let (r, checks) = validate(&vec![0u8; max + 1]);
    assert_eq!(r, Err(Invalid::TooLarge(max + 1)));
    assert_eq!(checks, 0);
    // At the bound the size is not the reason: garbage that long fails to
    // decode instead, so `>` is not `>=`.
    assert_eq!(validate(&vec![0u8; max]).0, Err(Invalid::NotCanonical));
    assert_eq!(validate(&vec![0u8; max - 1]).0, Err(Invalid::NotCanonical));
}

#[test]
fn a_valid_state_at_the_byte_ceiling_is_not_refused_for_size() {
    // No valid state has exactly `max_state_bytes` bytes (record sizes are
    // fixed), so the comparison is pinned from both sides: bytes of exactly
    // that length are not `TooLarge` (see the test above), and the largest
    // legitimate state is below the bound and valid.
    let (s, dropped) = largest_state(Caps::ceilings().scaled(400), 0);
    assert_eq!(dropped, 0);
    let bytes = s.to_bytes();
    assert!(bytes.len() <= max_state_bytes());
    assert!(validate(&bytes).0.is_ok());
}

#[test]
fn validate_refuses_too_many_records_before_any_signature_check() {
    // Two Clock records (cap 1), unsigned: refused on the count alone.
    let s = RoomStateV2::from_records(vec![
        clock_record(1, zero_sig()),
        clock_record(2, zero_sig()),
    ]);
    let (r, checks) = validate(&s.to_bytes());
    assert_eq!(r, Err(Invalid::TooManyRecords));
    assert_eq!(checks, 0);
    // MAX_LINKS + 1 links (cap MAX_LINKS), unsigned.
    let osk = owner_key();
    let mk = |n: u32| -> Vec<Record> {
        (0..n)
            .map(|i| Record::Link(link(&osk, Parent::Owner(()), i, &skey(1, i), false)))
            .collect()
    };
    assert!(within_kind_counts(&mk(ceiling::MAX_LINKS)));
    let s = RoomStateV2::from_records(mk(ceiling::MAX_LINKS + 1));
    let (r, checks) = validate(&s.to_bytes());
    assert_eq!(r, Err(Invalid::TooManyRecords));
    assert_eq!(checks, 0);
}

#[test]
fn reserved_and_unknown_record_tags_cannot_decode() {
    // [2, [ [tag, 0] ]]
    for tag in [1u8, 24, 25, 30] {
        let mut b = vec![0x82, 0x02, 0x81, 0x82];
        if tag < 24 {
            b.push(tag);
        } else {
            b.extend([0x18, tag]);
        }
        b.push(0x00);
        let (r, checks) = validate(&b);
        assert_eq!(r, Err(Invalid::NotCanonical), "tag {tag}");
        assert_eq!(checks, 0);
    }
}

#[test]
fn empty_garbage_and_truncated_bytes_are_not_canonical() {
    assert_eq!(validate(&[]).0, Err(Invalid::NotCanonical));
    assert_eq!(validate(&[0xff; 64]).0, Err(Invalid::NotCanonical));
    assert_eq!(validate(&[0x00]).0, Err(Invalid::NotCanonical));
    assert_eq!(validate(b"not cbor at all").0, Err(Invalid::NotCanonical));
    let (bytes, _) = one_clock_bytes();
    assert!(validate(&bytes).0.is_ok());
    for n in 0..bytes.len() {
        assert_eq!(
            validate(&bytes[..n]).0,
            Err(Invalid::NotCanonical),
            "prefix of {n} bytes"
        );
    }
    // Trailing bytes.
    let mut t = bytes.clone();
    t.push(0);
    assert_eq!(validate(&t).0, Err(Invalid::NotCanonical));
}

#[test]
fn an_empty_room_state_validates() {
    // [2, []]: no records is a fixpoint of `f`.
    let bytes = empty_state().to_bytes();
    assert_eq!(bytes, vec![0x82, 0x02, 0x80]);
    let (r, checks) = validate(&bytes);
    assert_eq!(r, Ok(empty_state()));
    assert_eq!(checks, 0);
}

/// `[2, [[2, [[7], h'<64 bytes>']]]]` with one non-canonical form spliced in
/// deep inside the record. Each mutated form still decodes as the same value
/// (checked), so only the canonical-encoding rule refuses it.
#[test]
fn non_canonical_inner_encodings_are_refused() {
    let (bytes, _) = one_clock_bytes();
    // 82 02 | 81 | 82 02 | 82 | 81 07 | 58 40 <sig>
    let prefix = [0x82, 0x02, 0x81, 0x82, 0x02, 0x82, 0x81, 0x07, 0x58, 0x40];
    assert_eq!(&bytes[..prefix.len()], &prefix);
    let sig = &bytes[prefix.len()..];
    assert_eq!(sig.len(), 64);
    let join = |parts: &[&[u8]]| parts.concat();
    let mutants: Vec<(&str, Vec<u8>)> = vec![
        (
            "non-minimal integer header (hours 0x18 0x07)",
            join(&[
                &[
                    0x82, 0x02, 0x81, 0x82, 0x02, 0x82, 0x81, 0x18, 0x07, 0x58, 0x40,
                ],
                sig,
            ]),
        ),
        (
            "two-byte integer header (0x19 0x00 0x07)",
            join(&[
                &[
                    0x82, 0x02, 0x81, 0x82, 0x02, 0x82, 0x81, 0x19, 0x00, 0x07, 0x58, 0x40,
                ],
                sig,
            ]),
        ),
        (
            "indefinite-length payload array",
            join(&[
                &[
                    0x82, 0x02, 0x81, 0x82, 0x02, 0x82, 0x9f, 0x07, 0xff, 0x58, 0x40,
                ],
                sig,
            ]),
        ),
        (
            "indefinite-length records array",
            join(&[
                &[0x82, 0x02, 0x9f, 0x82, 0x02, 0x82, 0x81, 0x07, 0x58, 0x40],
                sig,
                &[0xff],
            ]),
        ),
        (
            "non-minimal byte-string length (0x59 0x00 0x40)",
            join(&[
                &[
                    0x82, 0x02, 0x81, 0x82, 0x02, 0x82, 0x81, 0x07, 0x59, 0x00, 0x40,
                ],
                sig,
            ]),
        ),
        (
            "non-minimal record tag",
            join(&[
                &[
                    0x82, 0x02, 0x81, 0x82, 0x18, 0x02, 0x82, 0x81, 0x07, 0x58, 0x40,
                ],
                sig,
            ]),
        ),
        (
            "non-minimal state version",
            join(&[
                &[
                    0x82, 0x18, 0x02, 0x81, 0x82, 0x02, 0x82, 0x81, 0x07, 0x58, 0x40,
                ],
                sig,
            ]),
        ),
        (
            "indefinite-length signature byte string",
            join(&[
                &[
                    0x82, 0x02, 0x81, 0x82, 0x02, 0x82, 0x81, 0x07, 0x5f, 0x58, 0x40,
                ],
                sig,
                &[0xff],
            ]),
        ),
    ];
    for (name, m) in mutants {
        let decoded: Result<RoomStateV2, _> = ciborium::de::from_reader(&m[..]);
        assert!(decoded.is_ok(), "{name}: the mutant must still decode");
        assert_eq!(validate(&m).0, Err(Invalid::NotCanonical), "{name}");
    }
}

#[test]
fn a_wrong_version_is_refused_after_decoding() {
    let s = RoomStateV2 {
        version: 3,
        records: vec![],
    };
    assert_eq!(validate(&s.to_bytes()).0, Err(Invalid::WrongVersion(3)));
}

#[test]
fn a_state_over_a_cap_by_one_is_not_a_fixpoint() {
    let o = owner();
    // max_members = 2 with three members: the third (by rank) is dropped.
    let mut cfg = ConfigurationV2::defaults(2);
    cfg.version = 1;
    assert_eq!(cfg.max_members, 2);
    let osk = owner_key();
    let members: Vec<SigningKey> = (0..3).map(|i| skey(2, i)).collect();
    let mut recs = vec![config_record(&cfg)];
    for (i, m) in members.iter().enumerate() {
        recs.push(Record::Link(link(
            &osk,
            Parent::Owner(()),
            i as u32,
            m,
            true,
        )));
        recs.push(activity(m, 5));
        // A link stays whether or not its member is under the cap; the
        // member's own records (L5) depend on it.
        recs.push(Record::MemberInfo(signed(
            tag::MEMBER_INFO,
            MemberInfoP {
                member: kof(m),
                version: 1,
                nickname: Sealed::Public(ByteBuf(b"nick".to_vec())),
            },
            m,
        )));
    }
    let s = RoomStateV2::from_records(recs.clone());
    let r = retain(&o, &s.records, &mut Verifier::new());
    assert_eq!(r.members.len(), 2, "the member cap holds");
    let dropped = r.keep.iter().filter(|k| !**k).count();
    assert!(dropped >= 1);
    assert_eq!(
        validate(&s.to_bytes()).0,
        Err(Invalid::NotAFixpoint(dropped))
    );
    // At the cap exactly (two members) the state is valid.
    let ok = RoomStateV2::from_records(
        recs.into_iter()
            .filter(|r| match r {
                Record::Link(l) => l.acceptance.member != kof(&members[2]),
                Record::Activity(a) => a.payload.member != kof(&members[2]),
                Record::MemberInfo(i) => i.payload.member != kof(&members[2]),
                _ => true,
            })
            .collect(),
    );
    assert!(validate(&ok.to_bytes()).0.is_ok());
}

#[test]
fn a_link_cap_exceeded_by_one_is_not_a_fixpoint() {
    let osk = owner_key();
    let mut cfg = ConfigurationV2::defaults(100);
    cfg.version = 1;
    cfg.max_links = 2;
    let mut recs = vec![config_record(&cfg)];
    for i in 0..3 {
        recs.push(Record::Link(link(
            &osk,
            Parent::Owner(()),
            i,
            &skey(3, i),
            true,
        )));
    }
    let s = RoomStateV2::from_records(recs.clone());
    assert_eq!(validate(&s.to_bytes()).0, Err(Invalid::NotAFixpoint(1)));
    recs.pop();
    let s = RoomStateV2::from_records(recs);
    assert!(validate(&s.to_bytes()).0.is_ok());
}

#[test]
fn an_orphan_link_is_not_a_fixpoint() {
    // A link whose parent link is absent from the state.
    let missing_parent = skey(4, 0);
    let child = skey(4, 1);
    let missing_link = link(&owner_key(), Parent::Owner(()), 0, &missing_parent, true);
    let orphan = link(
        &missing_parent,
        Parent::Link(Record::Link(missing_link.clone()).digest()),
        0,
        &child,
        true,
    );
    let s = RoomStateV2::from_records(vec![Record::Link(orphan.clone())]);
    assert_eq!(validate(&s.to_bytes()).0, Err(Invalid::NotAFixpoint(1)));
    // With the parent present both stay.
    let s = RoomStateV2::from_records(vec![Record::Link(missing_link), Record::Link(orphan)]);
    assert!(validate(&s.to_bytes()).0.is_ok());
}

// -------------------------------------------------------------- 4. costs

#[test]
fn many_unlinked_activity_records_cost_no_signature_check() {
    let o = owner();
    let delta: Vec<Record> = (0..500).map(|i| activity(&skey(5, i), 3)).collect();
    assert!(within_kind_counts(&delta));
    let base = empty_state();
    let mut v = Verifier::new();
    let next = apply_delta(&o, &base, delta, &mut v).unwrap();
    assert_eq!(next, base);
    assert_eq!(v.checks, 0);
}

#[test]
fn many_broken_links_are_refused_after_one_signature_check() {
    let o = owner();
    let osk = owner_key();
    let delta: Vec<Record> = (0..500)
        .map(|i| Record::Link(link(&osk, Parent::Owner(()), i, &skey(6, i), false)))
        .collect();
    assert!(within_kind_counts(&delta));
    let mut v = Verifier::new();
    let r = apply_delta(&o, &empty_state(), delta, &mut v);
    assert!(matches!(r, Err(Invalid::BadSignature(_))), "{r:?}");
    assert!(v.checks <= 1, "checks = {}", v.checks);
    assert!(v.checks >= 1, "the refusal needs a failed check");
}

#[test]
fn many_clock_records_are_refused_before_any_check() {
    let s = RoomStateV2::from_records((0..200).map(|i| clock_record(i, zero_sig())).collect());
    let (r, checks) = validate(&s.to_bytes());
    assert_eq!(r, Err(Invalid::TooManyRecords));
    assert_eq!(checks, 0);
    let mut v = Verifier::new();
    let r = apply_delta(&owner(), &empty_state(), s.records, &mut v);
    assert_eq!(r, Err(Invalid::TooManyRecords));
    assert_eq!(v.checks, 0);
}

#[test]
fn a_restore_chain_one_past_the_depth_limit_is_never_checked() {
    let o = owner();
    let depth = ceiling::MAX_CHAIN_DEPTH as usize;
    // Control: a chain of exactly the limit verifies.
    let (chain, by) = owner_chain(depth, 1);
    let ok = restore(chain, 1, &by);
    assert!(signature_ok(&o, &ok));
    // One longer: refused by `signature_ok` before any signature is verified,
    // though every link in it is genuinely signed.
    let (chain, by) = owner_chain(depth + 1, 2);
    let long = restore(chain, 1, &by);
    assert!(!signature_ok(&o, &long));
    // Through a delta: dropped without a check.
    let mut v = Verifier::new();
    let next = apply_delta(&o, &empty_state(), vec![long], &mut v).unwrap();
    assert_eq!(next, empty_state());
    assert_eq!(v.checks, 0);
    // The control chain, by contrast, is examined (a restore for a link that
    // is not admitted is still checked once, then kept).
    let mut v = Verifier::new();
    let next = apply_delta(&o, &empty_state(), vec![ok], &mut v).unwrap();
    assert_eq!(v.checks, 1);
    assert_eq!(next.records.len(), 1);
}

fn each_record_is_checked_once(caps: Caps) {
    let o = owner();
    let (s, dropped) = largest_state(caps, 0);
    assert_eq!(dropped, 0);
    let mut v = Verifier::new();
    validate_state(&o, &s.to_bytes(), &mut v).unwrap();
    assert_eq!(v.checks, s.records.len());
}

#[test]
fn validate_checks_each_record_once_scaled() {
    each_record_is_checked_once(Caps::ceilings().scaled(400));
    each_record_is_checked_once(Caps::ceilings_legacy().scaled(400));
}

#[test]
#[ignore = "builds and verifies the full ceiling states (slow in debug); the scaled test covers CI"]
fn validate_checks_each_record_once_at_the_ceilings() {
    each_record_is_checked_once(Caps::ceilings());
    each_record_is_checked_once(Caps::ceilings_legacy());
}

fn delta_cost_is_bounded(caps: Caps) {
    let o = owner();
    let (a, _) = largest_state(caps, 0);
    let (b, _) = largest_state(caps, 1);
    let mut v = Verifier::new();
    let next = apply_delta(&o, &a, b.records.clone(), &mut v).unwrap();
    assert!(
        v.checks <= next.records.len() + 1,
        "checks {} > kept {} + 1",
        v.checks,
        next.records.len()
    );
    let held: std::collections::HashSet<_> = a.digests().into_iter().collect();
    let new = b
        .records
        .iter()
        .filter(|r| !held.contains(&r.digest()))
        .count();
    assert!(v.checks <= new, "held records are not checked again");
}

#[test]
fn applying_the_second_variant_costs_at_most_the_kept_records_plus_one_scaled() {
    delta_cost_is_bounded(Caps::ceilings().scaled(400));
    delta_cost_is_bounded(Caps::ceilings_legacy().scaled(400));
}

#[test]
#[ignore = "builds and verifies the full ceiling states (slow in debug); the scaled test covers CI"]
fn applying_the_second_variant_costs_at_most_the_kept_records_plus_one_at_the_ceilings() {
    delta_cost_is_bounded(Caps::ceilings());
    delta_cost_is_bounded(Caps::ceilings_legacy());
}

// -------------------------------------------------------- 5. restore slots

#[test]
fn a_member_cannot_take_more_than_its_share_of_restore_slots() {
    let o = owner();
    let osk = owner_key();
    let m = skey(7, 0);
    let m_link = link(&osk, Parent::Owner(()), 0, &m, true);
    let m_link_digest = Record::Link(m_link.clone()).digest();
    let mut recs = vec![Record::Link(m_link), activity(&m, 1)];
    // Five requests, each a fake link of the member's own key under its
    // admitted link; later ones carry later hours, so they sort first.
    let mut member_requests = vec![];
    for i in 0..5u32 {
        let x = skey(7, 10 + i);
        let fake = link(&m, Parent::Link(m_link_digest), 100 + i, &x, true);
        let r = restore(vec![fake], 100 + i, &x);
        member_requests.push(r.clone());
        recs.push(r);
    }
    // One owner-attached request with the lowest hours of all.
    let y = skey(7, 99);
    let owner_req = restore(vec![link(&osk, Parent::Owner(()), 7, &y, true)], 1, &y);
    recs.push(owner_req.clone());

    let (state, _) = normalize(&o, recs, &mut Verifier::new());
    let count = |set: &[Record]| set.iter().filter(|r| state.records.contains(r)).count();
    let kept_member = count(&member_requests);
    assert!(
        kept_member <= ceiling::RESTORES_PER_SUBTREE,
        "kept {kept_member} of one member's requests"
    );
    assert_eq!(kept_member, ceiling::RESTORES_PER_SUBTREE);
    assert!(
        state.records.contains(&owner_req),
        "the owner-attached request still has a slot"
    );
    assert!(validate(&state.to_bytes()).0.is_ok());
}

#[test]
fn keys_a_member_admits_share_its_subtree_s_restore_slots() {
    let o = owner();
    let osk = owner_key();
    let m = skey(8, 0);
    let m_link = link(&osk, Parent::Owner(()), 0, &m, true);
    let m_digest = Record::Link(m_link.clone()).digest();
    let mut recs = vec![Record::Link(m_link), activity(&m, 1)];
    // Four keys the member admits, each signing two requests for fake links
    // of its own keys under its own link, later hours first.
    let mut sybil_requests = vec![];
    for s in 0..4u32 {
        let k = skey(8, 1 + s);
        let k_link = link(&m, Parent::Link(m_digest), s, &k, true);
        let k_digest = Record::Link(k_link.clone()).digest();
        recs.push(Record::Link(k_link));
        recs.push(activity(&k, 1));
        for r in 0..2u32 {
            let x = skey(8, 100 + s * 10 + r);
            let fake = link(&k, Parent::Link(k_digest), 50 + r, &x, true);
            let req = restore(vec![fake], 200 + s * 10 + r, &x);
            sybil_requests.push(req.clone());
            recs.push(req);
        }
    }
    // Another of the owner's invitees, with one request under its link.
    let n = skey(8, 50);
    let n_link = link(&osk, Parent::Owner(()), 1, &n, true);
    let n_digest = Record::Link(n_link.clone()).digest();
    recs.push(Record::Link(n_link));
    recs.push(activity(&n, 1));
    let z = skey(8, 51);
    let n_req = restore(vec![link(&n, Parent::Link(n_digest), 9, &z, true)], 1, &z);
    recs.push(n_req.clone());

    let (state, _) = normalize(&o, recs, &mut Verifier::new());
    let kept = sybil_requests
        .iter()
        .filter(|r| state.records.contains(r))
        .count();
    assert_eq!(kept, ceiling::RESTORES_PER_SUBTREE, "one subtree's share");
    assert!(
        state.records.contains(&n_req),
        "another subtree's request keeps a slot"
    );
    assert!(validate(&state.to_bytes()).0.is_ok());
}

// ------------------------------------------------------- 6. roster batches

/// Batches that only repeat attested entries still count against the
/// roster's entry budget and batch count, so `f` never emits a state over
/// `max_state_bytes` (decision (s)).
#[test]
fn repeated_roster_entries_count_and_the_result_validates_at_the_ceilings() {
    use crate::room_state_v2::retain::roster_entry_budget;
    let o = owner();
    let osk = owner_key();
    let (s, dropped) = largest_state(Caps::ceilings_legacy(), 0);
    assert_eq!(dropped, 0);
    let held: Vec<RosterEntry> = s
        .records
        .iter()
        .filter_map(|r| match r {
            Record::Roster(b) => Some(b.payload.batch.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    assert!(!held.is_empty());
    // Every held entry again, as many times as fits twice over, in batches
    // of 1,000 that only repeat.
    let mut recs = s.records.clone();
    let mut seq = 1_000u32;
    for _ in 0..(2 * roster_entry_budget() / held.len() + 1) {
        for chunk in held.chunks(ceiling::ROSTER_BATCH_ENTRIES) {
            let mut batch = chunk.to_vec();
            batch.sort_by_key(|e| (e.kind, e.digest));
            recs.push(Record::Roster(signed(
                tag::ROSTER,
                RosterP { seq, batch },
                &osk,
            )));
            seq += 1;
        }
    }
    let (out, _) = normalize(&o, recs, &mut Verifier::new());
    let batches: Vec<&RosterP> = out
        .records
        .iter()
        .filter_map(|r| match r {
            Record::Roster(b) => Some(&b.payload),
            _ => None,
        })
        .collect();
    let entries: usize = batches.iter().map(|b| b.batch.len()).sum();
    assert!(
        batches.len() <= ceiling::ROSTER_BATCHES,
        "{} batches",
        batches.len()
    );
    assert!(entries <= roster_entry_budget(), "{entries} entries");
    let bytes = out.to_bytes();
    assert!(
        bytes.len() <= max_state_bytes(),
        "{} > {}",
        bytes.len(),
        max_state_bytes()
    );
    assert!(validate(&bytes).0.is_ok());
}
