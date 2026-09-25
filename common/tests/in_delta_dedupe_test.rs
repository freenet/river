//! A record repeated identically inside ONE delta is stored once, for the
//! fields that key records by id: bans and recent messages. A peer that
//! received the record twice ends byte-identical to a peer that received it
//! once, both from the delta itself and after merging with that peer. Records
//! that share an id but differ are not merged into one, and the result does not
//! depend on their order in the delta.

use ed25519_dalek::SigningKey;
use freenet_scaffold::ComposableState;
use rand::rngs::OsRng;
use river_core::room_state::ban::{AuthorizedUserBan, BansV1, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersDelta, MembersV1};
use river_core::room_state::message::{
    AuthorizedMessageV1, MessageV1, MessagesV1, RoomMessageBody,
};
use river_core::room_state::privacy::{PrivacyMode, RoomCipherSpec};
use river_core::room_state::secret::{
    AuthorizedSecretVersionRecord, RoomSecretsV1, SecretVersionRecordV1,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
use std::time::{Duration, SystemTime};

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

struct Room {
    owner: Peer,
    a: Peer,
    b: Peer,
    params: ChatRoomParametersV1,
    privacy: PrivacyMode,
}

impl Room {
    fn new() -> Self {
        Self::with_privacy(PrivacyMode::Public)
    }

    fn with_privacy(privacy: PrivacyMode) -> Self {
        let owner = Peer::new();
        let params = ChatRoomParametersV1 {
            owner: owner.sk.verifying_key(),
        };
        Self {
            owner,
            a: Peer::new(),
            b: Peer::new(),
            params,
            privacy,
        }
    }

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000 + secs)
    }

    fn member(&self, who: &Peer) -> AuthorizedMember {
        AuthorizedMember::new(
            Member {
                owner_member_id: self.owner.id,
                invited_by: self.owner.id,
                member_vk: who.sk.verifying_key(),
            },
            &self.owner.sk,
        )
    }

    fn message(&self, who: &Peer, secs: u64) -> AuthorizedMessageV1 {
        AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: self.owner.id,
                author: who.id,
                time: Self::at(secs),
                content: match self.privacy {
                    PrivacyMode::Public => RoomMessageBody::public(format!("message {secs}")),
                    PrivacyMode::Private => RoomMessageBody::private_text(
                        format!("ciphertext {secs}").into_bytes(),
                        [0u8; 12],
                        0,
                    ),
                },
            },
            &who.sk,
        )
    }

    fn owner_ban(&self, target: MemberId, secs: u64) -> AuthorizedUserBan {
        AuthorizedUserBan::new(
            UserBan {
                owner_member_id: self.owner.id,
                banned_at: Self::at(secs),
                banned_user: target,
            },
            self.owner.id,
            &self.owner.sk,
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
                        created_at: Self::at(0),
                    },
                    &self.owner.sk,
                )],
                encrypted_secrets: vec![],
            },
        }
    }

    /// `a` and `b` are members, each with one message, and one ban is stored.
    /// `max_recent_messages` and `max_user_bans` are both 3.
    fn baseline(&self) -> ChatRoomStateV1 {
        let mut state = ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(
                Configuration {
                    owner_member_id: self.owner.id,
                    privacy_mode: self.privacy.clone(),
                    max_members: 20,
                    max_user_bans: 3,
                    max_recent_messages: 3,
                    ..Default::default()
                },
                &self.owner.sk,
            ),
            members: MembersV1 {
                members: vec![self.member(&self.a), self.member(&self.b)],
            },
            recent_messages: MessagesV1 {
                messages: vec![self.message(&self.a, 1), self.message(&self.b, 2)],
                ..Default::default()
            },
            bans: BansV1(vec![self.owner_ban(Peer::new().id, 0)]),
            secrets: self.secrets(),
            ..Default::default()
        };
        state.post_apply_cleanup(&self.params).unwrap();
        state.verify(&state, &self.params).unwrap();
        state
    }
}

fn apply(
    state: &ChatRoomStateV1,
    params: &ChatRoomParametersV1,
    delta: ChatRoomStateV1Delta,
) -> ChatRoomStateV1 {
    let mut after = state.clone();
    after
        .apply_delta(state, params, &Some(delta))
        .unwrap_or_else(|e| panic!("delta must apply: {e}"));
    after
        .verify(&after, params)
        .unwrap_or_else(|e| panic!("state must verify: {e}"));
    after
}

fn bytes(state: &ChatRoomStateV1) -> Vec<u8> {
    let mut v = Vec::new();
    ciborium::ser::into_writer(state, &mut v).unwrap();
    v
}

fn shape(state: &ChatRoomStateV1) -> String {
    format!(
        "{} bans, {} messages",
        state.bans.0.len(),
        state.recent_messages.messages.len()
    )
}

/// Check `twice` against `once` directly and after a merge in each direction.
fn assert_converged(
    label: &str,
    twice: &ChatRoomStateV1,
    once: &ChatRoomStateV1,
    params: &ChatRoomParametersV1,
) {
    assert!(
        bytes(twice) == bytes(once),
        "{label}: delta results differ ({} vs {})",
        shape(twice),
        shape(once)
    );
    let mut t = twice.clone();
    t.merge(twice, params, once).unwrap();
    let mut o = once.clone();
    o.merge(once, params, twice).unwrap();
    assert!(
        bytes(&t) == bytes(&o),
        "{label}: states differ after merging ({} vs {})",
        shape(&t),
        shape(&o)
    );
    assert!(
        bytes(&t) == bytes(once),
        "{label}: merge changed the single-copy state"
    );
}

#[test]
fn a_ban_repeated_in_one_delta_is_stored_once() {
    let room = Room::new();
    let base = room.baseline();
    let absent = Peer::new();
    let ban = room.owner_ban(absent.id, 3);
    let other = room.owner_ban(Peer::new().id, 4);

    let once = apply(
        &base,
        &room.params,
        ChatRoomStateV1Delta {
            bans: Some(vec![ban.clone(), other.clone()]),
            ..Default::default()
        },
    );
    // The repeat is placed both adjacent and apart, so the result cannot
    // depend on where in the delta it sits.
    for (label, bans) in [
        ("adjacent", vec![ban.clone(), ban.clone(), other.clone()]),
        ("apart", vec![ban.clone(), other.clone(), ban.clone()]),
    ] {
        let twice = apply(
            &base,
            &room.params,
            ChatRoomStateV1Delta {
                bans: Some(bans),
                ..Default::default()
            },
        );
        assert_eq!(
            twice.bans.0.len(),
            3,
            "{label}: the stored ban and both distinct new bans fit the cap"
        );
        assert_converged(label, &twice, &once, &room.params);
    }
}

#[test]
fn a_message_repeated_in_one_delta_is_stored_once() {
    for privacy in [PrivacyMode::Public, PrivacyMode::Private] {
        message_repeated_case(privacy);
    }
}

fn message_repeated_case(privacy: PrivacyMode) {
    let room = Room::with_privacy(privacy);
    let base = room.baseline();
    let m = room.message(&room.a, 10);
    let other = room.message(&room.b, 11);

    let once = apply(
        &base,
        &room.params,
        ChatRoomStateV1Delta {
            recent_messages: Some(vec![m.clone(), other.clone()]),
            ..Default::default()
        },
    );
    for (label, messages) in [
        ("adjacent", vec![m.clone(), m.clone(), other.clone()]),
        ("apart", vec![m.clone(), other.clone(), m.clone()]),
    ] {
        let twice = apply(
            &base,
            &room.params,
            ChatRoomStateV1Delta {
                recent_messages: Some(messages),
                ..Default::default()
            },
        );
        assert_converged(label, &twice, &once, &room.params);
    }
}

/// The banned user is a current member, so cleanup also removes the member and
/// their message in the same apply.
#[test]
fn a_ban_of_a_current_member_repeated_in_one_delta_is_stored_once() {
    let room = Room::new();
    let base = room.baseline();
    let ban = room.owner_ban(room.b.id, 3);

    let once = apply(
        &base,
        &room.params,
        ChatRoomStateV1Delta {
            bans: Some(vec![ban.clone()]),
            ..Default::default()
        },
    );
    assert!(
        once.members
            .members
            .iter()
            .all(|m| m.member.id() != room.b.id),
        "the banned member is removed"
    );
    let twice = apply(
        &base,
        &room.params,
        ChatRoomStateV1Delta {
            bans: Some(vec![ban.clone(), ban]),
            ..Default::default()
        },
    );
    assert_eq!(twice.bans.0.len(), 2, "the stored ban and the new ban");
    assert_converged("current member", &twice, &once, &room.params);
}

/// The result of applying `delta` to `base`: the state's bytes, or `None` if
/// the delta is rejected or the result does not verify.
fn outcome(
    base: &ChatRoomStateV1,
    params: &ChatRoomParametersV1,
    delta: ChatRoomStateV1Delta,
) -> Option<Vec<u8>> {
    let mut after = base.clone();
    after.apply_delta(base, params, &Some(delta)).ok()?;
    after.verify(&after, params).ok()?;
    Some(bytes(&after))
}

/// `first` and `second` hold records that share an id. Applying them in
/// either order gives the same outcome.
fn assert_order_independent(
    label: &str,
    base: &ChatRoomStateV1,
    params: &ChatRoomParametersV1,
    delta: impl Fn(Vec<AuthorizedUserBan>, Vec<AuthorizedMessageV1>) -> ChatRoomStateV1Delta,
    first: (Vec<AuthorizedUserBan>, Vec<AuthorizedMessageV1>),
    second: (Vec<AuthorizedUserBan>, Vec<AuthorizedMessageV1>),
) {
    let a = outcome(base, params, delta(first.0, first.1));
    let b = outcome(base, params, delta(second.0, second.1));
    assert!(
        a == b,
        "{label}: the outcome depends on the order in the delta"
    );
}

fn bans_delta(bans: Vec<AuthorizedUserBan>, _: Vec<AuthorizedMessageV1>) -> ChatRoomStateV1Delta {
    ChatRoomStateV1Delta {
        bans: Some(bans),
        ..Default::default()
    }
}

fn messages_delta(
    _: Vec<AuthorizedUserBan>,
    messages: Vec<AuthorizedMessageV1>,
) -> ChatRoomStateV1Delta {
    ChatRoomStateV1Delta {
        recent_messages: Some(messages),
        ..Default::default()
    }
}

#[test]
fn bans_sharing_an_id_are_order_independent() {
    let room = Room::new();
    let base = room.baseline();

    // Issued by the owner, whose signature is checked.
    let genuine = room.owner_ban(Peer::new().id, 5);
    let mut same_id = genuine.clone();
    same_id.ban.banned_user = Peer::new().id;
    assert_eq!(same_id.id(), genuine.id());
    assert_order_independent(
        "owner",
        &base,
        &room.params,
        bans_delta,
        (vec![same_id.clone(), genuine.clone()], vec![]),
        (vec![genuine, same_id], vec![]),
    );

    // Issued by an id that is not a member, whose signature cannot be checked
    // when the bans apply; cleanup then removes both.
    let absent = Peer::new();
    let genuine = AuthorizedUserBan::new(
        UserBan {
            owner_member_id: room.owner.id,
            banned_at: Room::at(5),
            banned_user: Peer::new().id,
        },
        absent.id,
        &absent.sk,
    );
    let mut same_id = genuine.clone();
    same_id.ban.banned_user = Peer::new().id;
    let both = outcome(
        &base,
        &room.params,
        bans_delta(vec![same_id.clone(), genuine.clone()], vec![]),
    );
    assert!(both.is_some(), "non-member banner: the delta applies");
    assert_order_independent(
        "non-member banner",
        &base,
        &room.params,
        bans_delta,
        (vec![same_id.clone(), genuine.clone()], vec![]),
        (vec![genuine, same_id], vec![]),
    );
}

/// The banner joins in the same delta as the two bans, so their signatures are
/// first checked after the bans apply.
#[test]
fn bans_sharing_an_id_from_a_rejoining_banner_are_order_independent() {
    let room = Room::new();
    let base = room.baseline();
    let banner = Peer::new();
    let genuine = AuthorizedUserBan::new(
        UserBan {
            owner_member_id: room.owner.id,
            banned_at: Room::at(5),
            banned_user: Peer::new().id,
        },
        banner.id,
        &banner.sk,
    );
    let mut same_id = genuine.clone();
    same_id.ban.banned_user = Peer::new().id;
    let delta = |bans: Vec<AuthorizedUserBan>, _: Vec<AuthorizedMessageV1>| ChatRoomStateV1Delta {
        members: Some(MembersDelta::new(vec![room.member(&banner)])),
        bans: Some(bans),
        recent_messages: Some(vec![room.message(&banner, 6)]),
        ..Default::default()
    };
    let mut kept = base.clone();
    kept.apply_delta(
        &base,
        &room.params,
        &Some(delta(vec![same_id.clone(), genuine.clone()], vec![])),
    )
    .unwrap();
    assert!(
        kept.bans.0.contains(&genuine) && !kept.bans.0.contains(&same_id),
        "only the record with a valid signature is kept"
    );
    assert_order_independent(
        "rejoining banner",
        &base,
        &room.params,
        delta,
        (vec![same_id.clone(), genuine.clone()], vec![]),
        (vec![genuine, same_id], vec![]),
    );
}

#[test]
fn messages_sharing_an_id_are_order_independent() {
    for privacy in [PrivacyMode::Public, PrivacyMode::Private] {
        let room = Room::with_privacy(privacy.clone());
        let base = room.baseline();
        let genuine = room.message(&room.a, 10);
        let mut same_id = genuine.clone();
        same_id.message.time = Room::at(11);
        assert!(same_id.id() == genuine.id());
        assert_order_independent(
            &format!("{privacy:?}"),
            &base,
            &room.params,
            messages_delta,
            (vec![], vec![same_id.clone(), genuine.clone()]),
            (vec![], vec![genuine, same_id]),
        );
    }
}
