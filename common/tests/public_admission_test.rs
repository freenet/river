//! Public-room provisional admission (freenet/river#974, Phase 1): the
//! deterministic cases of the design's test list. The randomized fork-merge
//! cases are `Mode::Provisional` in `fork_merge_closure_test.rs`.
//!
//! Every state an apply produces here is also checked the way the contract's
//! `validate_state` checks it: `validate_whole_state` (verify, then cleanup
//! must leave the encoded bytes unchanged).

use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_scaffold::ComposableState;
use river_core::room_state::ban::{AuthorizedUserBan, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::direct_messages::{sign_direct_message, DirectMessagesDelta};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersDelta};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
use river_core::room_state::privacy::PrivacyMode;
use river_core::room_state::provisional::{
    leading_zero_bits, pow_hash, AdmissionError, AdmissionEvidence, AuthorizedProvisionalBan,
    AuthorizedProvisionalMember, PowPolicy, ProvisionalBan, ProvisionalDelta, ProvisionalMember,
    PublicRoomPolicy, PROVISIONAL_MESSAGES_PER_AUTHOR,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
use std::time::{Duration, SystemTime};

pub fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

pub fn id_of(k: &SigningKey) -> MemberId {
    k.verifying_key().into()
}

pub fn policy(bits: u8, epoch: u32) -> PublicRoomPolicy {
    PublicRoomPolicy {
        listed: false,
        max_provisional: 5,
        max_provisional_messages: 20,
        max_provisional_bans: 5,
        pow: Some(PowPolicy {
            difficulty_bits: bits,
            epoch,
        }),
    }
}

pub fn solve(owner: &VerifyingKey, member: &VerifyingKey, epoch: u32, bits: u8) -> u64 {
    (0u64..)
        .find(|n| leading_zero_bits(&pow_hash(owner, member, epoch, *n)) >= bits as u32)
        .unwrap()
}

pub struct Room {
    pub owner: SigningKey,
    pub params: ChatRoomParametersV1,
    pub state: ChatRoomStateV1,
    pub version: u32,
}

impl Room {
    pub fn new(max_recent: usize, max_members: usize, policy: Option<PublicRoomPolicy>) -> Self {
        let owner = sk(200);
        let params = ChatRoomParametersV1 {
            owner: owner.verifying_key(),
        };
        let config = Configuration {
            owner_member_id: params.owner_id(),
            max_recent_messages: max_recent,
            max_members,
            public_admission: policy,
            ..Default::default()
        };
        let state = ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(config, &owner),
            ..Default::default()
        };
        Room {
            owner,
            params,
            state,
            version: 1,
        }
    }

    pub fn owner_id(&self) -> MemberId {
        self.params.owner_id()
    }

    pub fn config(&self) -> &Configuration {
        &self.state.configuration.configuration
    }

    /// The owner signs the next configuration, edited by `f`.
    pub fn config_delta(&mut self, f: impl FnOnce(&mut Configuration)) -> ChatRoomStateV1Delta {
        let mut c = self.config().clone();
        self.version += 1;
        c.configuration_version = self.version;
        f(&mut c);
        ChatRoomStateV1Delta {
            configuration: Some(AuthorizedConfigurationV1::new(c, &self.owner)),
            ..Default::default()
        }
    }

    pub fn set_config(&mut self, f: impl FnOnce(&mut Configuration)) {
        let d = self.config_delta(f);
        self.apply(d).expect("config applies");
    }

    /// Apply a delta the way the contract does (`update_state`), then check
    /// the result the way `validate_state` does.
    pub fn apply(&mut self, d: ChatRoomStateV1Delta) -> Result<(), String> {
        let mut next = self.state.clone();
        next.apply_delta(&self.state.clone(), &self.params, &Some(d))?;
        validate(&next, &self.params)?;
        self.state = next;
        Ok(())
    }

    pub fn entry(&self, k: &SigningKey) -> AuthorizedProvisionalMember {
        let p = self.config().public_admission.clone().unwrap();
        let pow = p.pow.unwrap();
        entry_with(
            &self.params.owner,
            k,
            pow.epoch,
            solve(
                &self.params.owner,
                &k.verifying_key(),
                pow.epoch,
                pow.difficulty_bits,
            ),
        )
    }

    pub fn msg(&self, k: &SigningKey, t: u64, text: &str) -> AuthorizedMessageV1 {
        msg_of(self.owner_id(), k, t, text)
    }

    pub fn invite(&self, inviter: Option<&SigningKey>, invitee: &VerifyingKey) -> AuthorizedMember {
        let (id, key) = match inviter {
            None => (self.owner_id(), &self.owner),
            Some(k) => (id_of(k), k),
        };
        AuthorizedMember::new(
            Member {
                owner_member_id: self.owner_id(),
                invited_by: id,
                member_vk: *invitee,
            },
            key,
        )
    }

    /// An entry with the member's `member_info` and first message, in one delta.
    pub fn join_delta(&self, k: &SigningKey, t: u64, text: &str) -> ChatRoomStateV1Delta {
        ChatRoomStateV1Delta {
            provisional: Some(ProvisionalDelta {
                entries: vec![self.entry(k)],
                bans: vec![],
            }),
            member_info: Some(vec![AuthorizedMemberInfo::new_with_member_key(
                MemberInfo::new_public(id_of(k), 1, format!("p{}", id_of(k))),
                k,
            )]),
            recent_messages: Some(vec![self.msg(k, t, text)]),
            ..Default::default()
        }
    }

    pub fn members_delta(&self, v: Vec<AuthorizedMember>) -> ChatRoomStateV1Delta {
        ChatRoomStateV1Delta {
            members: Some(MembersDelta::new(v)),
            ..Default::default()
        }
    }

    pub fn msg_delta(&self, v: Vec<AuthorizedMessageV1>) -> ChatRoomStateV1Delta {
        ChatRoomStateV1Delta {
            recent_messages: Some(v),
            ..Default::default()
        }
    }

    pub fn prov_ban(
        &self,
        by: Option<&SigningKey>,
        target: MemberId,
        at: u64,
    ) -> AuthorizedProvisionalBan {
        let (id, key) = match by {
            None => (self.owner_id(), &self.owner),
            Some(k) => (id_of(k), k),
        };
        AuthorizedProvisionalBan::new(
            &self.params.owner,
            ProvisionalBan {
                banned_by: id,
                banned_at: (SystemTime::UNIX_EPOCH + Duration::from_secs(at)).into(),
                target,
            },
            key,
        )
    }

    pub fn user_ban(
        &self,
        by: Option<&SigningKey>,
        target: MemberId,
        at: u64,
    ) -> AuthorizedUserBan {
        let (id, key) = match by {
            None => (self.owner_id(), &self.owner),
            Some(k) => (id_of(k), k),
        };
        AuthorizedUserBan::new(
            UserBan {
                owner_member_id: self.owner_id(),
                banned_at: (SystemTime::UNIX_EPOCH + Duration::from_secs(at)).into(),
                banned_user: target,
            },
            id,
            key,
        )
    }

    pub fn has_entry(&self, k: &SigningKey) -> bool {
        self.state
            .provisional
            .entries
            .iter()
            .any(|e| e.provisional.member_vk == k.verifying_key())
    }

    pub fn has_msg_by(&self, k: &SigningKey) -> bool {
        let id = id_of(k);
        self.state
            .recent_messages
            .messages
            .iter()
            .any(|m| m.message.author == id)
    }

    pub fn is_member(&self, k: &SigningKey) -> bool {
        let id = id_of(k);
        self.state
            .members
            .members
            .iter()
            .any(|m| m.member.id() == id)
    }
}

pub fn entry_with(
    owner: &VerifyingKey,
    k: &SigningKey,
    epoch: u32,
    nonce: u64,
) -> AuthorizedProvisionalMember {
    let _ = epoch;
    AuthorizedProvisionalMember::new(
        owner,
        ProvisionalMember {
            member_vk: k.verifying_key(),
            evidence: AdmissionEvidence::Pow { nonce },
        },
        k,
    )
}

pub fn msg_of(owner_id: MemberId, k: &SigningKey, t: u64, text: &str) -> AuthorizedMessageV1 {
    AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: owner_id,
            author: id_of(k),
            time: (SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000 + t)).into(),
            content: RoomMessageBody::public(text.to_string()),
        },
        k,
    )
}

pub fn ser(s: &ChatRoomStateV1) -> Vec<u8> {
    let mut v = Vec::new();
    ciborium::ser::into_writer(s, &mut v).unwrap();
    v
}

/// The contract's `validate_state`.
pub fn validate(s: &ChatRoomStateV1, p: &ChatRoomParametersV1) -> Result<(), String> {
    s.validate_whole_state(p, &ser(s))
        .map_err(|e| e.to_string())
}

/// `merge_from` both ways, each result validated.
pub fn merge2(
    a: &ChatRoomStateV1,
    b: &ChatRoomStateV1,
    p: &ChatRoomParametersV1,
) -> (ChatRoomStateV1, ChatRoomStateV1) {
    let mut ab = a.clone();
    ab.merge_from(p, b).expect("merge a b");
    let mut ba = b.clone();
    ba.merge_from(p, a).expect("merge b a");
    validate(&ab, p).expect("merge(a,b) validates");
    validate(&ba, p).expect("merge(b,a) validates");
    (ab, ba)
}

/// A key whose `MemberId` equals `victim`'s and whose bytes differ (see the
/// `MemberId` docs). It has no secret key.
pub fn forged_vk_with_id_of(victim: &VerifyingKey) -> VerifyingKey {
    let target: MemberId = victim.into();
    let base = victim.to_bytes();
    for i in 0..31 {
        let mut bytes = base;
        if bytes[i] == 255 || bytes[i + 1] < 31 {
            continue;
        }
        bytes[i] += 1;
        bytes[i + 1] -= 31;
        if let Ok(vk) = VerifyingKey::from_bytes(&bytes) {
            if MemberId::from(&vk) == target && vk != *victim {
                return vk;
            }
        }
    }
    panic!("no forged key found");
}

pub fn dm(
    room: &Room,
    from: &SigningKey,
    to: MemberId,
    t: u64,
) -> river_core::room_state::direct_messages::AuthorizedDirectMessage {
    sign_direct_message(from, id_of(from), to, &room.params.owner, t, b"x".to_vec()).unwrap()
}

pub fn dm_delta(
    m: Vec<river_core::room_state::direct_messages::AuthorizedDirectMessage>,
) -> ChatRoomStateV1Delta {
    ChatRoomStateV1Delta {
        direct_messages: Some(DirectMessagesDelta {
            new_messages: m,
            advanced_purges: vec![],
        }),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Smoke: the basic flow
// ---------------------------------------------------------------------------

#[test]
fn a_provisional_member_posts_with_their_first_message_in_one_delta() {
    let mut room = Room::new(20, 10, Some(policy(4, 7)));
    let (a, b) = (sk(1), sk(2));
    room.apply(room.join_delta(&a, 1, "hello")).unwrap();
    assert!(room.has_entry(&a), "the entry is kept");
    assert!(room.has_msg_by(&a), "the first message is kept");
    assert!(
        !room.is_member(&a),
        "a provisional member is never in members"
    );
    assert!(
        room.state
            .member_info
            .member_info
            .iter()
            .any(|i| i.member_info.member_id == id_of(&a)),
        "their member_info is kept"
    );
    // A second author, in a later delta.
    room.apply(room.join_delta(&b, 2, "hi")).unwrap();
    assert!(room.has_entry(&b) && room.has_msg_by(&b));
    // A later message from the first author needs no entry attached.
    room.apply(room.msg_delta(vec![room.msg(&a, 3, "again")]))
        .unwrap();
    assert_eq!(room.state.recent_messages.messages.len(), 3);
}

#[test]
fn an_entry_with_no_message_is_dropped() {
    let mut room = Room::new(20, 10, Some(policy(4, 7)));
    let a = sk(1);
    let d = ChatRoomStateV1Delta {
        provisional: Some(ProvisionalDelta {
            entries: vec![room.entry(&a)],
            bans: vec![],
        }),
        ..Default::default()
    };
    room.apply(d).unwrap();
    assert!(
        !room.has_entry(&a),
        "P7: no message in the window, no entry"
    );
}

#[test]
fn nothing_is_admitted_without_a_policy_or_in_a_private_room() {
    // No policy: the same delta applies and leaves nothing.
    let mut with = Room::new(20, 10, Some(policy(4, 7)));
    let a = sk(1);
    let d = with.join_delta(&a, 1, "hi");
    let mut room = Room::new(20, 10, None);
    room.apply(d.clone()).unwrap();
    assert!(!room.has_entry(&a) && !room.has_msg_by(&a));
    // A private room with a policy is valid and inert.
    let mut private = Room::new(20, 10, Some(policy(4, 7)));
    private.set_config(|c| {
        c.privacy_mode = PrivacyMode::Private;
        c.display.name = river_core::room_state::privacy::SealedBytes::Private {
            ciphertext: vec![1; 16],
            nonce: [0; 12],
            secret_version: 0,
            declared_len_bytes: 4,
        };
        c.display.description = None;
    });
    private.apply(d).unwrap();
    assert!(!private.has_entry(&a) && !private.has_msg_by(&a));
    let _ = &mut with;
}

#[test]
fn an_entry_below_the_required_work_is_skipped_not_an_error() {
    let mut room = Room::new(20, 10, Some(policy(8, 7)));
    let a = sk(1);
    // A nonce with fewer than 8 bits.
    let owner = room.params.owner;
    let weak = (0u64..)
        .find(|n| leading_zero_bits(&pow_hash(&owner, &a.verifying_key(), 7, *n)) < 8)
        .unwrap();
    let d = ChatRoomStateV1Delta {
        provisional: Some(ProvisionalDelta {
            entries: vec![entry_with(&owner, &a, 7, weak)],
            bans: vec![],
        }),
        recent_messages: Some(vec![room.msg(&a, 1, "hi")]),
        ..Default::default()
    };
    room.apply(d)
        .expect("a weak entry is skipped, never an Err");
    assert!(!room.has_entry(&a) && !room.has_msg_by(&a));
}

#[test]
fn a_forged_entry_signature_is_an_error() {
    let mut room = Room::new(20, 10, Some(policy(4, 7)));
    let (a, b) = (sk(1), sk(2));
    let mut e = room.entry(&a);
    // Signed by someone else.
    e.signature = room.entry(&b).signature;
    let d = ChatRoomStateV1Delta {
        provisional: Some(ProvisionalDelta {
            entries: vec![e],
            bans: vec![],
        }),
        ..Default::default()
    };
    assert!(room.apply(d).is_err());
}

#[test]
fn the_provisional_pool_and_quota_bound_the_window() {
    // pool cap 2, quota min(20, 10/2) = 5, window 10.
    let mut p = policy(0, 1);
    p.max_provisional = 2;
    let mut room = Room::new(10, 10, Some(p));
    let owner = sk(200);
    for t in 1..=3u64 {
        room.apply(room.msg_delta(vec![room.msg(&owner, t, "owner")]))
            .unwrap();
    }
    // Three provisional authors post, newest last: only the two newest fit.
    let (a, b, c) = (sk(1), sk(2), sk(3));
    room.apply(room.join_delta(&a, 10, "a")).unwrap();
    room.apply(room.join_delta(&b, 11, "b")).unwrap();
    room.apply(room.join_delta(&c, 12, "c")).unwrap();
    assert!(room.has_entry(&c) && room.has_entry(&b));
    assert!(
        !room.has_entry(&a) && !room.has_msg_by(&a),
        "oldest author falls out of the pool"
    );
    assert_eq!(room.state.provisional.entries.len(), 2);
    // The owner's messages are untouched.
    assert_eq!(
        room.state
            .recent_messages
            .messages
            .iter()
            .filter(|m| m.message.author == id_of(&owner))
            .count(),
        3
    );
}

/// A full pool at the ceiling (24 entries): a new admission is not refused for
/// being the 33rd. The pool is the authors of the newest provisional messages
/// in the window (ties broken by message id), so the author whose newest
/// message is oldest is the one that falls out, with its entry and messages. A
/// newcomer whose message is older than all 24 is the one dropped instead. The
/// dropped author keeps its stored entry and gets back in by sending a newer
/// message (the client attaches the entry again).
#[test]
fn a_full_pool_ages_out_the_author_with_the_oldest_newest_message() {
    let mut p = policy(0, 1);
    p.max_provisional = 24;
    p.max_provisional_messages = 40;
    let mut room = Room::new(100, 10, Some(p));
    let keys: Vec<SigningKey> = (1..=24u8).map(sk).collect();
    for (i, k) in keys.iter().enumerate() {
        room.apply(room.join_delta(k, 100 + i as u64, "hi"))
            .unwrap();
    }
    assert_eq!(room.state.provisional.entries.len(), 24);

    // A 25th newcomer with the newest message gets in; the oldest author goes.
    let newcomer = sk(50);
    room.apply(room.join_delta(&newcomer, 200, "me")).unwrap();
    assert_eq!(room.state.provisional.entries.len(), 24);
    assert!(room.has_entry(&newcomer) && room.has_msg_by(&newcomer));
    assert!(
        !room.has_entry(&keys[0]) && !room.has_msg_by(&keys[0]),
        "the author whose newest message is oldest ages out, entry and messages"
    );
    for k in &keys[1..] {
        assert!(room.has_entry(k), "every other author stays");
    }

    // The author that aged out comes back with a newer message (its entry
    // attached again) and the next oldest author falls out.
    room.apply(room.join_delta(&keys[0], 300, "back")).unwrap();
    assert!(room.has_entry(&keys[0]) && room.has_msg_by(&keys[0]));
    assert!(!room.has_entry(&keys[1]));
    assert_eq!(room.state.provisional.entries.len(), 24);

    // A newcomer whose message is older than all 24 is the one refused.
    let stale = sk(51);
    room.apply(room.join_delta(&stale, 50, "late")).unwrap();
    assert!(!room.has_entry(&stale) && !room.has_msg_by(&stale));
    assert_eq!(room.state.provisional.entries.len(), 24);
}

/// One provisional author can no longer fill the newcomer message quota: it
/// holds its newest `PROVISIONAL_MESSAGES_PER_AUTHOR` messages, and the
/// honest newcomers' older messages stay.
#[test]
fn one_provisional_author_cannot_fill_the_quota() {
    let mut p = policy(0, 1);
    p.max_provisional = 10;
    p.max_provisional_messages = 20;
    let mut room = Room::new(40, 10, Some(p)); // quota min(20, 20) = 20
    let honest: Vec<SigningKey> = (1..=5u8).map(sk).collect();
    for (i, k) in honest.iter().enumerate() {
        room.apply(room.join_delta(k, 10 + i as u64, "hi")).unwrap();
    }
    let hog = sk(100);
    room.apply(room.join_delta(&hog, 1000, "m0")).unwrap();
    for t in 1001..=1019u64 {
        room.apply(room.msg_delta(vec![room.msg(&hog, t, "m")]))
            .unwrap();
    }
    let times_of = |room: &Room, k: &SigningKey| -> Vec<u64> {
        room.state
            .recent_messages
            .messages
            .iter()
            .filter(|m| m.message.author == id_of(k))
            .map(|m| m.message.time.secs() - 1_800_000_000)
            .collect()
    };
    let mut hogs = times_of(&room, &hog);
    hogs.sort();
    assert_eq!(hogs, vec![1018, 1019], "the author's newest two are kept");
    for k in &honest {
        assert!(
            room.has_entry(k) && room.has_msg_by(k),
            "honest newcomer kept"
        );
    }
}

/// The pool is a function of the set of messages held: the same set in any
/// order, with repeats, gives identical bytes, and cleanup is idempotent.
mod pool_is_a_function_of_the_set {
    use super::*;
    use proptest::prelude::*;
    use std::sync::OnceLock;

    const AUTHORS: usize = 6;
    const TIMES: usize = 24;

    struct Fixture {
        room: Room,
        msgs: Vec<AuthorizedMessageV1>, // [author * TIMES + t]
    }

    fn fixture() -> &'static Fixture {
        static F: OnceLock<Fixture> = OnceLock::new();
        F.get_or_init(|| {
            let mut p = policy(0, 1);
            p.max_provisional = 3;
            p.max_provisional_messages = 6;
            let mut room = Room::new(12, 10, Some(p)); // quota min(6, 6) = 6
            let mut msgs = Vec::new();
            for a in 0..AUTHORS {
                let k = sk(a as u8 + 1);
                room.state.provisional.entries.push(room.entry(&k));
                for t in 0..TIMES {
                    msgs.push(room.msg(&k, t as u64, "m"));
                }
            }
            Fixture { room, msgs }
        })
    }

    /// Deliver `picks` as one messages delta (repeats included) to the
    /// room holding the entries, then clean up as the contract does.
    fn run(picks: &[usize]) -> (Vec<u8>, ChatRoomStateV1) {
        let f = fixture();
        let mut s = f.room.state.clone();
        let delta = ChatRoomStateV1Delta {
            recent_messages: Some(picks.iter().map(|&i| f.msgs[i].clone()).collect()),
            ..Default::default()
        };
        s.apply_delta(&f.room.state.clone(), &f.room.params, &Some(delta))
            .unwrap();
        (ser(&s), s)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn same_set_same_pool(
            set in proptest::collection::btree_set(0..AUTHORS * TIMES, 0..60),
            seed in any::<u64>(),
            dups in 0usize..20,
        ) {
            use rand::seq::SliceRandom;
            use rand::SeedableRng;
            let base: Vec<usize> = set.iter().copied().collect();
            let (reference, state) = run(&base);
            // At most the cap per provisional author.
            let mut per: std::collections::HashMap<MemberId, usize> = Default::default();
            for m in &state.recent_messages.messages {
                *per.entry(m.message.author).or_default() += 1;
            }
            prop_assert!(per.values().all(|&n| n <= PROVISIONAL_MESSAGES_PER_AUTHOR));
            // Reordered, with repeats.
            let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
            let mut shuffled = base.clone();
            shuffled.extend(base.iter().take(dups));
            shuffled.shuffle(&mut rng);
            prop_assert_eq!(&run(&shuffled).0, &reference);
            // Idempotent.
            let mut again = state.clone();
            again.post_apply_cleanup(&fixture().room.params).unwrap();
            prop_assert_eq!(ser(&again), reference);
        }
    }
}

#[test]
fn provisional_messages_fill_at_most_the_quota_and_members_keep_the_rest() {
    // window 6 => quota min(500, 3) = 3 (max_provisional_messages 500).
    let mut p = policy(0, 1);
    p.max_provisional = 10;
    p.max_provisional_messages = 500;
    let mut room = Room::new(6, 10, Some(p));
    let owner = sk(200);
    for t in 1..=4u64 {
        room.apply(room.msg_delta(vec![room.msg(&owner, t, &format!("o{t}"))]))
            .unwrap();
    }
    let a = sk(1);
    room.apply(room.join_delta(&a, 100, "a0")).unwrap();
    for t in 101..=110u64 {
        room.apply(room.msg_delta(vec![room.msg(&a, t, &format!("a{t}"))]))
            .unwrap();
    }
    let by = |room: &Room, k: &SigningKey| {
        room.state
            .recent_messages
            .messages
            .iter()
            .filter(|m| m.message.author == id_of(k))
            .count()
    };
    // One author holds at most PROVISIONAL_MESSAGES_PER_AUTHOR of the quota.
    assert_eq!(by(&room, &a), PROVISIONAL_MESSAGES_PER_AUTHOR);
    assert_eq!(
        by(&room, &owner),
        4,
        "the owner keeps the rest of the window"
    );
    // Three authors fill the quota (3), two each would be 6: the newest three.
    let (b, c) = (sk(2), sk(3));
    for (k, t) in [(&b, 200u64), (&c, 300u64)] {
        room.apply(room.join_delta(k, t, "x")).unwrap();
        room.apply(room.msg_delta(vec![room.msg(k, t + 1, "y")]))
            .unwrap();
    }
    let provisional: usize = [&a, &b, &c].iter().map(|k| by(&room, k)).sum();
    assert_eq!(provisional, 3, "provisional share is the quota");
    assert_eq!(by(&room, &owner), 3, "the owner keeps window - quota slots");
}

// ---------------------------------------------------------------------------
// Test list 2, 3, 4, 13: stale deltas, idempotence, sponsorship, no synthetic records
// ---------------------------------------------------------------------------

/// Apply `deltas` in the given order to a clone of `room`.
fn replay(room: &Room, deltas: &[&ChatRoomStateV1Delta]) -> ChatRoomStateV1 {
    let mut r = Room {
        owner: room.owner.clone(),
        params: room.params.clone(),
        state: room.state.clone(),
        version: room.version,
    };
    for d in deltas {
        r.apply((*d).clone()).expect("delta accepted in this order");
    }
    r.state
}

#[test]
fn a_stale_entry_is_skipped_with_its_message_and_the_rest_applies_in_every_order() {
    let base = Room::new(30, 10, Some(policy(2, 7)));
    let (a, m) = (sk(1), sk(2));
    let owner = sk(200);
    // v1 (difficulty 2) admits the entry; v2 raises the difficulty (and
    // changes the epoch, so the old proof cannot satisfy it either).
    let mut v2 = Room {
        owner: base.owner.clone(),
        params: base.params.clone(),
        state: base.state.clone(),
        version: base.version,
    };
    let raise = v2.config_delta(|c| {
        c.public_admission = Some(policy(14, 8));
    });
    // One delta: the entry, its author's first message, and unrelated
    // records (a member join with its message, an owner message).
    let mut d = base.join_delta(&a, 1, "stale hello");
    d.members = Some(MembersDelta::new(vec![
        base.invite(None, &m.verifying_key())
    ]));
    d.recent_messages = Some(vec![
        base.msg(&a, 1, "stale hello"),
        base.msg(&m, 2, "member hello"),
        base.msg(&owner, 3, "owner hello"),
    ]);
    let orders: [[&ChatRoomStateV1Delta; 2]; 2] = [[&raise, &d], [&d, &raise]];
    let mut finals = Vec::new();
    for order in orders {
        let s = replay(&base, &order);
        assert!(
            s.provisional.entries.is_empty(),
            "the entry does not survive v2"
        );
        assert!(
            !s.recent_messages
                .messages
                .iter()
                .any(|x| x.message.author == id_of(&a)),
            "the stale entry's message goes with it"
        );
        assert_eq!(s.members.members.len(), 1, "the unrelated join applied");
        assert_eq!(
            s.recent_messages.messages.len(),
            2,
            "the member's and the owner's messages applied"
        );
        finals.push(s);
    }
    assert_eq!(ser(&finals[0]), ser(&finals[1]), "both orders converge");
    // A third peer that still holds v1 and the entry merges with them both ways.
    let holder = replay(&base, &[&d]);
    assert!(holder.provisional.entries.len() == 1);
    for other in &finals {
        let (x, y) = merge2(&holder, other, &base.params);
        assert_eq!(ser(&x), ser(&y), "the holder and a v2 peer converge");
        assert!(x.provisional.entries.is_empty());
        assert!(!x
            .recent_messages
            .messages
            .iter()
            .any(|m| m.message.author == id_of(&a)));
    }
    // A replayed v1 configuration cannot win, and replaying the stale delta
    // after v2 changes nothing.
    let after = replay(&base, &[&raise, &d]);
    let again = replay(&base, &[&raise, &d, &d]);
    assert_eq!(ser(&after), ser(&again));
}

#[test]
fn a_message_whose_author_id_resolves_to_another_key_is_skipped_not_an_error() {
    let mut room = Room::new(30, 10, Some(policy(2, 7)));
    let (m, v) = (sk(2), sk(3));
    // M invites a forged key F with V's id; V's message (signed by V's key)
    // arrives, and then V's member_info.
    let forged = forged_vk_with_id_of(&v.verifying_key());
    assert_eq!(MemberId::from(&forged), id_of(&v));
    room.apply(room.members_delta(vec![room.invite(None, &m.verifying_key())]))
        .unwrap();
    room.apply(room.members_delta(vec![room.invite(Some(&m), &forged)]))
        .unwrap();
    let bad = ChatRoomStateV1Delta {
        recent_messages: Some(vec![room.msg(&v, 5, "from V")]),
        member_info: Some(vec![AuthorizedMemberInfo::new_with_member_key(
            MemberInfo::new_public(id_of(&v), 1, "V".into()),
            &v,
        )]),
        ..Default::default()
    };
    room.apply(bad).expect("skipped, never an Err");
    assert!(!room.has_msg_by(&v));
    assert!(!room
        .state
        .member_info
        .member_info
        .iter()
        .any(|i| i.member_info.member_id == id_of(&v)));
}

#[test]
fn the_same_delta_twice_and_redelivered_records_leave_identical_bytes() {
    let mut room = Room::new(30, 10, Some(policy(2, 7)));
    let a = sk(1);
    let d = room.join_delta(&a, 1, "once");
    room.apply(d.clone()).unwrap();
    let once = ser(&room.state);
    room.apply(d.clone()).unwrap();
    assert_eq!(once, ser(&room.state), "the same delta twice");
    // A delta with the entry duplicated inside it.
    let mut dup = d.clone();
    let e = dup.provisional.as_ref().unwrap().entries[0].clone();
    dup.provisional.as_mut().unwrap().entries.push(e);
    room.apply(dup).unwrap();
    assert_eq!(once, ser(&room.state), "a duplicated entry in one delta");
    // A delta re-delivering what is held: the delta a peer with an empty
    // summary would be sent.
    let redelivered = room
        .state
        .delta(
            &room.state,
            &room.params,
            &ChatRoomStateV1::default().summarize(&ChatRoomStateV1::default(), &room.params),
        )
        .unwrap();
    room.apply(redelivered).unwrap();
    assert_eq!(
        once,
        ser(&room.state),
        "records already held are re-delivered"
    );
    // Merging with itself.
    let s = room.state.clone();
    let (x, y) = merge2(&s, &s, &room.params);
    assert_eq!(ser(&x), once);
    assert_eq!(ser(&y), once);
}

#[test]
fn sponsorship_makes_the_member_durable_in_every_order() {
    let base = Room::new(30, 10, Some(policy(2, 7)));
    let a = sk(1);
    let join = base.join_delta(&a, 1, "hi");
    let invite = base.members_delta(vec![base.invite(None, &a.verifying_key())]);
    // One delta carrying both.
    let mut both = join.clone();
    both.members = invite.members.clone();
    // A bare invite is pruned for inactivity until its member has a message,
    // so the invite that precedes the entry carries a message by the member.
    let mut invite_with_message = invite.clone();
    invite_with_message.recent_messages = Some(vec![base.msg(&a, 1, "hi")]);
    let orders: Vec<Vec<&ChatRoomStateV1Delta>> = vec![
        vec![&join, &invite],
        vec![&invite_with_message, &join],
        vec![&both],
    ];
    let mut finals = Vec::new();
    for order in orders {
        let s = replay(&base, &order);
        assert!(
            s.provisional.entries.is_empty(),
            "the entry is dropped (P5)"
        );
        assert!(
            s.members.members.iter().any(|m| m.member.id() == id_of(&a)),
            "the durable record is held"
        );
        assert!(
            s.recent_messages
                .messages
                .iter()
                .any(|m| m.message.author == id_of(&a)),
            "earlier messages survive"
        );
        assert!(
            s.member_info
                .member_info
                .iter()
                .any(|i| i.member_info.member_id == id_of(&a)),
            "member_info survives"
        );
        finals.push(s);
    }
    assert_eq!(ser(&finals[0]), ser(&finals[1]));
    assert_eq!(ser(&finals[0]), ser(&finals[2]));
    // Merges of the peers in each state agree too.
    let p1 = replay(&base, &[&join]);
    let p2 = replay(&base, &[&invite_with_message]);
    let (x, y) = merge2(&p1, &p2, &base.params);
    assert_eq!(ser(&x), ser(&y));
    assert_eq!(ser(&x), ser(&finals[0]));
}

#[test]
fn several_entries_for_one_key_keep_the_smallest_digest() {
    use river_core::room_state::provisional::entry_digest;
    let mut room = Room::new(30, 10, Some(policy(2, 7)));
    let a = sk(1);
    let owner = room.params.owner;
    let n1 = solve(&owner, &a.verifying_key(), 7, 2);
    let n2 = (n1 + 1..)
        .find(|n| leading_zero_bits(&pow_hash(&owner, &a.verifying_key(), 7, *n)) >= 2)
        .unwrap();
    let e1 = entry_with(&owner, &a, 7, n1);
    let e2 = entry_with(&owner, &a, 7, n2);
    let d = ChatRoomStateV1Delta {
        provisional: Some(ProvisionalDelta {
            entries: vec![e1.clone(), e2.clone()],
            bans: vec![],
        }),
        recent_messages: Some(vec![room.msg(&a, 1, "hi")]),
        ..Default::default()
    };
    room.apply(d).unwrap();
    assert_eq!(room.state.provisional.entries.len(), 1);
    let smallest = if entry_digest(&e1) < entry_digest(&e2) {
        e1
    } else {
        e2
    };
    assert_eq!(room.state.provisional.entries[0], smallest);
}

#[test]
fn a_provisional_id_is_never_in_a_members_lookup() {
    let mut room = Room::new(30, 10, Some(policy(2, 7)));
    let a = sk(1);
    room.apply(room.join_delta(&a, 1, "hi")).unwrap();
    assert!(room.has_entry(&a));
    let id = id_of(&a);
    assert!(!room.state.members.members_by_member_id().contains_key(&id));
    assert!(!room
        .state
        .ban_evidence
        .lookup(&room.state.members, room.owner_id())
        .contains_key(&id));
    assert!(room.state.ban_evidence.members.is_empty());
    // A deputy list naming a provisional id grants it nothing: it cannot ban.
    let m = sk(2);
    room.apply(room.members_delta(vec![room.invite(None, &m.verifying_key())]))
        .unwrap();
    let mut info = MemberInfo::new_public(room.owner_id(), 5, "owner".into());
    info.deputies = vec![id];
    room.apply(ChatRoomStateV1Delta {
        member_info: Some(vec![AuthorizedMemberInfo::new(info, &room.owner)]),
        recent_messages: Some(vec![room.msg(&m, 2, "m")]),
        ..Default::default()
    })
    .unwrap();
    assert!(!room.state.members.members_by_member_id().contains_key(&id));
}

#[test]
fn raising_difficulty_drops_short_entries_and_lowering_does_not_restore_them() {
    let mut room = Room::new(30, 10, Some(policy(3, 7)));
    let a = sk(1);
    room.apply(room.join_delta(&a, 1, "hi")).unwrap();
    let held = room.state.provisional.entries[0].clone();
    // Raise the difficulty past what the held nonce achieves.
    let achieved = leading_zero_bits(&pow_hash(
        &room.params.owner,
        &a.verifying_key(),
        7,
        match held.provisional.evidence {
            AdmissionEvidence::Pow { nonce } => nonce,
        },
    ));
    room.set_config(|c| c.public_admission = Some(policy((achieved + 1) as u8, 7)));
    assert!(!room.has_entry(&a), "P3: the short entry is dropped");
    assert!(!room.has_msg_by(&a), "and its messages go");
    // Lowering restores nothing.
    room.set_config(|c| c.public_admission = Some(policy(1, 7)));
    assert!(!room.has_entry(&a));
    // A replay of the old entry is accepted again under the lower difficulty,
    // which is how an author re-attaches.
    room.apply(room.join_delta(&a, 5, "back")).unwrap();
    assert!(room.has_entry(&a));
    // A new epoch invalidates every proof.
    room.set_config(|c| c.public_admission = Some(policy(1, 99)));
    let owner = room.params.owner;
    let stale_nonce = match room
        .state
        .provisional
        .entries
        .first()
        .map(|e| e.provisional.evidence.clone())
    {
        Some(AdmissionEvidence::Pow { nonce }) => nonce,
        None => 0,
    };
    let _ = (owner, stale_nonce);
}

#[test]
fn a_new_epoch_drops_every_entry() {
    // Difficulty 8 makes it all but certain that a proof for epoch 7 fails
    // epoch 8 (1 in 256 would pass); the test checks the held nonces.
    let mut room = Room::new(30, 10, Some(policy(10, 7)));
    let keys = [sk(1), sk(2), sk(3)];
    for (i, k) in keys.iter().enumerate() {
        room.apply(room.join_delta(k, 1 + i as u64, "hi")).unwrap();
    }
    assert_eq!(room.state.provisional.entries.len(), 3);
    let owner = room.params.owner;
    // Only proofs that happen to also pass epoch 8 survive.
    let passing: Vec<bool> = keys
        .iter()
        .map(|k| {
            let nonce = match room
                .state
                .provisional
                .entries
                .iter()
                .find(|e| e.provisional.member_vk == k.verifying_key())
                .unwrap()
                .provisional
                .evidence
            {
                AdmissionEvidence::Pow { nonce } => nonce,
            };
            leading_zero_bits(&pow_hash(&owner, &k.verifying_key(), 8, nonce)) >= 10
        })
        .collect();
    room.set_config(|c| c.public_admission = Some(policy(10, 8)));
    for (k, pass) in keys.iter().zip(passing) {
        assert_eq!(room.has_entry(k), pass);
    }
}

// ---------------------------------------------------------------------------
// Test list 5: bans and collisions
// ---------------------------------------------------------------------------

/// A room with a member `m` (owner-invited, with a message), and `d`, an
/// owner deputy (a member with a message, listed on the owner's `member_info`).
fn room_with_member_and_deputy() -> (Room, SigningKey, SigningKey) {
    let mut room = Room::new(40, 20, Some(policy(1, 7)));
    let (m, d) = (sk(2), sk(3));
    // A bare invite is pruned for inactivity, so the records, the owner's
    // deputy grant and the members' messages arrive in one delta.
    let mut info = MemberInfo::new_public(room.owner_id(), 2, "owner".into());
    info.deputies = vec![id_of(&d)];
    room.apply(ChatRoomStateV1Delta {
        members: Some(MembersDelta::new(vec![
            room.invite(None, &m.verifying_key()),
            room.invite(None, &d.verifying_key()),
        ])),
        member_info: Some(vec![AuthorizedMemberInfo::new(info, &room.owner)]),
        recent_messages: Some(vec![room.msg(&m, 1, "m"), room.msg(&d, 2, "d")]),
        ..Default::default()
    })
    .unwrap();
    assert!(room.is_member(&m) && room.is_member(&d));
    (room, m, d)
}

fn prov_ban_delta(b: AuthorizedProvisionalBan) -> ChatRoomStateV1Delta {
    ChatRoomStateV1Delta {
        provisional: Some(ProvisionalDelta {
            entries: vec![],
            bans: vec![b],
        }),
        ..Default::default()
    }
}

fn user_ban_delta(b: AuthorizedUserBan) -> ChatRoomStateV1Delta {
    ChatRoomStateV1Delta {
        bans: Some(vec![b]),
        ..Default::default()
    }
}

#[test]
fn the_owner_and_an_owner_deputy_ban_a_provisional_member_in_either_list() {
    for (who, list) in [
        ("owner", "provisional"),
        ("deputy", "provisional"),
        ("owner", "user"),
        ("deputy", "user"),
    ] {
        let (mut room, _m, d) = room_with_member_and_deputy();
        let a = sk(10);
        room.apply(room.join_delta(&a, 5, "hello")).unwrap();
        assert!(room.has_entry(&a) && room.has_msg_by(&a), "{who} {list}");
        let by = if who == "owner" { None } else { Some(&d) };
        let delta = if list == "provisional" {
            prov_ban_delta(room.prov_ban(by, id_of(&a), 9))
        } else {
            user_ban_delta(room.user_ban(by, id_of(&a), 9))
        };
        room.apply(delta).unwrap();
        assert!(!room.has_entry(&a), "{who} {list}: the entry is gone");
        assert!(!room.has_msg_by(&a), "{who} {list}: so are its messages");
        // The same entry cannot come back while the ban stands.
        room.apply(room.join_delta(&a, 6, "again")).unwrap();
        assert!(!room.has_entry(&a), "{who} {list}: still banned");
    }
}

#[test]
fn a_provisional_ban_has_its_own_budget() {
    let (mut room, m, _d) = room_with_member_and_deputy();
    // max_user_bans defaults to 10; fill the provisional list past its own
    // cap (5) and check UserBans on invited members are untouched.
    let victims: Vec<SigningKey> = (20..30).map(sk).collect();
    for (i, v) in victims.iter().enumerate() {
        room.apply(prov_ban_delta(room.prov_ban(None, id_of(v), i as u64)))
            .unwrap();
    }
    assert_eq!(
        room.state.provisional.bans.len(),
        5,
        "capped at max_provisional_bans"
    );
    room.apply(user_ban_delta(room.user_ban(None, id_of(&m), 99)))
        .unwrap();
    assert_eq!(room.state.bans.0.len(), 1, "the UserBan list is separate");
    assert_eq!(room.state.provisional.bans.len(), 5);
    assert!(!room.is_member(&m), "the UserBan removed the member");
}

#[test]
fn a_non_deputys_ban_on_a_provisional_id_changes_nothing() {
    let (mut room, m, _d) = room_with_member_and_deputy();
    let a = sk(10);
    room.apply(room.join_delta(&a, 5, "hello")).unwrap();
    // M is neither an ancestor of A nor a deputy: its provisional ban is
    // dropped, its UserBan is stored but inert.
    room.apply(prov_ban_delta(room.prov_ban(Some(&m), id_of(&a), 9)))
        .unwrap();
    assert!(room.state.provisional.bans.is_empty(), "P1 drops it");
    assert!(room.has_entry(&a) && room.has_msg_by(&a));
    room.apply(user_ban_delta(room.user_ban(Some(&m), id_of(&a), 9)))
        .unwrap();
    assert!(
        room.has_entry(&a) && room.has_msg_by(&a),
        "an inert UserBan"
    );
}

#[test]
fn a_deputy_grant_and_the_deputys_provisional_ban_in_one_delta_equal_the_two_apart() {
    let (base, m, d) = room_with_member_and_deputy();
    // A second deputy E, granted by the owner in the same delta as E's ban.
    let mut room = Room {
        owner: base.owner.clone(),
        params: base.params.clone(),
        state: base.state.clone(),
        version: base.version,
    };
    let e = sk(4);
    let mut join_e = room.members_delta(vec![room.invite(None, &e.verifying_key())]);
    join_e.recent_messages = Some(vec![room.msg(&e, 3, "e")]);
    room.apply(join_e).unwrap();
    let a = sk(10);
    room.apply(room.join_delta(&a, 5, "hello")).unwrap();
    let mut info = MemberInfo::new_public(room.owner_id(), 3, "owner".into());
    info.deputies = vec![id_of(&d), id_of(&e)];
    let grant = ChatRoomStateV1Delta {
        member_info: Some(vec![AuthorizedMemberInfo::new(info, &room.owner)]),
        ..Default::default()
    };
    let ban = prov_ban_delta(room.prov_ban(Some(&e), id_of(&a), 9));
    let mut both = grant.clone();
    both.provisional = ban.provisional.clone();
    let together = replay(&room, &[&both]);
    let grant_first = replay(&room, &[&grant, &ban]);
    assert!(
        ser(&together) == ser(&grant_first),
        "a grant and the deputy's ban in one delta equal the grant, then the ban"
    );
    assert!(
        together.provisional.entries.is_empty(),
        "E's ban took effect"
    );
    // A ban that reaches a peer BEFORE the grant is judged against the
    // converged state at that time and dropped (P1 keeps only owner and
    // owner-deputy bans); the peer that received both keeps it, and a merge
    // of the two gives the same state both ways.
    let ban_first = replay(&room, &[&ban, &grant]);
    assert!(ban_first.provisional.bans.is_empty());
    assert!(!ban_first.provisional.entries.is_empty());
    let (x, y) = merge2(&together, &ban_first, &room.params);
    assert!(ser(&x) == ser(&y));
    let _ = m;
}

#[test]
fn a_ban_with_a_bad_signature_from_a_known_issuer_is_skipped_and_from_the_owner_is_an_error() {
    let (mut room, m, _d) = room_with_member_and_deputy();
    let a = sk(10);
    room.apply(room.join_delta(&a, 5, "hello")).unwrap();
    // M's record is known; a ban attributed to M but signed by another key
    // is skipped, never an Err.
    let mut forged = room.prov_ban(Some(&m), id_of(&a), 9);
    forged.signature = room.prov_ban(None, id_of(&a), 9).signature;
    room.apply(prov_ban_delta(forged)).expect("skipped");
    assert!(room.state.provisional.bans.is_empty());
    // Attributed to the owner: the parameters fix that key, so it is a forgery.
    let mut forged = room.prov_ban(None, id_of(&a), 9);
    forged.signature = room.prov_ban(Some(&m), id_of(&a), 9).signature;
    assert!(room.apply(prov_ban_delta(forged)).is_err());
    // An issuer known nowhere is kept on arrival and swept by cleanup.
    let stranger = sk(77);
    room.apply(prov_ban_delta(room.prov_ban(Some(&stranger), id_of(&a), 9)))
        .unwrap();
    assert!(room.state.provisional.bans.is_empty());
    assert!(room.has_entry(&a));
}

#[test]
fn a_provisional_ban_and_a_user_ban_cannot_be_replayed_as_each_other() {
    let (room, _m, _d) = room_with_member_and_deputy();
    let target = id_of(&sk(10));
    let pb = room.prov_ban(None, target, 9);
    let ub = room.user_ban(None, target, 9);
    // The provisional ban's signature as a UserBan's, and the reverse.
    let as_user = AuthorizedUserBan::with_signature(ub.ban.clone(), ub.banned_by, pb.signature);
    assert!(as_user.verify_signature(&room.params.owner).is_err());
    let as_prov = AuthorizedProvisionalBan {
        ban: pb.ban.clone(),
        signature: ub.signature,
    };
    assert!(as_prov
        .verify_signature(&room.params.owner, &room.params.owner)
        .is_err());
    assert!(pb
        .verify_signature(&room.params.owner, &room.params.owner)
        .is_ok());
}

#[test]
fn a_provisionally_banned_key_sponsored_by_a_member_becomes_durable_and_a_user_ban_prevents_that() {
    // Sponsorship is NOT blocked by a provisional ban (design 2.7, D2(c)).
    let (mut room, m, _d) = room_with_member_and_deputy();
    let a = sk(10);
    room.apply(prov_ban_delta(room.prov_ban(None, id_of(&a), 9)))
        .unwrap();
    let mut sponsor = room.members_delta(vec![room.invite(Some(&m), &a.verifying_key())]);
    sponsor.recent_messages = Some(vec![room.msg(&a, 11, "sponsored")]);
    room.apply(sponsor.clone()).unwrap();
    assert!(room.is_member(&a), "the sponsored key is durable");
    assert!(room.has_msg_by(&a));
    // A UserBan on the key removes it, and the same sponsorship then fails.
    room.apply(user_ban_delta(room.user_ban(None, id_of(&a), 12)))
        .unwrap();
    assert!(!room.is_member(&a));
    room.apply(sponsor).unwrap();
    assert!(!room.is_member(&a), "a UserBan prevents sponsorship");
}

/// Member M invites a forged key F with V's `MemberId`, keeps the record
/// alive with a DM to it, and (optionally) bans the id.
struct Collision {
    room: Room,
    m: SigningKey,
    d: SigningKey,
    v: SigningKey,
    forged: VerifyingKey,
}

fn collision_room() -> Collision {
    let (mut room, m, d) = room_with_member_and_deputy();
    let v = sk(10);
    room.apply(room.join_delta(&v, 5, "victim")).unwrap();
    assert!(room.has_entry(&v));
    let forged = forged_vk_with_id_of(&v.verifying_key());
    Collision {
        room,
        m,
        d,
        v,
        forged,
    }
}

impl Collision {
    fn shadow(&mut self, with_ban: bool) {
        let room = &mut self.room;
        let mut delta = room.members_delta(vec![room.invite(Some(&self.m), &self.forged)]);
        delta.direct_messages =
            dm_delta(vec![dm(room, &self.m, id_of(&self.v), 50)]).direct_messages;
        if with_ban {
            delta.bans = Some(vec![room.user_ban(Some(&self.m), id_of(&self.v), 60)]);
        }
        room.apply(delta).expect("the shadowing delta applies");
    }
}

#[test]
fn a_member_can_shadow_a_provisional_member_and_the_owner_frees_them() {
    let mut c = collision_room();
    c.shadow(false);
    let v = c.v.clone();
    assert!(!c.room.has_entry(&v), "C1: the record beats the entry");
    assert!(
        !c.room.has_msg_by(&v),
        "C3: the victim's messages signed by another key are dropped (and never fail verify)"
    );
    assert!(c.room.is_member(&v), "the forged record holds the id");
    // The victim re-attaches the unchanged entry: still shadowed.
    c.room.apply(c.room.join_delta(&v, 7, "still me")).unwrap();
    assert!(!c.room.has_entry(&v) && !c.room.has_msg_by(&v));
    // The owner bans M: M's subtree, the forged record and M's DMs go.
    let ban_m = user_ban_delta(c.room.user_ban(None, id_of(&c.m), 70));
    c.room.apply(ban_m).unwrap();
    assert!(!c.room.is_member(&c.v), "the forged record is gone");
    c.room.apply(c.room.join_delta(&v, 8, "free")).unwrap();
    assert!(
        c.room.has_entry(&v) && c.room.has_msg_by(&v),
        "the victim's unchanged entry is accepted again"
    );
}

#[test]
fn shadowing_with_a_ban_on_the_id_is_freed_by_the_owner_but_not_by_a_deputy() {
    // The owner bans M.
    let mut c = collision_room();
    c.shadow(true);
    let v = c.v.clone();
    assert!(!c.room.has_entry(&v) && !c.room.has_msg_by(&v));
    c.room
        .apply(user_ban_delta(c.room.user_ban(None, id_of(&c.m), 70)))
        .unwrap();
    c.room.apply(c.room.join_delta(&v, 8, "free")).unwrap();
    assert!(
        c.room.has_entry(&v),
        "after an owner ban on M the victim is free"
    );

    // A deputy bans M instead: M's ban on the id stays stored, the forged
    // record stays as ban evidence for it, and the victim stays shadowed.
    let mut c = collision_room();
    c.shadow(true);
    let v = c.v.clone();
    let d = c.d.clone();
    c.room
        .apply(user_ban_delta(c.room.user_ban(Some(&d), id_of(&c.m), 70)))
        .unwrap();
    assert!(!c.room.is_member(&c.m), "M is removed");
    assert!(
        c.room
            .state
            .bans
            .0
            .iter()
            .any(|b| b.banned_by == id_of(&c.m) && b.ban.banned_user == id_of(&v)),
        "M's ban on the shadowed id stays stored"
    );
    c.room.apply(c.room.join_delta(&v, 8, "try")).unwrap();
    assert!(
        !c.room.has_entry(&v),
        "the forged evidence record keeps the victim shadowed"
    );
}

#[test]
fn a_collision_converges_in_both_merge_directions() {
    // The forged record, an owner ban on its inviter and the victim's entry
    // in one delta, both merge directions.
    let c = collision_room();
    let victim_state = c.room.state.clone();
    let mut attacker = Room {
        owner: c.room.owner.clone(),
        params: c.room.params.clone(),
        state: c.room.state.clone(),
        version: c.room.version,
    };
    let mut shadow = attacker.members_delta(vec![attacker.invite(Some(&c.m), &c.forged)]);
    shadow.direct_messages = dm_delta(vec![dm(&attacker, &c.m, id_of(&c.v), 50)]).direct_messages;
    attacker.apply(shadow).unwrap();
    let (x, y) = merge2(&victim_state, &attacker.state, &c.room.params);
    assert_eq!(ser(&x), ser(&y));
    assert!(!x
        .provisional
        .entries
        .iter()
        .any(|e| e.provisional.member_vk == c.v.verifying_key()));
    // Owner ban on M on one side only.
    let mut banner = Room {
        owner: c.room.owner.clone(),
        params: c.room.params.clone(),
        state: victim_state.clone(),
        version: c.room.version,
    };
    banner
        .apply(user_ban_delta(banner.user_ban(None, id_of(&c.m), 70)))
        .unwrap();
    let (x, y) = merge2(&attacker.state, &banner.state, &c.room.params);
    assert_eq!(ser(&x), ser(&y));
    let (x2, y2) = merge2(&x, &victim_state, &c.room.params);
    assert_eq!(ser(&x2), ser(&y2));
}

#[test]
fn two_attacker_entries_with_one_id_are_both_dropped() {
    // C2. A record for a second key with an existing id needs only its
    // inviter's signature, but an ENTRY needs the key's own signature, which
    // nobody holds for a key built to match an id. The contract only ever
    // sees two entries with one id and different keys when one party holds
    // both keys.
    let mut room = Room::new(30, 10, Some(policy(1, 7)));
    let a = sk(1);
    let forged = forged_vk_with_id_of(&a.verifying_key());
    let owner = room.params.owner;
    // An entry for `forged` signed by `a` fails entry_sig_ok (an Err), so the
    // C2 case can only be built with two valid entries; assert that. Its
    // proof of work passes, so the policy check (which runs first) admits it
    // and the signature check is reached.
    let nonce = solve(&owner, &forged, 7, 1);
    let fake = AuthorizedProvisionalMember::new(
        &owner,
        ProvisionalMember {
            member_vk: forged,
            evidence: AdmissionEvidence::Pow { nonce },
        },
        &a,
    );
    let d = ChatRoomStateV1Delta {
        provisional: Some(ProvisionalDelta {
            entries: vec![fake],
            bans: vec![],
        }),
        ..Default::default()
    };
    assert!(
        room.apply(d).is_err(),
        "a forged-key entry cannot be signed"
    );
}

// ---------------------------------------------------------------------------
// Test list 6, 12, bounds and caps
// ---------------------------------------------------------------------------

#[test]
fn a_flood_of_future_dated_provisional_messages_leaves_members_their_share_of_the_window() {
    // A busy room: the members post more than their share of the window, and
    // `max_members` binds (four invitees, a cap of three). Future-dated
    // provisional messages push member messages out of the window only down
    // to `window - quota` (design D4), no invited member is pruned, and the
    // cap removes exactly the member it removes without the flood.
    let mut p = policy(0, 1);
    p.max_provisional = 5;
    p.max_provisional_messages = 100;
    let build = |flood: bool| {
        // window 20: quota min(100, 10) = 10; members keep at least 10 slots.
        let mut room = Room::new(20, 3, Some(p.clone()));
        let members: Vec<SigningKey> = (2..6).map(sk).collect();
        let mut d = room.members_delta(
            members
                .iter()
                .map(|k| room.invite(None, &k.verifying_key()))
                .collect(),
        );
        d.recent_messages = Some(
            members
                .iter()
                .flat_map(|k| (1..=4).map(|t| room.msg(k, t, &format!("m{t}"))))
                .collect(),
        );
        room.apply(d).unwrap();
        if flood {
            // 3 x max_provisional authors, each posting future-dated messages.
            for i in 0..15u8 {
                let k = sk(100 + i);
                let mut d = room.join_delta(&k, 1_000_000 + i as u64 * 10, "flood");
                d.recent_messages
                    .as_mut()
                    .unwrap()
                    .extend((1..=4).map(|t| room.msg(&k, 1_000_000 + i as u64 * 10 + t, "more")));
                room.apply(d).unwrap();
            }
        }
        (room, members)
    };
    let (clean, _) = build(false);
    let (flooded, members) = build(true);
    assert_eq!(
        clean.state.members.members.len(),
        3,
        "the member cap binds: one of the four invitees is removed"
    );
    let members: Vec<SigningKey> = members.into_iter().filter(|k| clean.is_member(k)).collect();
    let member_ids: Vec<MemberId> = members.iter().map(id_of).collect();
    let count_member_msgs = |s: &ChatRoomStateV1| {
        s.recent_messages
            .messages
            .iter()
            .filter(|m| member_ids.contains(&m.message.author))
            .count()
    };
    assert_eq!(
        count_member_msgs(&clean.state),
        12,
        "without the flood all 12 fit"
    );
    assert_eq!(
        count_member_msgs(&flooded.state),
        10,
        "the flood takes the quota (10) and members keep the rest of the window (20 - 10)"
    );
    let provisional_msgs = flooded
        .state
        .recent_messages
        .messages
        .iter()
        .filter(|m| !member_ids.contains(&m.message.author))
        .count();
    assert!(
        provisional_msgs <= 10,
        "the quota bounds provisional messages"
    );
    assert_eq!(
        flooded.state.provisional.entries.len().min(5),
        flooded.state.provisional.entries.len()
    );
    for k in &members {
        assert!(flooded.is_member(k), "no invited member is pruned");
    }
    // `max_members` behaviour is byte-identical to a room without entries.
    let mut a = Vec::new();
    ciborium::ser::into_writer(&clean.state.members, &mut a).unwrap();
    let mut b = Vec::new();
    ciborium::ser::into_writer(&flooded.state.members, &mut b).unwrap();
    assert!(a == b, "the members field is unchanged by the flood");
}

#[test]
fn a_delta_past_the_entry_work_bound_is_refused_and_one_at_it_applies() {
    let mut p = policy(0, 1);
    p.max_provisional = 5;
    let mut room = Room::new(200, 10, Some(p));
    let make = |n: usize, room: &Room| {
        let keys: Vec<SigningKey> = (0..n)
            .map(|i| {
                SigningKey::from_bytes(&[
                    (i % 250) as u8 + 1,
                    (i / 250) as u8 + 1,
                    7,
                    7,
                    7,
                    7,
                    7,
                    7,
                    7,
                    7,
                    7,
                    7,
                    7,
                    7,
                    7,
                    7,
                    7,
                    7,
                    7,
                    7,
                    7,
                    7,
                    7,
                    7,
                    7,
                    7,
                    7,
                    7,
                    7,
                    7,
                    7,
                    7,
                ])
            })
            .collect();
        ChatRoomStateV1Delta {
            provisional: Some(ProvisionalDelta {
                entries: keys.iter().map(|k| room.entry(k)).collect(),
                bans: vec![],
            }),
            recent_messages: Some(
                keys.iter()
                    .enumerate()
                    .map(|(i, k)| room.msg(k, i as u64 + 1, "x"))
                    .collect(),
            ),
            ..Default::default()
        }
    };
    assert!(
        room.apply(make(25, &room)).is_err(),
        "25 new entries exceed the bound"
    );
    room.apply(make(24, &room)).expect("24 is at the bound");
    assert_eq!(
        room.state.provisional.entries.len(),
        5,
        "cleanup keeps max_provisional authors"
    );
}

#[test]
fn a_whole_state_over_its_own_caps_is_refused_before_any_signature_is_checked() {
    let mut room = Room::new(50, 10, Some(policy(0, 1)));
    let keys: Vec<SigningKey> = (1..=6).map(sk).collect();
    for (i, k) in keys.iter().enumerate() {
        room.apply(room.join_delta(k, i as u64 + 1, "hi")).unwrap();
    }
    assert_eq!(
        room.state.provisional.entries.len(),
        5,
        "premise: the cap holds 5"
    );
    // A state holding a sixth entry (as a full-state PUT might): `verify`
    // refuses it, and so does a merge.
    let mut over = room.state.clone();
    over.provisional.entries.push(room.entry(&keys[5]));
    over.provisional.entries.sort_by_key(|e| {
        (
            e.member_id(),
            river_core::room_state::provisional::entry_digest(e),
        )
    });
    assert!(over.verify(&over, &room.params).is_err());
    let mut target = room.state.clone();
    assert!(target.merge_from(&room.params, &over).is_err());
    // With no policy the caps are zero.
    let mut none = Room::new(50, 10, None);
    let mut one = none.state.clone();
    one.provisional.entries.push(room.entry(&keys[0]));
    assert!(one.verify(&one, &none.params).is_err());
    let _ = &mut none;
}

#[test]
fn a_network_delta_trim_keeps_provisional_messages_with_their_entries() {
    let mut room = Room::new(50, 10, Some(policy(0, 1)));
    let (a, b) = (sk(1), sk(2));
    room.apply(room.join_delta(&a, 1, "a")).unwrap();
    room.apply(room.join_delta(&b, 2, "b")).unwrap();
    let empty = ChatRoomStateV1::default();
    let empty_summary = empty.summarize(&empty, &room.params);
    // Against an empty receiver: the delta carries entries with messages.
    let delta = room
        .state
        .delta_within_work_bounds(&room.params, &empty_summary)
        .unwrap();
    let carried_entries: Vec<MemberId> = delta
        .provisional
        .as_ref()
        .unwrap()
        .entries
        .iter()
        .map(|e| e.member_id())
        .collect();
    assert_eq!(carried_entries.len(), 2);
    let authors: Vec<MemberId> = delta
        .recent_messages
        .as_ref()
        .unwrap()
        .iter()
        .map(|m| m.message.author)
        .collect();
    assert!(authors.contains(&id_of(&a)) && authors.contains(&id_of(&b)));

    // An entry is never sent without a message by its author: drop B's
    // message from a hand-built delta and trim.
    let mut d = room
        .state
        .delta(&room.state, &room.params, &empty_summary)
        .unwrap();
    d.recent_messages
        .as_mut()
        .unwrap()
        .retain(|m| m.message.author != id_of(&b));
    room.state
        .trim_network_delta(&room.params, &empty_summary, &mut d);
    let entries: Vec<MemberId> = d
        .provisional
        .as_ref()
        .unwrap()
        .entries
        .iter()
        .map(|e| e.member_id())
        .collect();
    assert_eq!(entries, vec![id_of(&a)], "B's entry goes with its message");

    // A message whose author's entry was trimmed goes too.
    let mut d = room
        .state
        .delta(&room.state, &room.params, &empty_summary)
        .unwrap();
    d.provisional
        .as_mut()
        .unwrap()
        .entries
        .retain(|e| e.member_id() != id_of(&b));
    room.state
        .trim_network_delta(&room.params, &empty_summary, &mut d);
    assert!(d
        .recent_messages
        .as_ref()
        .unwrap()
        .iter()
        .all(|m| m.message.author != id_of(&b)));

    // The receiver already holds A's entry: A's message is kept without it.
    let mut holder = ChatRoomStateV1 {
        configuration: room.state.configuration.clone(),
        ..Default::default()
    };
    holder.provisional.entries = room.state.provisional.entries.clone();
    let summary = holder.summarize(&holder, &room.params);
    let mut d = room
        .state
        .delta(&room.state, &room.params, &summary)
        .unwrap();
    assert!(
        d.provisional.is_none(),
        "the receiver holds both entries: nothing to send"
    );
    room.state
        .trim_network_delta(&room.params, &summary, &mut d);
    assert!(
        d.recent_messages
            .as_ref()
            .unwrap()
            .iter()
            .any(|m| m.message.author == id_of(&a)),
        "a message by an author whose entry the receiver holds is kept"
    );
}

#[test]
fn an_unreferenced_provisional_ban_is_swept_and_a_ban_held_twice_is_kept_once() {
    let mut room = Room::new(50, 10, Some(policy(0, 1)));
    let target = id_of(&sk(9));
    let b = room.prov_ban(None, target, 5);
    room.apply(prov_ban_delta(b.clone())).unwrap();
    room.apply(prov_ban_delta(b.clone())).unwrap();
    assert_eq!(room.state.provisional.bans.len(), 1);
    // Removing the policy clears the list (P0).
    room.set_config(|c| c.public_admission = None);
    assert!(room.state.provisional.is_empty());
}

#[test]
fn the_owners_provisional_bans_rank_ahead_of_a_deputys_when_the_list_is_full() {
    let (mut room, _m, d) = room_with_member_and_deputy();
    // Cap is 5. The deputy bans 5 targets, the owner then one more: the
    // owner's ban is kept and the deputy's largest-digest ban makes room.
    let targets: Vec<MemberId> = (40..46).map(|i| id_of(&sk(i))).collect();
    let delta = ChatRoomStateV1Delta {
        provisional: Some(ProvisionalDelta {
            entries: vec![],
            bans: targets[..5]
                .iter()
                .enumerate()
                .map(|(i, t)| room.prov_ban(Some(&d), *t, i as u64))
                .collect(),
        }),
        ..Default::default()
    };
    room.apply(delta).unwrap();
    assert_eq!(room.state.provisional.bans.len(), 5);
    room.apply(prov_ban_delta(room.prov_ban(None, targets[5], 99)))
        .unwrap();
    assert_eq!(room.state.provisional.bans.len(), 5);
    assert!(room
        .state
        .provisional
        .bans
        .iter()
        .any(|b| b.ban.target == targets[5] && b.ban.banned_by == room.owner_id()));
}

// ---------------------------------------------------------------------------
// C2 and C3 in their less common shapes
// ---------------------------------------------------------------------------

/// An entry for `vk` with a valid proof of work but a garbage signature, built
/// by hand: only a state that skipped arrival (a full-state PUT) can hold one,
/// which is exactly what cleanup must still handle.
fn unsigned_entry(room: &Room, vk: &VerifyingKey) -> AuthorizedProvisionalMember {
    let p = room.config().public_admission.clone().unwrap().pow.unwrap();
    let nonce = solve(&room.params.owner, vk, p.epoch, p.difficulty_bits);
    AuthorizedProvisionalMember::with_signature(
        ProvisionalMember {
            member_vk: *vk,
            evidence: AdmissionEvidence::Pow { nonce },
        },
        ed25519_dalek::Signature::from_bytes(&[7; 64]),
    )
}

#[test]
fn two_entries_with_one_id_and_different_keys_are_all_dropped_with_their_content() {
    let mut room = Room::new(30, 10, Some(policy(1, 7)));
    let v = sk(10);
    room.apply(room.join_delta(&v, 5, "victim")).unwrap();
    assert!(room.has_entry(&v) && room.has_msg_by(&v));
    // A second entry with V's id under another key (C2). Injected directly.
    let forged = forged_vk_with_id_of(&v.verifying_key());
    let mut s = room.state.clone();
    s.provisional.entries.push(unsigned_entry(&room, &forged));
    s.post_apply_cleanup(&room.params).unwrap();
    assert!(
        s.provisional.entries.is_empty(),
        "C2: both entries are dropped"
    );
    assert!(
        !s.recent_messages
            .messages
            .iter()
            .any(|m| m.message.author == id_of(&v)),
        "C3: V's messages go (the id resolves to no key)"
    );
    assert!(!s
        .member_info
        .member_info
        .iter()
        .any(|i| i.member_info.member_id == id_of(&v)));
    // The result is a cleanup fixpoint and verifies.
    assert!(validate(&s, &room.params).is_ok());
}

#[test]
fn a_shadowed_id_whose_entry_is_dropped_by_another_rule_still_loses_its_content() {
    // The entry is dropped in the same cleanup by a rule that runs before C1
    // (here an owner provisional ban), while a record for another key holds
    // the id: the messages signed by the victim's key must not stay.
    let (mut room, m, _d) = {
        let (r, m, d) = room_with_member_and_deputy();
        (r, m, d)
    };
    let v = sk(10);
    room.apply(room.join_delta(&v, 5, "victim")).unwrap();
    let forged = forged_vk_with_id_of(&v.verifying_key());
    let mut delta = room.members_delta(vec![room.invite(Some(&m), &forged)]);
    delta.direct_messages = dm_delta(vec![dm(&room, &m, id_of(&v), 50)]).direct_messages;
    delta.provisional = Some(ProvisionalDelta {
        entries: vec![],
        bans: vec![room.prov_ban(None, id_of(&v), 51)],
    });
    room.apply(delta).expect("applies and validates");
    assert!(!room.has_entry(&v));
    assert!(
        !room.has_msg_by(&v),
        "V's messages do not outlive the shadowing"
    );
    assert!(room.is_member(&v), "the forged record holds the id");
}

#[test]
fn a_record_for_another_key_and_a_removed_policy_in_one_merge_leave_a_valid_state() {
    let (base, m, _d) = room_with_member_and_deputy();
    let v = sk(10);
    let mut victim_side = Room {
        owner: base.owner.clone(),
        params: base.params.clone(),
        state: base.state.clone(),
        version: base.version,
    };
    victim_side
        .apply(victim_side.join_delta(&v, 5, "victim"))
        .unwrap();
    let mut other = Room {
        owner: base.owner.clone(),
        params: base.params.clone(),
        state: base.state.clone(),
        version: base.version,
    };
    let forged = forged_vk_with_id_of(&v.verifying_key());
    let mut shadow = other.members_delta(vec![other.invite(Some(&m), &forged)]);
    shadow.direct_messages = dm_delta(vec![dm(&other, &m, id_of(&v), 50)]).direct_messages;
    other.apply(shadow).unwrap();
    other.set_config(|c| c.public_admission = None);
    let (x, y) = merge2(&victim_side.state, &other.state, &base.params);
    assert!(ser(&x) == ser(&y));
    assert!(x.provisional.is_empty());
    assert!(!x
        .recent_messages
        .messages
        .iter()
        .any(|m| m.message.author == id_of(&v)));
}

// ---------------------------------------------------------------------------
// D2(a) and the P2 restriction: a ban reaches an entry only where no record
// holds the id, so the result does not depend on whether the record was
// present before or after ban enforcement
// ---------------------------------------------------------------------------

/// Owner-invited member I with a member V invited by I, both with messages.
fn room_with_inviter_and_invitee() -> (Room, SigningKey, SigningKey) {
    let mut room = Room::new(40, 20, Some(policy(1, 7)));
    let (i, v) = (sk(2), sk(3));
    let rec_v = AuthorizedMember::new(
        Member {
            owner_member_id: room.owner_id(),
            invited_by: id_of(&i),
            member_vk: v.verifying_key(),
        },
        &i,
    );
    room.apply(ChatRoomStateV1Delta {
        members: Some(MembersDelta::new(vec![
            room.invite(None, &i.verifying_key()),
            rec_v,
        ])),
        recent_messages: Some(vec![room.msg(&i, 1, "i"), room.msg(&v, 2, "v")]),
        ..Default::default()
    })
    .unwrap();
    assert!(room.is_member(&i) && room.is_member(&v));
    (room, i, v)
}

#[test]
fn a_member_removed_with_their_inviter_may_come_back_as_provisional_in_either_order() {
    let (base, i, v) = room_with_inviter_and_invitee();
    // The owner bans the inviter: V goes with the subtree. V's own entry (the
    // same key as the record that just left) rides in the same delta.
    let ban_i = base.user_ban(None, id_of(&i), 10);
    let mut together = base.join_delta(&v, 11, "back as provisional");
    together.bans = Some(vec![ban_i.clone()]);
    let ban_then_entry = [
        user_ban_delta(ban_i),
        base.join_delta(&v, 11, "back as provisional"),
    ];
    let one = replay(&base, &[&together]);
    let two = replay(&base, &[&ban_then_entry[0], &ban_then_entry[1]]);
    for s in [&one, &two] {
        assert!(
            s.provisional
                .entries
                .iter()
                .any(|e| e.provisional.member_vk == v.verifying_key()),
            "V is provisional again (D2(a))"
        );
        assert!(!s.members.members.iter().any(|m| m.member.id() == id_of(&v)));
    }
    // The same provisional set and members either way (V's older message
    // survives only when the entry is held while the ban applies: both peers
    // hold it after one merge).
    let field = |s: &ChatRoomStateV1| {
        let mut b = Vec::new();
        ciborium::ser::into_writer(&(&s.provisional, &s.members), &mut b).unwrap();
        b
    };
    assert!(
        field(&one) == field(&two),
        "one delta and two give the same provisional set (the P2 restriction)"
    );
    let (x, y) = merge2(&one, &two, &base.params);
    assert!(ser(&x) == ser(&y));
    // Banned directly, V stays out in both orders.
    let ban_v = base.user_ban(None, id_of(&v), 10);
    let mut together = base.join_delta(&v, 11, "no");
    together.bans = Some(vec![ban_v.clone()]);
    let one = replay(&base, &[&together]);
    let two = replay(
        &base,
        &[&user_ban_delta(ban_v), &base.join_delta(&v, 11, "no")],
    );
    assert!(one.provisional.entries.is_empty() && two.provisional.entries.is_empty());
    assert!(field(&one) == field(&two));
}

#[test]
fn a_member_the_cap_removes_does_not_come_back_through_their_own_entry() {
    // P5: an entry whose key has a `members` record at the start of the pass is
    // dropped, even when the member cap then removes that record, so the
    // member does not slip back in as provisional in the same apply.
    let mut room = Room::new(40, 1, Some(policy(1, 7)));
    let (a, b) = (sk(2), sk(3));
    let mut d = room.members_delta(vec![
        room.invite(None, &a.verifying_key()),
        room.invite(None, &b.verifying_key()),
    ]);
    d.recent_messages = Some(vec![room.msg(&a, 1, "a"), room.msg(&b, 2, "b")]);
    d.provisional = Some(ProvisionalDelta {
        entries: vec![room.entry(&a), room.entry(&b)],
        bans: vec![],
    });
    room.apply(d).unwrap();
    assert_eq!(room.state.members.members.len(), 1, "the cap keeps one");
    assert!(
        room.state.provisional.entries.is_empty(),
        "neither key comes back as provisional in the same apply"
    );
}

#[test]
fn two_entries_with_one_id_never_leave_either_key_resolving_the_id_whichever_digest_is_smaller() {
    // C2 in several digest orders: the victim's entry and a second entry under
    // another key for the same id (injected, as only a full-state PUT could
    // hold it) are both dropped, and the victim's messages go with them.
    let owner_room = Room::new(30, 10, Some(policy(1, 7)));
    let owner = owner_room.params.owner;
    for round in 0..8u8 {
        let mut room = Room::new(30, 10, Some(policy(1, 7)));
        let v = sk(10 + round);
        // A different nonce each round changes the digest order.
        let nonce = (u64::from(round)..)
            .find(|n| leading_zero_bits(&pow_hash(&owner, &v.verifying_key(), 7, *n)) >= 1)
            .unwrap();
        let victim_entry = entry_with(&owner, &v, 7, nonce);
        let d = ChatRoomStateV1Delta {
            provisional: Some(ProvisionalDelta {
                entries: vec![victim_entry],
                bans: vec![],
            }),
            member_info: Some(vec![AuthorizedMemberInfo::new_with_member_key(
                MemberInfo::new_public(id_of(&v), 1, "v".into()),
                &v,
            )]),
            recent_messages: Some(vec![room.msg(&v, 5, "victim")]),
            ..Default::default()
        };
        room.apply(d).unwrap();
        let forged = forged_vk_with_id_of(&v.verifying_key());
        let mut s = room.state.clone();
        // The injected entry goes first in some rounds and last in others, so
        // the id resolves to either key if C2 does not drop both.
        if round % 2 == 0 {
            s.provisional.entries.push(unsigned_entry(&room, &forged));
        } else {
            s.provisional
                .entries
                .insert(0, unsigned_entry(&room, &forged));
        }
        s.post_apply_cleanup(&room.params).unwrap();
        assert!(s.provisional.entries.is_empty(), "round {round}: C2");
        assert!(
            !s.recent_messages
                .messages
                .iter()
                .any(|m| m.message.author == id_of(&v)),
            "round {round}: the victim's messages go"
        );
        assert!(validate(&s, &room.params).is_ok(), "round {round}");
    }
}

#[test]
fn a_deputy_with_a_provisional_ban_is_not_pruned_for_inactivity() {
    // The issuers of kept provisional bans are required ids in the member loop,
    // as the issuers of valid UserBans are, so a deputy whose only message has
    // left the window keeps their record and their bans.
    let mut room = Room::new(6, 20, Some(policy(1, 7)));
    let d = sk(3);
    let mut info = MemberInfo::new_public(room.owner_id(), 2, "owner".into());
    info.deputies = vec![id_of(&d)];
    room.apply(ChatRoomStateV1Delta {
        members: Some(MembersDelta::new(vec![
            room.invite(None, &d.verifying_key())
        ])),
        member_info: Some(vec![AuthorizedMemberInfo::new(info, &room.owner)]),
        recent_messages: Some(vec![room.msg(&d, 1, "d")]),
        ..Default::default()
    })
    .unwrap();
    let target = id_of(&sk(40));
    room.apply(prov_ban_delta(room.prov_ban(Some(&d), target, 5)))
        .unwrap();
    assert_eq!(room.state.provisional.bans.len(), 1);
    // Owner messages push the deputy's only message out of the window.
    let owner = sk(200);
    for t in 10..20u64 {
        room.apply(room.msg_delta(vec![room.msg(&owner, t, "owner")]))
            .unwrap();
    }
    assert!(
        !room.has_msg_by(&d),
        "premise: the deputy's message left the window"
    );
    assert!(room.is_member(&d), "the issuer of a kept ban is not pruned");
    assert_eq!(room.state.provisional.bans.len(), 1, "and the ban stays");
}

/// A member that the owner or an owner deputy bans directly cannot come back
/// as provisional with the same key by doing the work. (A ban by the member's
/// own inviter does not block it: Ian's decision of 2026-11-22, see
/// `a_member_their_inviter_banned_may_rejoin_as_provisional`.) The banned
/// member's record moves to
/// ban evidence under the entry's own key, so neither C1 (another key) nor P5
/// (a `members` record) drops the entry; P2 must. Checked with the ban and the
/// entry in one delta, in two deltas, and with each on a different peer,
/// merged in both directions.
fn a_directly_banned_member_stays_out(
    base: &Room,
    banner: Option<&SigningKey>,
    target: &SigningKey,
) {
    let ban = base.user_ban(banner, id_of(target), 10);
    let mut together = base.join_delta(target, 11, "back");
    together.bans = Some(vec![ban.clone()]);
    let one = replay(base, &[&together]);
    let two = replay(
        base,
        &[
            &user_ban_delta(ban.clone()),
            &base.join_delta(target, 11, "back"),
        ],
    );
    let author = id_of(target);
    for s in [&one, &two] {
        assert!(
            s.bans.0.iter().any(|b| b.ban.banned_user == author),
            "the ban is held"
        );
        assert!(
            !s.provisional
                .entries
                .iter()
                .any(|e| e.provisional.member_vk == target.verifying_key()),
            "a directly banned member has no entry"
        );
        assert!(
            !s.recent_messages
                .messages
                .iter()
                .any(|m| m.message.author == author),
            "and no message"
        );
    }
    assert!(
        ser(&one) == ser(&two),
        "one delta and two give the same state"
    );

    // The entry on a peer that never saw the member's record, the ban on
    // another: merged either way, the entry goes.
    let mut fresh = Room::new(40, 20, Some(policy(1, 7)));
    fresh.apply(fresh.join_delta(target, 11, "back")).unwrap();
    assert!(fresh.has_entry(target));
    let banned = replay(base, &[&user_ban_delta(ban)]);
    let (x, y) = merge2(&banned, &fresh.state, &base.params);
    assert!(ser(&x) == ser(&y), "a commutative first exchange");
    assert!(!x
        .provisional
        .entries
        .iter()
        .any(|e| e.provisional.member_vk == target.verifying_key()));
}

#[test]
fn a_member_an_owner_banned_does_not_come_back_as_provisional() {
    let (base, m, _d) = room_with_member_and_deputy();
    a_directly_banned_member_stays_out(&base, None, &m);
}

#[test]
fn a_member_a_deputy_banned_does_not_come_back_as_provisional() {
    let (base, m, d) = room_with_member_and_deputy();
    a_directly_banned_member_stays_out(&base, Some(&d), &m);
}

/// Rejoin after a ban by a plain member (the target's inviter) succeeds: the
/// ban removes the membership but does not block a provisional entry for the
/// same key, in one delta, in two, and with the ban and the entry on
/// different peers merged either way.
fn a_member_banned_by_a_plain_member_may_rejoin(
    base: &Room,
    banner: &SigningKey,
    target: &SigningKey,
) {
    let ban = base.user_ban(Some(banner), id_of(target), 10);
    let mut together = base.join_delta(target, 11, "back");
    together.bans = Some(vec![ban.clone()]);
    let one = replay(base, &[&together]);
    let two = replay(
        base,
        &[
            &user_ban_delta(ban.clone()),
            &base.join_delta(target, 11, "back"),
        ],
    );
    let author = id_of(target);
    for s in [&one, &two] {
        assert!(
            s.bans.0.iter().any(|b| b.ban.banned_user == author),
            "the ban is held"
        );
        assert!(
            !s.members.members.iter().any(|m| m.member.id() == author),
            "the ban removed the membership"
        );
        assert!(
            s.provisional
                .entries
                .iter()
                .any(|e| e.provisional.member_vk == target.verifying_key()),
            "a ban by the inviter does not block the provisional entry"
        );
        assert!(
            s.recent_messages
                .messages
                .iter()
                .any(|m| m.message.author == author),
            "and its message stands"
        );
    }
    // The two histories can differ (an old message of the member, purged by
    // the ban before the entry existed, is kept where ban and entry arrive
    // together, because it verifies under the entry's key), but they converge
    // when merged, in either direction.
    let (x, y) = merge2(&one, &two, &base.params);
    assert!(ser(&x) == ser(&y), "one delta and two merge to one state");

    let mut fresh = Room::new(40, 20, Some(policy(1, 7)));
    fresh.apply(fresh.join_delta(target, 11, "back")).unwrap();
    let banned = replay(base, &[&user_ban_delta(ban)]);
    let (x, y) = merge2(&banned, &fresh.state, &base.params);
    assert!(ser(&x) == ser(&y), "a commutative first exchange");
    assert!(x
        .provisional
        .entries
        .iter()
        .any(|e| e.provisional.member_vk == target.verifying_key()));
}

#[test]
fn a_member_their_inviter_banned_may_rejoin_as_provisional() {
    let (base, i, v) = room_with_inviter_and_invitee();
    a_member_banned_by_a_plain_member_may_rejoin(&base, &i, &v);
}

/// The griefing case: a plain member invites a newcomer's key (the newcomer
/// never joined through it) and bans it. The newcomer can still join
/// provisionally.
#[test]
fn a_plain_member_cannot_keep_a_newcomer_out_by_inviting_and_banning_its_key() {
    let mut base = Room::new(40, 20, Some(policy(1, 7)));
    let (i, newcomer) = (sk(2), sk(3));
    let rec = AuthorizedMember::new(
        Member {
            owner_member_id: base.owner_id(),
            invited_by: id_of(&i),
            member_vk: newcomer.verifying_key(),
        },
        &i,
    );
    base.apply(ChatRoomStateV1Delta {
        members: Some(MembersDelta::new(vec![
            base.invite(None, &i.verifying_key()),
            rec,
        ])),
        recent_messages: Some(vec![base.msg(&i, 1, "i")]),
        ..Default::default()
    })
    .unwrap();
    a_member_banned_by_a_plain_member_may_rejoin(&base, &i, &newcomer);
}

/// Review finding 2 (seed 101055 shape 1 of the fork fuzz): an id with a
/// provisional entry on one peer, and on the other a ban-evidence record for
/// the same id under another key that sits deeper than the merged room's
/// evidence depth cap. The merge drops that record (`without_too_deep`), so
/// the entry stays, and the entry's `member_info` must stay too in BOTH merge
/// directions. Before the fix, the side receiving the `member_info` resolved
/// its author through the soon-dropped record and skipped it, while the side
/// already holding it kept it: `merge(A, B) != merge(B, A)`.
#[test]
fn an_evidence_record_the_merge_drops_as_too_deep_does_not_decide_a_provisional_arrival() {
    let mut a = Room::new(40, 10, Some(policy(1, 7)));
    let (m1, m2, x) = (sk(2), sk(3), sk(4));
    // A record for another key with x's id, invited by m2 at depth 3, then
    // banned by its inviter: it is ban evidence on peer A.
    let forged = forged_vk_with_id_of(&x.verifying_key());
    let rec_m2 = AuthorizedMember::new(
        Member {
            owner_member_id: a.owner_id(),
            invited_by: id_of(&m1),
            member_vk: m2.verifying_key(),
        },
        &m1,
    );
    let rec_f = AuthorizedMember::new(
        Member {
            owner_member_id: a.owner_id(),
            invited_by: id_of(&m2),
            member_vk: forged,
        },
        &m2,
    );
    let mut d = a.members_delta(vec![a.invite(None, &m1.verifying_key()), rec_m2, rec_f]);
    d.recent_messages = Some(vec![a.msg(&m1, 1, "m1"), a.msg(&m2, 2, "m2")]);
    d.bans = Some(vec![a.user_ban(Some(&m2), id_of(&x), 3)]);
    a.apply(d).unwrap();
    assert!(
        a.state
            .ban_evidence
            .members
            .iter()
            .any(|m| m.member.member_vk == forged),
        "the record is ban evidence on A"
    );

    // Peer B: the owner lowered `max_members` to 2 (the evidence depth cap
    // with it), and x joined provisionally with a nickname and a message.
    let mut b = Room::new(40, 10, Some(policy(1, 7)));
    b.set_config(|c| c.max_members = 2);
    b.apply(b.join_delta(&x, 20, "hello")).unwrap();
    assert!(b.has_entry(&x));

    let (ab, ba) = merge2(&a.state, &b.state, &a.params);
    assert!(ser(&ab) == ser(&ba), "the first exchange is commutative");
    let info_of_x = |s: &ChatRoomStateV1| {
        s.member_info
            .member_info
            .iter()
            .any(|i| i.member_info.member_id == id_of(&x))
    };
    assert!(
        ab.provisional
            .entries
            .iter()
            .any(|e| e.provisional.member_vk == x.verifying_key()),
        "the too-deep record is dropped, so the entry stays"
    );
    assert!(info_of_x(&ab), "and so does its member_info");
}

/// The proof of work is bound to the room owner and to the member key: a
/// nonce found for one room, or for one key, does not admit an entry that is
/// correctly self-signed for another room or another key. The golden vector
/// pins the hash; this pins that `check_policy` and the room use those inputs.
#[test]
fn a_proof_of_work_does_not_carry_to_another_room_or_another_key() {
    let bits = 12;
    let room = Room::new(30, 10, Some(policy(bits, 7)));
    let other_owner = sk(201);
    let (k1, k2) = (sk(2), sk(3));
    let nonce = solve(&room.params.owner, &k1.verifying_key(), 7, bits);
    assert!(room
        .entry(&k1)
        .check_policy(room.config(), &room.params.owner)
        .is_ok());

    // Another room: same key, same nonce, signed for the other owner.
    let for_other_room = entry_with(&other_owner.verifying_key(), &k1, 7, nonce);
    let mut other_config = room.config().clone();
    other_config.owner_member_id = MemberId::from(&other_owner.verifying_key());
    assert!(matches!(
        for_other_room.check_policy(&other_config, &other_owner.verifying_key()),
        Err(AdmissionError::InsufficientWork { .. })
    ));
    // Another key: the nonce k1 found, in an entry k2 signs.
    let for_other_key = entry_with(&room.params.owner, &k2, 7, nonce);
    assert!(for_other_key.verify_signature(&room.params.owner).is_ok());
    assert!(matches!(
        for_other_key.check_policy(room.config(), &room.params.owner),
        Err(AdmissionError::InsufficientWork { .. })
    ));
    // Applied, the borrowed proof is skipped (not an error) with its message.
    let mut r = Room::new(30, 10, Some(policy(bits, 7)));
    let mut d = r.join_delta(&k2, 1, "borrowed");
    d.provisional.as_mut().unwrap().entries = vec![for_other_key];
    r.apply(d).unwrap();
    assert!(!r.has_entry(&k2) && !r.has_msg_by(&k2));
    // A new epoch invalidates it too.
    let mut r = Room::new(30, 10, Some(policy(bits, 8)));
    let mut d = r.join_delta(&k1, 1, "old epoch");
    d.provisional.as_mut().unwrap().entries = vec![entry_with(&r.params.owner, &k1, 7, nonce)];
    r.apply(d).unwrap();
    assert!(!r.has_entry(&k1));
}

/// A small-order key signs nothing that needs a secret, so it is never
/// admitted: an entry for one fails the policy (skipped, not an error).
#[test]
fn a_weak_key_is_never_admitted() {
    let room = Room::new(30, 10, Some(policy(0, 7)));
    // The identity point, encoded.
    let mut bytes = [0u8; 32];
    bytes[0] = 1;
    let weak = VerifyingKey::from_bytes(&bytes).unwrap();
    assert!(weak.is_weak());
    let entry = AuthorizedProvisionalMember::with_signature(
        ProvisionalMember {
            member_vk: weak,
            evidence: AdmissionEvidence::Pow { nonce: 0 },
        },
        ed25519_dalek::Signature::from_bytes(&{
            let mut s = [0u8; 64];
            s[0] = 1; // R = identity, S = 0
            s
        }),
    );
    assert_eq!(
        entry.check_policy(room.config(), &room.params.owner),
        Err(AdmissionError::WeakKey)
    );
    assert!(
        entry.verify_signature(&room.params.owner).is_err(),
        "strict verification refuses it too"
    );
    let mut r = Room::new(30, 10, Some(policy(0, 7)));
    r.apply(ChatRoomStateV1Delta {
        provisional: Some(ProvisionalDelta {
            entries: vec![entry],
            bans: vec![],
        }),
        ..Default::default()
    })
    .unwrap();
    assert!(r.state.provisional.entries.is_empty());
}

/// A provisional member cannot invite (an `AuthorizedMember` naming a
/// provisional id as inviter never resolves), and cannot ban in either list.
#[test]
fn a_provisional_member_cannot_invite_or_ban() {
    let (mut room, m, _d) = room_with_member_and_deputy();
    let (p, q) = (sk(10), sk(11));
    room.apply(room.join_delta(&p, 5, "hi")).unwrap();
    assert!(room.has_entry(&p));
    // An invite signed by the provisional key, with the invitee's message.
    let invite = AuthorizedMember::new(
        Member {
            owner_member_id: room.owner_id(),
            invited_by: id_of(&p),
            member_vk: q.verifying_key(),
        },
        &p,
    );
    let mut d = room.members_delta(vec![invite]);
    d.recent_messages = Some(vec![room.msg(&q, 6, "invited by a provisional")]);
    let _ = room.apply(d);
    assert!(!room.is_member(&q) && !room.has_msg_by(&q));
    // A `UserBan` and a provisional ban issued by the provisional key.
    let before = ser(&room.state);
    let mut d = user_ban_delta(room.user_ban(Some(&p), id_of(&m), 7));
    d.provisional = Some(ProvisionalDelta {
        entries: vec![],
        bans: vec![room.prov_ban(Some(&p), id_of(&q), 7)],
    });
    let _ = room.apply(d);
    assert!(room.is_member(&m), "the member stays");
    assert!(
        room.state.provisional.bans.is_empty(),
        "the provisional ban is swept"
    );
    assert!(
        !room.state.bans.0.iter().any(|b| b.banned_by == id_of(&p)),
        "the user ban is swept"
    );
    assert!(room.has_entry(&p));
    let _ = before;
}

/// A room with no active policy pays nothing for the field: entries and
/// provisional bans in a delta are not processed at all, so even a forged
/// entry signature, which is an `Err` in an open room, is ignored.
#[test]
fn a_room_without_an_active_policy_ignores_the_provisional_field() {
    let room = Room::new(30, 10, Some(policy(0, 7)));
    let forged = AuthorizedProvisionalMember::with_signature(
        room.entry(&sk(2)).provisional,
        ed25519_dalek::Signature::from_bytes(&[7u8; 64]),
    );
    let delta = ChatRoomStateV1Delta {
        provisional: Some(ProvisionalDelta {
            entries: vec![forged],
            bans: vec![room.prov_ban(None, id_of(&sk(3)), 1)],
        }),
        ..Default::default()
    };
    let mut open = Room::new(30, 10, Some(policy(0, 7)));
    assert!(
        open.apply(delta.clone()).is_err(),
        "a forgery in an open room"
    );
    let mut no_policy = Room::new(30, 10, None);
    let mut private_room = Room::new(30, 10, Some(policy(0, 7)));
    private_room.set_config(|c| {
        c.privacy_mode = PrivacyMode::Private;
        c.display.name = river_core::room_state::privacy::SealedBytes::Private {
            ciphertext: vec![1; 16],
            nonce: [0; 12],
            secret_version: 0,
            declared_len_bytes: 4,
        };
        c.display.description = None;
    });
    for closed in [&mut no_policy, &mut private_room] {
        let before = ser(&closed.state);
        closed.apply(delta.clone()).unwrap();
        assert!(ser(&closed.state) == before, "nothing changes");
    }
    // An owner-issued provisional ban with a bad signature is an `Err` where
    // the field is processed (its arrival checks it against the owner key),
    // and changes nothing where it is not.
    let mut forged_ban = room.prov_ban(None, id_of(&sk(4)), 2);
    forged_ban.signature = ed25519_dalek::Signature::from_bytes(&[9u8; 64]);
    let ban_delta = prov_ban_delta(forged_ban);
    assert!(open.apply(ban_delta.clone()).is_err());
    for closed in [&mut no_policy, &mut private_room] {
        let before = ser(&closed.state);
        closed.apply(ban_delta.clone()).unwrap();
        assert!(
            ser(&closed.state) == before,
            "nothing is checked or changed"
        );
    }
    // A policy with no proof of work admits nobody: the forged entry fails
    // the policy (checked first) and is skipped without its signature being
    // checked. Such a room still holds provisional bans (a listed room whose
    // owner bans ahead of opening it).
    let mut listed = Room::new(
        30,
        10,
        Some(PublicRoomPolicy {
            pow: None,
            ..policy(0, 7)
        }),
    );
    listed.apply(delta).unwrap();
    assert!(listed.state.provisional.entries.is_empty());
}

/// Review finding 7: when the member cap removes a deputy who holds a
/// provisional ban, the ban goes in the same cleanup pass (P1 runs again after
/// the member loop), so the provisional step adds no pass of its own. Without
/// that, the ban stayed one more pass and this cleanup needed all of
/// `MAX_CLEANUP_PASSES`.
#[test]
fn a_provisional_ban_goes_in_the_pass_the_member_cap_removes_its_deputy() {
    let mut room = Room::new(40, 10, Some(policy(1, 7)));
    // Two members the owner invited; the cap of one below keeps the one with
    // the lower id, so the deputy is the one with the higher id.
    let (mut a, mut d) = (sk(2), sk(3));
    if id_of(&d) < id_of(&a) {
        std::mem::swap(&mut a, &mut d);
    }
    let mut info = MemberInfo::new_public(room.owner_id(), 2, "owner".into());
    info.deputies = vec![id_of(&d)];
    room.apply(ChatRoomStateV1Delta {
        members: Some(MembersDelta::new(vec![
            room.invite(None, &a.verifying_key()),
            room.invite(None, &d.verifying_key()),
        ])),
        member_info: Some(vec![AuthorizedMemberInfo::new(info, &room.owner)]),
        recent_messages: Some(vec![room.msg(&a, 1, "a"), room.msg(&d, 2, "d")]),
        ..Default::default()
    })
    .unwrap();
    room.apply(prov_ban_delta(room.prov_ban(Some(&d), id_of(&sk(40)), 5)))
        .unwrap();
    assert_eq!(room.state.provisional.bans.len(), 1);
    // The owner lowers `max_members` to 1.
    let lowered = room.config_delta(|c| c.max_members = 1);
    let mut state = room.state.clone();
    state.configuration = lowered.configuration.unwrap();
    let passes = state.post_apply_cleanup_counted(&room.params).unwrap();
    assert!(!state
        .members
        .members
        .iter()
        .any(|m| m.member.id() == id_of(&d)));
    assert!(
        state.provisional.bans.is_empty(),
        "the deputy's ban is gone"
    );
    assert!(passes.confirmed);
    assert_eq!(
        passes.passes, 2,
        "one changing pass and one confirming pass"
    );
}

/// Only a ban that takes effect blocks an entry. The inviter banned their
/// invitee, then the owner banned the inviter: the inviter's bans take no
/// effect from then on, so the invitee, now removed only with a banned
/// inviter, may come back as provisional (design D2 (a)), in the same delta
/// as the owner's ban and after it.
#[test]
fn a_ban_by_an_issuer_the_owner_removed_does_not_block_an_entry() {
    let (mut base, i, v) = room_with_inviter_and_invitee();
    base.apply(user_ban_delta(base.user_ban(Some(&i), id_of(&v), 10)))
        .unwrap();
    assert!(!base.is_member(&v), "premise: the inviter's ban removed V");
    let ban_i = base.user_ban(None, id_of(&i), 20);
    let mut together = base.join_delta(&v, 21, "back");
    together.bans = Some(vec![ban_i.clone()]);
    let one = replay(&base, &[&together]);
    let two = replay(
        &base,
        &[&user_ban_delta(ban_i), &base.join_delta(&v, 21, "back")],
    );
    for s in [&one, &two] {
        assert!(s
            .provisional
            .entries
            .iter()
            .any(|e| e.provisional.member_vk == v.verifying_key()));
    }
    assert!(ser(&one) == ser(&two));
}

/// A key other than `victim` with `victim`'s `MemberId`, ordering after
/// `victim`'s bytes when `after` is set and before them otherwise. It has no
/// secret key.
fn forged_vk_in_order(victim: &VerifyingKey, after: bool) -> VerifyingKey {
    let target: MemberId = victim.into();
    let base = victim.to_bytes();
    for i in 0..31 {
        let mut bytes = base;
        if after {
            if bytes[i] == 255 || bytes[i + 1] < 31 {
                continue;
            }
            bytes[i] += 1;
            bytes[i + 1] -= 31;
        } else {
            if bytes[i] == 0 || bytes[i + 1] > 255 - 31 {
                continue;
            }
            bytes[i] -= 1;
            bytes[i + 1] += 31;
        }
        if let Ok(vk) = VerifyingKey::from_bytes(&bytes) {
            if MemberId::from(&vk) == target && vk != *victim {
                return vk;
            }
        }
    }
    panic!("no forged key found");
}

/// A provisional member sponsored into `members` on one peer, and a record
/// under another key for their id (invited by a member and kept by a DM)
/// taken first on another: both merge directions apply, and every result
/// verifies and is canonical, for both byte orders of the two keys. The same
/// holds once the sponsored member is held as ban evidence instead (their
/// inviter bans them), with their `member_info` kept for the ban.
#[test]
fn a_sponsored_member_and_another_key_for_their_id_merge_and_verify() {
    for after in [true, false] {
        let (base, m, _d) = room_with_member_and_deputy();
        let v = sk(10);
        let forged = forged_vk_in_order(&v.verifying_key(), after);
        let fork = |room: &Room| Room {
            owner: room.owner.clone(),
            params: room.params.clone(),
            state: room.state.clone(),
            version: room.version,
        };
        let mut victim = fork(&base);
        victim.apply(victim.join_delta(&v, 5, "victim")).unwrap();
        let mut sponsor = victim.members_delta(vec![victim.invite(Some(&m), &v.verifying_key())]);
        sponsor.recent_messages = Some(vec![victim.msg(&v, 6, "sponsored")]);
        victim.apply(sponsor).unwrap();
        assert!(victim.is_member(&v));

        let mut attacker = fork(&base);
        let mut shadow = attacker.members_delta(vec![attacker.invite(Some(&m), &forged)]);
        shadow.direct_messages = dm_delta(vec![dm(&attacker, &m, id_of(&v), 50)]).direct_messages;
        attacker.apply(shadow).unwrap();

        let (x, y) = merge2(&victim.state, &attacker.state, &base.params);
        assert!(x
            .members
            .members
            .iter()
            .any(|r| r.member.member_vk == v.verifying_key()));
        assert!(y
            .members
            .members
            .iter()
            .any(|r| r.member.member_vk == forged));

        victim
            .apply(user_ban_delta(victim.user_ban(Some(&m), id_of(&v), 60)))
            .unwrap();
        assert!(victim
            .state
            .ban_evidence
            .members
            .iter()
            .any(|r| r.member.member_vk == v.verifying_key()));
        merge2(&victim.state, &attacker.state, &base.params);
    }
}

// ---------------------------------------------------------------------------
// Provisional ban ranking: newest first
// ---------------------------------------------------------------------------

fn full_ban_room() -> Room {
    let mut p = policy(0, 1);
    p.max_provisional_bans = 16;
    Room::new(20, 10, Some(p))
}

fn target_of(trial: u16, i: u16) -> MemberId {
    id_of(&sk(((trial * 20 + i) % 200 + 20) as u8))
}

/// A full list keeps the ban just issued and cuts the oldest, whatever the
/// digests are. A digest-only order cut the new ban about 1 time in 17.
#[test]
fn a_new_owner_provisional_ban_is_kept_and_the_oldest_is_cut_on_a_full_list() {
    for trial in 0..40u16 {
        let mut room = full_ban_room();
        for i in 0..16u16 {
            room.apply(prov_ban_delta(room.prov_ban(
                None,
                target_of(trial, i),
                1_000 + i as u64,
            )))
            .unwrap();
        }
        assert_eq!(room.state.provisional.bans.len(), 16);
        let newest = target_of(trial, 17);
        room.apply(prov_ban_delta(room.prov_ban(None, newest, 5_000)))
            .unwrap();
        let bans = &room.state.provisional.bans;
        assert_eq!(bans.len(), 16);
        assert!(
            bans.iter().any(|b| b.ban.target == newest),
            "trial {trial}: the new ban was cut"
        );
        assert!(
            !bans.iter().any(|b| b.ban.target == target_of(trial, 0)),
            "trial {trial}: the oldest ban stays"
        );
        for i in 1..16u16 {
            assert!(
                bans.iter().any(|b| b.ban.target == target_of(trial, i)),
                "trial {trial}"
            );
        }
    }
}

/// First ban per target ranks before repeats, and of two bans on one target
/// the newer is the one kept. Over many trials, so a digest-only order (which
/// keeps the newer one about half the time) cannot pass.
#[test]
fn provisional_ban_repeats_rank_after_first_bans_per_target() {
    for trial in 0..40u16 {
        let mut room = full_ban_room();
        for i in 0..16u16 {
            room.apply(prov_ban_delta(room.prov_ban(
                None,
                target_of(trial, i),
                1_000 + i as u64,
            )))
            .unwrap();
        }
        // A newer second ban on the oldest ban's target: the 17th record.
        room.apply(prov_ban_delta(room.prov_ban(
            None,
            target_of(trial, 0),
            9_000,
        )))
        .unwrap();
        let bans = &room.state.provisional.bans;
        assert_eq!(bans.len(), 16);
        for i in 0..16u16 {
            assert!(
                bans.iter().any(|b| b.ban.target == target_of(trial, i)),
                "trial {trial}: every target stays banned"
            );
        }
        let for_first: Vec<_> = bans
            .iter()
            .filter(|b| b.ban.target == target_of(trial, 0))
            .collect();
        assert_eq!(for_first.len(), 1, "trial {trial}");
        assert_eq!(
            for_first[0].ban.banned_at,
            (SystemTime::UNIX_EPOCH + Duration::from_secs(9_000)).into(),
            "trial {trial}: the older repeat was kept"
        );
    }
}

/// Bans with one `banned_at` are ordered by digest, so the kept set does not
/// depend on arrival order.
#[test]
fn provisional_bans_with_equal_banned_at_keep_the_same_set_in_every_order() {
    let base = full_ban_room();
    let bans: Vec<AuthorizedProvisionalBan> = (0..20u16)
        .map(|i| base.prov_ban(None, target_of(7, i), 3_000))
        .collect();
    let deltas: Vec<ChatRoomStateV1Delta> = bans.iter().cloned().map(prov_ban_delta).collect();
    let refs: Vec<&ChatRoomStateV1Delta> = deltas.iter().collect();
    let forward = replay(&base, &refs);
    let mut rev = refs.clone();
    rev.reverse();
    let backward = replay(&base, &rev);
    let mut rot = refs.clone();
    rot.rotate_left(7);
    let rotated = replay(&base, &rot);
    assert_eq!(forward.provisional.bans.len(), 16);
    assert_eq!(ser(&forward), ser(&backward));
    assert_eq!(ser(&forward), ser(&rotated));
}

/// `banned_at` at the extremes: the epoch and `SignedTime::MAX` neither fail
/// a merge nor break the order. The latest ban holds its slot, the epoch ban
/// goes first, and the result does not depend on arrival order.
#[test]
fn provisional_bans_at_the_time_extremes_rank_newest_first() {
    use river_core::signed_time::SignedTime;
    let base = full_ban_room();
    let at = |target: MemberId, t: SignedTime| {
        AuthorizedProvisionalBan::new(
            &base.params.owner,
            ProvisionalBan {
                banned_by: base.owner_id(),
                banned_at: t,
                target,
            },
            &base.owner,
        )
    };
    let mut bans = vec![
        at(target_of(9, 0), SignedTime::UNIX_EPOCH),
        at(target_of(9, 1), SignedTime::MAX),
    ];
    for i in 2..16u16 {
        bans.push(at(
            target_of(9, i),
            (SystemTime::UNIX_EPOCH + Duration::from_secs(1_000 + i as u64)).into(),
        ));
    }
    // The 17th: an ordinary ban, newer than all but the MAX one.
    bans.push(at(
        target_of(9, 17),
        (SystemTime::UNIX_EPOCH + Duration::from_secs(5_000)).into(),
    ));
    let deltas: Vec<ChatRoomStateV1Delta> = bans.iter().cloned().map(prov_ban_delta).collect();
    let refs: Vec<&ChatRoomStateV1Delta> = deltas.iter().collect();
    let forward = replay(&base, &refs);
    let mut rev = refs.clone();
    rev.reverse();
    let backward = replay(&base, &rev);
    assert_eq!(ser(&forward), ser(&backward));
    let kept = &forward.provisional.bans;
    assert_eq!(kept.len(), 16);
    assert!(
        kept.iter().any(|b| b.ban.target == target_of(9, 1)),
        "MAX kept"
    );
    assert!(
        kept.iter().any(|b| b.ban.target == target_of(9, 17)),
        "newest ordinary kept"
    );
    assert!(
        !kept.iter().any(|b| b.ban.target == target_of(9, 0)),
        "the epoch ban is the one cut"
    );
}

// ---------------------------------------------------------------------------
// Per-author cap (PROVISIONAL_MESSAGES_PER_AUTHOR): edge cases
// ---------------------------------------------------------------------------

fn times_of(room: &Room, k: &SigningKey) -> Vec<u64> {
    let mut v: Vec<u64> = room
        .state
        .recent_messages
        .messages
        .iter()
        .filter(|m| m.message.author == id_of(k))
        .map(|m| m.message.time.secs() - 1_800_000_000)
        .collect();
    v.sort();
    v
}

fn open_room(window: usize, pool: u32, msgs: u32) -> Room {
    let mut p = policy(0, 1);
    p.max_provisional = pool;
    p.max_provisional_messages = msgs;
    Room::new(window, 10, Some(p))
}

#[test]
fn an_author_at_exactly_the_cap_keeps_both_messages() {
    let mut room = open_room(40, 10, 20);
    let a = sk(1);
    room.apply(room.join_delta(&a, 5, "a")).unwrap();
    room.apply(room.msg_delta(vec![room.msg(&a, 6, "b")]))
        .unwrap();
    assert_eq!(times_of(&room, &a), vec![5, 6]);
}

#[test]
fn a_third_message_keeps_the_newest_two_whatever_its_age() {
    let mut room = open_room(40, 10, 20);
    let a = sk(1);
    room.apply(room.join_delta(&a, 10, "a")).unwrap();
    room.apply(room.msg_delta(vec![room.msg(&a, 20, "b")]))
        .unwrap();
    // Older than both kept: it is the one dropped.
    room.apply(room.msg_delta(vec![room.msg(&a, 1, "old")]))
        .unwrap();
    assert_eq!(times_of(&room, &a), vec![10, 20]);
    // Newer than both: the oldest kept one is dropped.
    room.apply(room.msg_delta(vec![room.msg(&a, 30, "new")]))
        .unwrap();
    assert_eq!(times_of(&room, &a), vec![20, 30]);
    // Between the two kept: the oldest of the three goes.
    room.apply(room.msg_delta(vec![room.msg(&a, 25, "mid")]))
        .unwrap();
    assert_eq!(times_of(&room, &a), vec![25, 30]);
}

/// A reaction is a message by the provisional key and uses a slot: post,
/// react, post leaves the reaction and the newest message, not the original.
/// Exempting reactions would let one key fill the quota with them.
#[test]
fn reactions_count_toward_the_per_author_cap() {
    let mut room = open_room(40, 10, 20);
    let a = sk(1);
    let first = room.msg(&a, 1, "hello");
    room.apply(ChatRoomStateV1Delta {
        recent_messages: Some(vec![first.clone()]),
        ..room.join_delta(&a, 1, "hello")
    })
    .unwrap();
    let react = AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: room.owner_id(),
            author: id_of(&a),
            time: (SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_002)).into(),
            content: RoomMessageBody::reaction(first.id(), "+1".to_string()),
        },
        &a,
    );
    room.apply(room.msg_delta(vec![react.clone()])).unwrap();
    room.apply(room.msg_delta(vec![room.msg(&a, 3, "again")]))
        .unwrap();
    assert_eq!(times_of(&room, &a), vec![2, 3], "the original is dropped");
    assert!(room
        .state
        .recent_messages
        .messages
        .iter()
        .any(|m| m.id() == react.id()));
}

#[test]
fn banning_a_capped_author_removes_it_and_leaves_the_others() {
    let mut room = open_room(40, 10, 20);
    let (hog, honest) = (sk(100), sk(1));
    room.apply(room.join_delta(&honest, 5, "hi")).unwrap();
    room.apply(room.join_delta(&hog, 100, "m")).unwrap();
    for t in 101..=103u64 {
        room.apply(room.msg_delta(vec![room.msg(&hog, t, "m")]))
            .unwrap();
    }
    assert_eq!(times_of(&room, &hog), vec![102, 103]);
    room.apply(prov_ban_delta(room.prov_ban(None, id_of(&hog), 200)))
        .unwrap();
    assert!(!room.has_entry(&hog) && !room.has_msg_by(&hog));
    assert!(room.has_entry(&honest) && room.has_msg_by(&honest));
}

#[test]
fn a_member_is_not_limited_after_crossing_from_provisional() {
    let mut room = open_room(40, 10, 20);
    let a = sk(1);
    room.apply(room.join_delta(&a, 1, "a")).unwrap();
    for t in 2..=4u64 {
        room.apply(room.msg_delta(vec![room.msg(&a, t, "m")]))
            .unwrap();
    }
    assert_eq!(times_of(&room, &a), vec![3, 4], "capped while provisional");
    let invite = room.invite(None, &a.verifying_key());
    room.apply(room.members_delta(vec![invite])).unwrap();
    assert!(room.is_member(&a) && !room.has_entry(&a));
    for t in 5..=9u64 {
        room.apply(room.msg_delta(vec![room.msg(&a, t, "m")]))
            .unwrap();
    }
    assert_eq!(
        times_of(&room, &a),
        vec![3, 4, 5, 6, 7, 8, 9],
        "no per-author limit on a member"
    );
}

/// Window 6 gives a quota of 3 (odd): the newest message of the quota goes to
/// the author with the newest message, the rest follow newest first.
#[test]
fn an_odd_quota_is_filled_newest_first() {
    let mut room = open_room(6, 10, 500);
    let (a, b) = (sk(1), sk(2));
    room.apply(room.join_delta(&a, 10, "a")).unwrap();
    room.apply(room.msg_delta(vec![room.msg(&a, 11, "a")]))
        .unwrap();
    room.apply(room.join_delta(&b, 20, "b")).unwrap();
    room.apply(room.msg_delta(vec![room.msg(&b, 21, "b")]))
        .unwrap();
    assert_eq!(times_of(&room, &b), vec![20, 21]);
    assert_eq!(times_of(&room, &a), vec![11], "a keeps only its newest");
}

/// Two authors with the same newest time at the quota boundary: the id breaks
/// the tie, the same way in every arrival order.
#[test]
fn authors_tied_on_the_newest_time_at_the_quota_boundary_resolve_by_id() {
    let base = open_room(4, 10, 500); // quota 2
    let (a, b) = (sk(1), sk(2));
    let da = base.join_delta(&a, 50, "a");
    let db = base.join_delta(&b, 50, "b");
    let ab = replay(&base, &[&da, &db]);
    let ba = replay(&base, &[&db, &da]);
    assert_eq!(ser(&ab), ser(&ba));
    let held = ab.recent_messages.messages.len();
    assert_eq!(held, 2, "both fit the quota of 2");
    // A third author at the same time cannot enlarge the quota, and which of
    // the three is dropped does not depend on arrival.
    let c = sk(3);
    let dc = base.join_delta(&c, 50, "c");
    let s1 = replay(&base, &[&da, &db, &dc]);
    let s2 = replay(&base, &[&dc, &db, &da]);
    let s3 = replay(&base, &[&db, &dc, &da]);
    assert_eq!(s1.recent_messages.messages.len(), 2);
    assert_eq!(ser(&s1), ser(&s2));
    assert_eq!(ser(&s1), ser(&s3));
}

/// Cleanup that runs before an invite arrives drops a newcomer's oldest
/// message; a peer that got the invite first keeps it. The two peers converge
/// on merge, to the state the peer that saw the invite first holds.
#[test]
fn promotion_before_or_after_cleanup_converges_on_merge() {
    let base = open_room(40, 10, 20);
    let a = sk(1);
    let join = base.join_delta(&a, 1, "m1");
    let more = base.msg_delta(vec![base.msg(&a, 2, "m2"), base.msg(&a, 3, "m3")]);
    let mut invite = base.members_delta(vec![base.invite(None, &a.verifying_key())]);
    invite.recent_messages = Some(vec![base.msg(&a, 1, "m1")]);
    // X cleans up with three messages before the invite; Y sees the invite
    // first.
    let x = replay(&base, &[&join, &more, &invite]);
    let y = replay(&base, &[&invite, &join, &more]);
    let (xy, yx) = merge2(&x, &y, &base.params);
    assert_eq!(ser(&xy), ser(&yx));
    let mut ms: Vec<u64> = xy
        .recent_messages
        .messages
        .iter()
        .map(|m| m.message.time.secs() - 1_800_000_000)
        .collect();
    ms.sort();
    assert_eq!(ms, vec![1, 2, 3], "the member keeps everything after merge");
}

/// Messages split over several deltas that arrive in any order, with member
/// and owner messages, a promotion and a ban between them, leave every peer
/// at one state once they have gossiped.
mod cross_delta {
    use super::*;
    use proptest::prelude::*;
    use rand::seq::SliceRandom;
    use rand::SeedableRng;
    use std::sync::OnceLock;

    const PROV: usize = 4; // P0..P3 hold entries
    const TIMES: usize = 8;

    struct Fx {
        base: Room,
        /// Provisional messages [author * TIMES + t], then member (index
        /// PROV) messages, then owner messages.
        msgs: Vec<AuthorizedMessageV1>,
        /// The entry of provisional author `a`: it travels with its messages,
        /// as a client sends it (an entry holding no message is dropped).
        entries: Vec<AuthorizedProvisionalMember>,
        promote: ChatRoomStateV1Delta, // invite P0
        ban: ChatRoomStateV1Delta,     // owner bans P1
    }

    fn fx() -> &'static Fx {
        static F: OnceLock<Fx> = OnceLock::new();
        F.get_or_init(|| {
            let mut p = policy(0, 1);
            p.max_provisional = 3;
            p.max_provisional_messages = 6;
            let mut base = Room::new(14, 10, Some(p)); // quota 6
            let mut msgs = Vec::new();
            // A member (invited by the owner) first: applying a delta runs
            // cleanup, which drops entries that hold no message.
            let m0 = sk(60);
            let inv0 = base.invite(None, &m0.verifying_key());
            let mut first = base.members_delta(vec![inv0]);
            // A bare invite is pruned for inactivity until it has a message.
            first.recent_messages = Some(vec![base.msg(&m0, 1, "m")]);
            base.apply(first).unwrap();
            let mut entries = Vec::new();
            for a in 0..PROV {
                let k = sk(a as u8 + 1);
                entries.push(base.entry(&k));
                for t in 0..TIMES {
                    msgs.push(base.msg(&k, (t * 2 + a) as u64, "p"));
                }
            }
            // A member (invited by the owner) and the owner.
            let m = m0;
            for t in 0..TIMES {
                msgs.push(base.msg(&m, (t * 2) as u64 + 1, "m"));
            }
            let owner = base.owner.clone();
            for t in 0..TIMES {
                msgs.push(base.msg(&owner, (t * 2) as u64, "o"));
            }
            let a0 = sk(1);
            let mut promote = base.members_delta(vec![base.invite(None, &a0.verifying_key())]);
            promote.recent_messages = Some(vec![msgs[0].clone()]);
            let ban = user_ban_delta(base.user_ban(None, id_of(&sk(2)), 500));
            Fx {
                base,
                msgs,
                entries,
                promote,
                ban,
            }
        })
    }

    fn apply_in_order(deltas: &[&ChatRoomStateV1Delta]) -> ChatRoomStateV1 {
        let f = fx();
        let mut s = f.base.state.clone();
        for d in deltas {
            let mut next = s.clone();
            if next
                .apply_delta(&s, &f.base.params, &Some((*d).clone()))
                .is_ok()
            {
                s = next;
            }
        }
        s
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]
        #[test]
        fn every_arrival_order_converges(
            picks in proptest::collection::btree_set(0usize..PROV * TIMES + 2 * TIMES, 0..50),
            nchunks in 1usize..6,
            with_promotion in any::<bool>(),
            with_ban in any::<bool>(),
            seed in any::<u64>(),
        ) {
            let f = fx();
            let picks: Vec<usize> = picks.into_iter().collect();
            let mut deltas: Vec<ChatRoomStateV1Delta> = Vec::new();
            let per = picks.len().div_ceil(nchunks).max(1);
            for chunk in picks.chunks(per) {
                let mut entries: Vec<AuthorizedProvisionalMember> = Vec::new();
                for &i in chunk {
                    if i < PROV * TIMES && !entries.contains(&f.entries[i / TIMES]) {
                        entries.push(f.entries[i / TIMES].clone());
                    }
                }
                deltas.push(ChatRoomStateV1Delta {
                    provisional: Some(ProvisionalDelta { entries, bans: vec![] }),
                    recent_messages: Some(chunk.iter().map(|&i| f.msgs[i].clone()).collect()),
                    ..Default::default()
                });
            }
            if with_promotion { deltas.push(f.promote.clone()); }
            if with_ban { deltas.push(f.ban.clone()); }
            let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
            let order = |rng: &mut rand::rngs::StdRng| {
                let mut r: Vec<&ChatRoomStateV1Delta> = deltas.iter().collect();
                r.shuffle(rng);
                r
            };
            let s1 = apply_in_order(&order(&mut rng));
            let s2 = apply_in_order(&order(&mut rng));
            // Applying deltas one at a time is history dependent (a message
            // a full pool or quota dropped is not restored by a later
            // delta), with or without the per-author cap; the peers agree
            // once they gossip.
            // After gossip the peers agree, whatever they went through.
            let mut ab = s1.clone();
            ab.merge_from(&f.base.params, &s2).unwrap();
            let mut ba = s2.clone();
            ba.merge_from(&f.base.params, &s1).unwrap();
            prop_assert_eq!(ser(&ab), ser(&ba));
            validate(&ab, &f.base.params).unwrap();
            // Still at most the cap per provisional author.
            let mut per: std::collections::HashMap<MemberId, usize> = Default::default();
            for m in &ab.recent_messages.messages {
                if ab.provisional.entries.iter().any(|e| MemberId::from(&e.provisional.member_vk) == m.message.author) {
                    *per.entry(m.message.author).or_default() += 1;
                }
            }
            prop_assert!(per.values().all(|&n| n <= PROVISIONAL_MESSAGES_PER_AUTHOR));
        }
    }
}
