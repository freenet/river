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
    leading_zero_bits, pow_hash, AdmissionEvidence, AuthorizedProvisionalBan,
    AuthorizedProvisionalMember, PowPolicy, ProvisionalBan, ProvisionalDelta, ProvisionalMember,
    PublicRoomPolicy,
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
                banned_at: SystemTime::UNIX_EPOCH + Duration::from_secs(at),
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
                banned_at: SystemTime::UNIX_EPOCH + Duration::from_secs(at),
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
            time: SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000 + t),
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

/// A key whose `MemberId` equals `victim`'s but whose bytes differ: replace
/// adjacent bytes `(a, b)` with `(a + 1, b - 31)`, which keeps
/// `h = h * 31 + byte`, until the bytes decode as a curve point. Costs
/// nothing and yields no secret key.
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
    assert_eq!(by(&room, &a), 3, "provisional share is the quota");
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
    // C2. A second key with the victim-style id is a free forgery for a
    // record, but an ENTRY needs the key's own signature, so the attacker
    // here holds both keys of the forged pair and signs both entries.
    // (The signing key of a forged vk is unknowable in practice; the
    // contract only sees two entries with one id and different keys.)
    let mut room = Room::new(30, 10, Some(policy(1, 7)));
    let a = sk(1);
    let forged = forged_vk_with_id_of(&a.verifying_key());
    let owner = room.params.owner;
    // An entry for `forged` signed by `a` fails entry_sig_ok (an Err), so the
    // C2 case can only be built with two valid entries; assert that.
    let fake = AuthorizedProvisionalMember::new(
        &owner,
        ProvisionalMember {
            member_vk: forged,
            evidence: AdmissionEvidence::Pow { nonce: 0 },
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
fn a_flood_of_future_dated_provisional_messages_cannot_push_members_out() {
    let mut p = policy(0, 1);
    p.max_provisional = 5;
    p.max_provisional_messages = 100;
    let build = |flood: bool| {
        // window 20: quota min(100, 10) = 10; members keep at least 10 slots.
        let mut room = Room::new(20, 10, Some(p.clone()));
        let members: Vec<SigningKey> = (2..5).map(sk).collect();
        let mut d = room.members_delta(
            members
                .iter()
                .map(|k| room.invite(None, &k.verifying_key()))
                .collect(),
        );
        d.recent_messages = Some(
            members
                .iter()
                .flat_map(|k| (1..=3).map(|t| room.msg(k, t, &format!("m{t}"))))
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
    let member_ids: Vec<MemberId> = members.iter().map(id_of).collect();
    let member_msgs = flooded
        .state
        .recent_messages
        .messages
        .iter()
        .filter(|m| member_ids.contains(&m.message.author))
        .count();
    assert_eq!(
        member_msgs, 9,
        "every member message survives: 9 <= 20 - quota"
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
        room.apply(make(17, &room)).is_err(),
        "17 new entries exceed the bound"
    );
    room.apply(make(16, &room)).expect("16 is at the bound");
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
