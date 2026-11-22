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
