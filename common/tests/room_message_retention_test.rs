//! After `apply_delta`, the recent messages a room holds are the newest
//! `max_recent_messages` among those `post_apply_cleanup` keeps, for every kind
//! of ban issuer and in both room types.

use ed25519_dalek::SigningKey;
use freenet_scaffold::ComposableState;
use rand::rngs::OsRng;
use river_core::room_state::ban::{AuthorizedUserBan, BansV1, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersDelta, MembersV1};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo, MemberInfoV1};
use river_core::room_state::message::{
    AuthorizedMessageV1, MessageId, MessageV1, MessagesV1, RoomMessageBody,
};
use river_core::room_state::privacy::PrivacyMode;
use river_core::room_state::privacy::RoomCipherSpec;
use river_core::room_state::secret::{
    AuthorizedSecretVersionRecord, RoomSecretsV1, SecretVersionRecordV1,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
use std::collections::HashSet;
use std::time::{Duration, SystemTime};

const CAP: usize = 50;

struct Peer {
    sk: SigningKey,
    id: MemberId,
}

impl Peer {
    fn new() -> Self {
        let sk = SigningKey::generate(&mut OsRng);
        let id = sk.verifying_key().into();
        Self { sk, id }
    }
}

/// Who issued the ban on `x`.
#[derive(Clone, Copy, Debug)]
enum Banner {
    Owner,
    /// `a`, who invited `x`.
    Ancestor,
    /// `g`, deputized by the owner.
    GlobalModerator,
    /// `d`, deputized by `a` (so `x` is inside the deputizer's subtree).
    SubtreeDeputy,
}

const ALL_BANNERS: [Banner; 4] = [
    Banner::Owner,
    Banner::Ancestor,
    Banner::GlobalModerator,
    Banner::SubtreeDeputy,
];

struct Room {
    owner: Peer,
    a: Peer,
    g: Peer,
    d: Peer,
    x: Peer,
    params: ChatRoomParametersV1,
    privacy: PrivacyMode,
    /// The time of the oldest baseline message.
    start: SystemTime,
}

impl Room {
    fn new(privacy: PrivacyMode) -> Self {
        let owner = Peer::new();
        let params = ChatRoomParametersV1 {
            owner: owner.sk.verifying_key(),
        };
        Self {
            owner,
            a: Peer::new(),
            g: Peer::new(),
            d: Peer::new(),
            x: Peer::new(),
            params,
            privacy,
            start: SystemTime::now() - Duration::from_secs(3600),
        }
    }

    /// `secs` seconds after the newest baseline message.
    fn after_baseline(&self, secs: u64) -> SystemTime {
        self.start + Duration::from_secs(CAP as u64 + secs)
    }

    fn member(&self, who: &Peer, inviter: &Peer) -> AuthorizedMember {
        AuthorizedMember::new(
            Member {
                owner_member_id: self.owner.id,
                invited_by: inviter.id,
                member_vk: who.sk.verifying_key(),
            },
            &inviter.sk,
        )
    }

    fn x_member(&self) -> AuthorizedMember {
        self.member(&self.x, &self.a)
    }

    fn info(&self, who: &Peer, version: u32, deputies: Vec<MemberId>) -> AuthorizedMemberInfo {
        let mut mi = MemberInfo::new_public(who.id, version, "nick".to_string());
        mi.deputies = deputies;
        AuthorizedMemberInfo::new_with_member_key(mi, &who.sk)
    }

    fn message(&self, who: &Peer, time: SystemTime, n: usize) -> AuthorizedMessageV1 {
        let content = match self.privacy {
            PrivacyMode::Public => RoomMessageBody::public(format!("message {n}")),
            PrivacyMode::Private => {
                RoomMessageBody::private_text(format!("ciphertext {n}").into_bytes(), [0u8; 12], 0)
            }
        };
        AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: self.owner.id,
                author: who.id,
                time,
                content,
            },
            &who.sk,
        )
    }

    fn ban_of_x(&self, banner: Banner) -> AuthorizedUserBan {
        let by = match banner {
            Banner::Owner => &self.owner,
            Banner::Ancestor => &self.a,
            Banner::GlobalModerator => &self.g,
            Banner::SubtreeDeputy => &self.d,
        };
        AuthorizedUserBan::new(
            UserBan {
                owner_member_id: self.owner.id,
                banned_at: SystemTime::now(),
                banned_user: self.x.id,
            },
            by.id,
            &by.sk,
        )
    }

    fn secrets(&self) -> RoomSecretsV1 {
        match self.privacy {
            PrivacyMode::Public => RoomSecretsV1::default(),
            PrivacyMode::Private => RoomSecretsV1 {
                current_version: 0,
                versions: vec![AuthorizedSecretVersionRecord::new(
                    SecretVersionRecordV1 {
                        version: 0,
                        cipher_spec: RoomCipherSpec::Aes256Gcm,
                        created_at: SystemTime::now(),
                    },
                    &self.owner.sk,
                )],
                encrypted_secrets: vec![],
            },
        }
    }

    /// A converged room holding exactly `CAP` messages from its current
    /// members (owner, `a`, `g`, `d`), with `x` banned and absent. The owner
    /// deputizes `g` (global moderator) and `a` deputizes `d` (subtree deputy
    /// over `a`'s invitees, which include `x`).
    fn baseline(&self, banner: Banner) -> ChatRoomStateV1 {
        let authors = [&self.owner, &self.a, &self.g, &self.d];
        let messages = (0..CAP)
            .map(|i| {
                self.message(
                    authors[i % authors.len()],
                    self.start + Duration::from_secs(i as u64),
                    i,
                )
            })
            .collect();
        let mut state = ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(
                Configuration {
                    owner_member_id: self.owner.id,
                    privacy_mode: self.privacy.clone(),
                    max_members: 100,
                    max_user_bans: 100,
                    max_recent_messages: CAP,
                    ..Default::default()
                },
                &self.owner.sk,
            ),
            members: MembersV1 {
                members: vec![
                    self.member(&self.a, &self.owner),
                    self.member(&self.g, &self.owner),
                    self.member(&self.d, &self.owner),
                ],
            },
            member_info: MemberInfoV1 {
                member_info: vec![
                    self.info(&self.owner, 1, vec![self.g.id]),
                    self.info(&self.a, 1, vec![self.d.id]),
                    self.info(&self.g, 0, vec![]),
                    self.info(&self.d, 0, vec![]),
                ],
            },
            secrets: self.secrets(),
            recent_messages: MessagesV1 {
                messages,
                ..Default::default()
            },
            bans: BansV1(vec![self.ban_of_x(banner)]),
            ..Default::default()
        };
        state.post_apply_cleanup(&self.params).unwrap();
        state
            .verify(&state, &self.params)
            .expect("baseline verifies");
        assert_eq!(state.recent_messages.messages.len(), CAP);
        assert_eq!(state.bans.0.len(), 1, "the ban on x is retained");
        state
    }

    /// A delta containing `x`'s membership and `CAP` messages from `x`, newer
    /// than the baseline.
    fn delta_with_x(&self) -> ChatRoomStateV1Delta {
        ChatRoomStateV1Delta {
            members: Some(MembersDelta::new(vec![self.x_member()])),
            recent_messages: Some(
                (0..CAP)
                    .map(|i| self.message(&self.x, self.after_baseline(100 + i as u64), i))
                    .collect(),
            ),
            ..Default::default()
        }
    }
}

fn message_ids(state: &ChatRoomStateV1) -> HashSet<MessageId> {
    state
        .recent_messages
        .messages
        .iter()
        .map(|m| m.id())
        .collect()
}

fn apply(state: &mut ChatRoomStateV1, params: &ChatRoomParametersV1, delta: ChatRoomStateV1Delta) {
    let parent = state.clone();
    state.apply_delta(&parent, params, &Some(delta)).unwrap();
}

fn check_message_retention_matches_cleanup(privacy: PrivacyMode) {
    // Run every row before asserting, so a failure reports the whole matrix.
    let mut failures = Vec::new();
    for banner in ALL_BANNERS {
        let room = Room::new(privacy.clone());
        let baseline = room.baseline(banner);
        let before = message_ids(&baseline);

        let mut state = baseline.clone();
        apply(&mut state, &room.params, room.delta_with_x());

        let retained_all = before.is_subset(&message_ids(&state));
        let x_messages_kept = state
            .recent_messages
            .messages
            .iter()
            .any(|m| m.message.author == room.x.id);
        let x_is_member = state
            .members
            .members
            .iter()
            .any(|m| m.member.id() == room.x.id);
        let verify = state.verify(&state, &room.params);
        if !retained_all || x_messages_kept || x_is_member || verify.is_err() {
            failures.push(format!(
                "x banned by {banner:?}: baseline retained: {retained_all}, \
                 x messages kept: {x_messages_kept}, x member: {x_is_member}, verify: {verify:?}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{privacy:?} room:\n{}",
        failures.join("\n")
    );
}

#[test]
fn message_retention_matches_cleanup_public_room() {
    check_message_retention_matches_cleanup(PrivacyMode::Public);
}

#[test]
fn message_retention_matches_cleanup_private_room() {
    check_message_retention_matches_cleanup(PrivacyMode::Private);
}

/// Applying the same delta again changes nothing.
#[test]
fn reapplying_the_delta_is_idempotent() {
    for banner in ALL_BANNERS {
        let room = Room::new(PrivacyMode::Public);
        let mut once = room.baseline(banner);
        let delta = room.delta_with_x();
        apply(&mut once, &room.params, delta.clone());
        let mut twice = once.clone();
        apply(&mut twice, &room.params, delta);
        assert_eq!(once, twice, "{banner:?}: second application changed state");
    }
}

/// Every permutation of three deltas (two halves of `x`'s messages, each
/// re-adding `x`, plus one ordinary message from `a`) reaches the same state,
/// and a peer that saw them merges with one that did not to that same state.
#[test]
fn delta_order_does_not_change_the_result() {
    for banner in ALL_BANNERS {
        let room = Room::new(PrivacyMode::Public);
        let baseline = room.baseline(banner);

        let x_half = |range: std::ops::Range<usize>| ChatRoomStateV1Delta {
            members: Some(MembersDelta::new(vec![room.x_member()])),
            recent_messages: Some(
                range
                    .map(|i| room.message(&room.x, room.after_baseline(100 + i as u64), i))
                    .collect(),
            ),
            ..Default::default()
        };
        let ordinary = ChatRoomStateV1Delta {
            recent_messages: Some(vec![room.message(&room.a, SystemTime::now(), 1000)]),
            ..Default::default()
        };
        let deltas = [x_half(0..CAP / 2), x_half(CAP / 2..CAP), ordinary];

        let orders = [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ];
        let results: Vec<ChatRoomStateV1> = orders
            .iter()
            .map(|order| {
                let mut s = baseline.clone();
                for &i in order {
                    apply(&mut s, &room.params, deltas[i].clone());
                }
                s
            })
            .collect();
        for (order, r) in orders.iter().zip(&results) {
            assert_eq!(r, &results[0], "{banner:?}: order {order:?} diverged");
        }

        let expected = &results[0];
        assert_eq!(expected.recent_messages.messages.len(), CAP);
        assert!(expected
            .recent_messages
            .messages
            .iter()
            .all(|m| m.message.author != room.x.id));
        expected.verify(expected, &room.params).unwrap();

        // Anti-entropy between a peer that saw the deltas and one that saw
        // only the ordinary message converges on the same state.
        let mut quiet = baseline.clone();
        apply(&mut quiet, &room.params, deltas[2].clone());
        let mut noisy = results[3].clone();
        let (q, n) = (quiet.clone(), noisy.clone());
        quiet.merge(&q, &room.params, &n).unwrap();
        noisy.merge(&n, &room.params, &q).unwrap();
        assert_eq!(quiet, noisy, "{banner:?}: merge diverged");
        assert_eq!(
            &quiet, expected,
            "{banner:?}: merge differs from direct apply"
        );
    }
}

/// Apply `x`'s delta to `state` and return how many of `x`'s messages survive.
/// Used where the ban on `x` does NOT take effect, so cleanup keeps `x`.
fn x_messages_after_delta(room: &Room, mut state: ChatRoomStateV1) -> usize {
    apply(&mut state, &room.params, room.delta_with_x());
    state
        .verify(&state, &room.params)
        .expect("state must verify");
    assert!(
        state
            .members
            .members
            .iter()
            .any(|m| m.member.id() == room.x.id),
        "x is not enforced-banned, so cleanup keeps x"
    );
    state
        .recent_messages
        .messages
        .iter()
        .filter(|m| m.message.author == room.x.id)
        .count()
}

/// A ban from a deputy whose grant was later revoked does not take effect:
/// `x` stays a member and `x`'s messages are kept.
#[test]
fn inert_ban_keeps_the_targets_messages() {
    for privacy in [PrivacyMode::Public, PrivacyMode::Private] {
        inert_ban_case(privacy);
    }
}

fn inert_ban_case(privacy: PrivacyMode) {
    let room = Room::new(privacy);
    let mut state = room.baseline(Banner::SubtreeDeputy);
    let revoke = ChatRoomStateV1Delta {
        member_info: Some(vec![room.info(&room.a, 2, vec![])]),
        ..Default::default()
    };
    apply(&mut state, &room.params, revoke);
    assert_eq!(state.bans.0.len(), 1, "the inert ban is still stored");
    assert_eq!(x_messages_after_delta(&room, state), CAP);
}

/// A ban evicted by `max_user_bans` does not take effect, so `x`'s messages
/// are kept. The newer ban arrives in the same delta as `x`'s messages.
#[test]
fn ban_evicted_by_the_ban_cap_keeps_the_targets_messages() {
    for privacy in [PrivacyMode::Public, PrivacyMode::Private] {
        ban_evicted_case(privacy);
    }
}

fn ban_evicted_case(privacy: PrivacyMode) {
    let room = Room::new(privacy.clone());
    let mut state = room.baseline(Banner::SubtreeDeputy);
    state.configuration = AuthorizedConfigurationV1::new(
        Configuration {
            owner_member_id: room.owner.id,
            privacy_mode: privacy,
            max_members: 100,
            max_user_bans: 1,
            max_recent_messages: CAP,
            ..Default::default()
        },
        &room.owner.sk,
    );
    state.post_apply_cleanup(&room.params).unwrap();
    assert_eq!(state.bans.0.len(), 1, "the ban on x fits the cap");

    // A newer, enforcing owner ban on an absent id: the cap keeps it and
    // evicts the older ban on `x`.
    let other = Peer::new();
    let mut delta = room.delta_with_x();
    delta.bans = Some(vec![AuthorizedUserBan::new(
        UserBan {
            owner_member_id: room.owner.id,
            banned_at: SystemTime::now() + Duration::from_secs(60),
            banned_user: other.id,
        },
        room.owner.id,
        &room.owner.sk,
    )]);
    apply(&mut state, &room.params, delta);
    state
        .verify(&state, &room.params)
        .expect("state must verify");
    assert!(
        state.bans.0.iter().all(|b| b.ban.banned_user != room.x.id),
        "the ban on x was evicted by the cap"
    );
    let kept = state
        .recent_messages
        .messages
        .iter()
        .filter(|m| m.message.author == room.x.id)
        .count();
    assert_eq!(kept, CAP);
}

/// Below the cap the result is the same as if `x`'s messages had never
/// arrived.
#[test]
fn below_the_cap_result_equals_cleanup() {
    for banner in ALL_BANNERS {
        let room = Room::new(PrivacyMode::Public);
        let mut baseline = room.baseline(banner);
        baseline.configuration = AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: room.owner.id,
                max_members: 100,
                max_user_bans: 100,
                max_recent_messages: 3 * CAP,
                ..Default::default()
            },
            &room.owner.sk,
        );

        let mut with_messages = baseline.clone();
        apply(&mut with_messages, &room.params, room.delta_with_x());

        let mut membership_only = baseline.clone();
        let mut delta = room.delta_with_x();
        delta.recent_messages = None;
        apply(&mut membership_only, &room.params, delta);

        assert_eq!(
            with_messages, membership_only,
            "{banner:?}: below the cap, x's messages must leave no trace"
        );
        assert_eq!(with_messages.recent_messages.messages.len(), CAP);
    }
}

/// The ban arrives in the SAME delta as `x`'s membership and messages, against
/// a room where `x` was never banned.
#[test]
fn message_retention_matches_cleanup_ban_in_the_same_delta() {
    let mut failures = Vec::new();
    for banner in ALL_BANNERS {
        let room = Room::new(PrivacyMode::Public);
        let mut baseline = room.baseline(banner);
        baseline.bans = BansV1::default();
        baseline.post_apply_cleanup(&room.params).unwrap();
        baseline.verify(&baseline, &room.params).unwrap();
        let before = message_ids(&baseline);

        let mut delta = room.delta_with_x();
        delta.bans = Some(vec![room.ban_of_x(banner)]);
        let mut state = baseline.clone();
        apply(&mut state, &room.params, delta);

        let retained_all = before.is_subset(&message_ids(&state));
        let x_messages_kept = state
            .recent_messages
            .messages
            .iter()
            .any(|m| m.message.author == room.x.id);
        let verify = state.verify(&state, &room.params);
        if !retained_all || x_messages_kept || verify.is_err() {
            failures.push(format!(
                "x banned by {banner:?}: baseline retained: {retained_all}, \
                 x messages kept: {x_messages_kept}, verify: {verify:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// An action message (here a reaction) from `x` is not kept, and leaves
/// nothing in `actions_state`.
///
/// In a private room the action is encrypted, so only the message itself is
/// checked.
#[test]
fn actions_from_a_removed_author_are_not_kept() {
    for privacy in [PrivacyMode::Public, PrivacyMode::Private] {
        for banner in ALL_BANNERS {
            let room = Room::new(privacy.clone());
            let baseline = room.baseline(banner);
            let before = message_ids(&baseline);
            let target = baseline.recent_messages.messages[0].id();

            let mut delta = room.delta_with_x();
            let content = match privacy {
                PrivacyMode::Public => RoomMessageBody::reaction(target.clone(), "x".to_string()),
                PrivacyMode::Private => {
                    RoomMessageBody::private_action(b"ciphertext".to_vec(), [0u8; 12], 0)
                }
            };
            let action = AuthorizedMessageV1::new(
                MessageV1 {
                    room_owner: room.owner.id,
                    author: room.x.id,
                    time: room.after_baseline(200),
                    content,
                },
                &room.x.sk,
            );
            let action_id = action.id();
            delta.recent_messages.as_mut().unwrap().push(action);

            let mut state = baseline.clone();
            apply(&mut state, &room.params, delta);
            let ids = message_ids(&state);
            assert!(
                before.is_subset(&ids),
                "{privacy:?} {banner:?}: baseline must be retained"
            );
            assert!(
                !ids.contains(&action_id),
                "{privacy:?} {banner:?}: x's action must not be stored"
            );
            assert!(
                state
                    .recent_messages
                    .reactions(&target)
                    .is_none_or(|r| r.values().all(|who| !who.contains(&room.x.id))),
                "{privacy:?} {banner:?}: x's reaction must not survive"
            );
            state.verify(&state, &room.params).unwrap();
        }
    }
}

/// The delta carries `x`'s messages and new owner messages that are newer
/// than the baseline but older than `x`'s. The room keeps every new owner
/// message plus the newest baseline messages.
#[test]
fn message_retention_matches_cleanup_with_new_owner_messages() {
    for banner in ALL_BANNERS {
        let room = Room::new(PrivacyMode::Public);
        let baseline = room.baseline(banner);

        let mut delta = room.delta_with_x();
        let owner_count = CAP / 2;
        let owner_messages: Vec<AuthorizedMessageV1> = (0..owner_count)
            .map(|i| room.message(&room.owner, room.after_baseline(10 + i as u64), 2000 + i))
            .collect();
        let owner_ids: HashSet<MessageId> = owner_messages.iter().map(|m| m.id()).collect();
        delta
            .recent_messages
            .as_mut()
            .unwrap()
            .extend(owner_messages);

        let mut expected: Vec<&AuthorizedMessageV1> =
            baseline.recent_messages.messages.iter().collect();
        expected.sort_by_key(|m| m.message.time);
        let newest_baseline: HashSet<MessageId> =
            expected[owner_count..].iter().map(|m| m.id()).collect();

        let mut state = baseline.clone();
        apply(&mut state, &room.params, delta);
        let ids = message_ids(&state);
        assert!(
            owner_ids.is_subset(&ids),
            "{banner:?}: every new owner message is kept"
        );
        assert_eq!(
            ids,
            &owner_ids | &newest_baseline,
            "{banner:?}: the owner's new messages and the newest baseline messages fill the cap"
        );
        state.verify(&state, &room.params).unwrap();
    }
}

/// One message over the cap: the baseline holds `CAP` messages and the delta
/// adds `x` and one message from `x`, newer than all of them. Every baseline
/// message is kept and `x`'s is not.
#[test]
fn message_retention_matches_cleanup_one_over_the_cap() {
    for privacy in [PrivacyMode::Public, PrivacyMode::Private] {
        for banner in ALL_BANNERS {
            let room = Room::new(privacy.clone());
            let baseline = room.baseline(banner);
            let before = message_ids(&baseline);

            let mut delta = room.delta_with_x();
            delta.recent_messages.as_mut().unwrap().truncate(1);
            let mut state = baseline.clone();
            apply(&mut state, &room.params, delta);
            assert_eq!(
                message_ids(&state),
                before,
                "{privacy:?} {banner:?}: exactly the baseline is kept"
            );
            state.verify(&state, &room.params).unwrap();
        }
    }
}

/// Exactly at the cap: the baseline holds `CAP - 1` messages and the delta adds
/// one from `x`. Exactly the baseline is kept.
#[test]
fn message_retention_matches_cleanup_exactly_at_the_cap() {
    for banner in ALL_BANNERS {
        let room = Room::new(PrivacyMode::Public);
        let mut baseline = room.baseline(banner);
        let oldest = baseline
            .recent_messages
            .messages
            .iter()
            .min_by_key(|m| m.message.time)
            .unwrap()
            .id();
        baseline
            .recent_messages
            .messages
            .retain(|m| m.id() != oldest);
        baseline.recent_messages.rebuild_actions_state();
        baseline.post_apply_cleanup(&room.params).unwrap();
        baseline.verify(&baseline, &room.params).unwrap();
        let before = message_ids(&baseline);
        assert_eq!(before.len(), CAP - 1);

        let mut delta = room.delta_with_x();
        delta.recent_messages.as_mut().unwrap().truncate(1);
        let mut state = baseline.clone();
        apply(&mut state, &room.params, delta);
        assert_eq!(message_ids(&state), before, "{banner:?}");
        state.verify(&state, &room.params).unwrap();
    }
}

/// One delta carries `x`'s membership and messages, a new ban on `x`, and the
/// deputy grant that authorizes it, against a room where `x` was never banned
/// and the banner was not yet a deputy.
#[test]
fn message_retention_matches_cleanup_ban_and_grant_in_the_same_delta() {
    for banner in [Banner::GlobalModerator, Banner::SubtreeDeputy] {
        let room = Room::new(PrivacyMode::Public);
        let mut baseline = room.baseline(banner);
        baseline.bans = BansV1::default();
        // Withdraw both grants; the delta restores the one the ban needs.
        baseline.member_info = MemberInfoV1 {
            member_info: vec![
                room.info(&room.owner, 1, vec![]),
                room.info(&room.a, 1, vec![]),
                room.info(&room.g, 0, vec![]),
                room.info(&room.d, 0, vec![]),
            ],
        };
        baseline.post_apply_cleanup(&room.params).unwrap();
        baseline.verify(&baseline, &room.params).unwrap();
        let before = message_ids(&baseline);

        let grant = match banner {
            Banner::GlobalModerator => room.info(&room.owner, 2, vec![room.g.id]),
            _ => room.info(&room.a, 2, vec![room.d.id]),
        };
        let mut delta = room.delta_with_x();
        delta.bans = Some(vec![room.ban_of_x(banner)]);
        delta.member_info = Some(vec![grant]);
        let mut state = baseline.clone();
        apply(&mut state, &room.params, delta);

        assert!(
            before.is_subset(&message_ids(&state)),
            "{banner:?}: baseline must be retained"
        );
        assert!(
            state
                .members
                .members
                .iter()
                .all(|m| m.member.id() != room.x.id),
            "{banner:?}: the ban on x is enforced"
        );
        assert!(state
            .recent_messages
            .messages
            .iter()
            .all(|m| m.message.author != room.x.id));
        state.verify(&state, &room.params).unwrap();
    }
}

/// A member record whose key is the owner's, signed by `inviter`.
fn owner_record(room: &Room, inviter: &Peer) -> AuthorizedMember {
    AuthorizedMember::new(
        Member {
            owner_member_id: room.owner.id,
            invited_by: inviter.id,
            member_vk: room.owner.sk.verifying_key(),
        },
        &inviter.sk,
    )
}

/// A delta that also carries a member record with the owner's key gives the
/// same result as the delta without it.
#[test]
fn a_member_record_with_the_owner_id_does_not_change_the_result() {
    for banner in ALL_BANNERS {
        let room = Room::new(PrivacyMode::Public);
        let baseline = room.baseline(banner);

        let mut with_record = room.delta_with_x();
        with_record.members = Some(MembersDelta::new(vec![
            room.x_member(),
            owner_record(&room, &room.x),
        ]));
        let mut state = baseline.clone();
        apply(&mut state, &room.params, with_record);

        let mut expected = baseline.clone();
        apply(&mut expected, &room.params, room.delta_with_x());
        assert_eq!(state, expected, "{banner:?}");
        assert!(
            message_ids(&baseline).is_subset(&message_ids(&state)),
            "{banner:?}: baseline must be retained"
        );
        state.verify(&state, &room.params).unwrap();
    }
}
