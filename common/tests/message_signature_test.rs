//! `MessagesV1::apply_delta` keeps a new message only if its signature
//! verifies against its author's key, and the result does not depend on the
//! order of messages within or across deltas.

use ed25519_dalek::SigningKey;
use freenet_scaffold::ComposableState;
use rand::rngs::OsRng;
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersV1};
use river_core::room_state::message::{
    AuthorizedMessageV1, MessageId, MessageV1, MessagesV1, RoomMessageBody,
};
use river_core::room_state::privacy::{PrivacyMode, RoomCipherSpec};
use river_core::room_state::secret::{
    AuthorizedSecretVersionRecord, RoomSecretsV1, SecretVersionRecordV1,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
use std::collections::HashSet;
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
}

impl Room {
    fn new() -> Self {
        let owner = Peer::new();
        let params = ChatRoomParametersV1 {
            owner: owner.sk.verifying_key(),
        };
        Self {
            owner,
            a: Peer::new(),
            b: Peer::new(),
            params,
        }
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

    /// A message attributed to `author` and signed with `signer`'s key.
    fn message(&self, author: &Peer, signer: &Peer, secs: u64) -> AuthorizedMessageV1 {
        AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: self.owner.id,
                author: author.id,
                time: SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000 + secs),
                content: RoomMessageBody::public(format!("message {secs}")),
            },
            &signer.sk,
        )
    }

    /// `a` and `b` are members, each with one message.
    fn baseline(&self) -> ChatRoomStateV1 {
        let mut state = ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(
                Configuration {
                    owner_member_id: self.owner.id,
                    max_members: 20,
                    max_recent_messages: 20,
                    ..Default::default()
                },
                &self.owner.sk,
            ),
            members: MembersV1 {
                members: vec![self.member(&self.a), self.member(&self.b)],
            },
            recent_messages: MessagesV1 {
                messages: vec![
                    self.message(&self.a, &self.a, 1),
                    self.message(&self.b, &self.b, 2),
                ],
                ..Default::default()
            },
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
    messages: Vec<AuthorizedMessageV1>,
) -> ChatRoomStateV1 {
    let mut after = state.clone();
    after
        .apply_delta(
            state,
            params,
            &Some(ChatRoomStateV1Delta {
                recent_messages: Some(messages),
                ..Default::default()
            }),
        )
        .unwrap_or_else(|e| panic!("delta must apply: {e}"));
    after
        .verify(&after, params)
        .unwrap_or_else(|e| panic!("state must verify: {e}"));
    after
}

fn ids(state: &ChatRoomStateV1) -> HashSet<MessageId> {
    state
        .recent_messages
        .messages
        .iter()
        .map(|m| m.id())
        .collect()
}

/// Messages attributed to the owner or to a member but signed with another
/// key are not kept; a correctly signed message in the same delta is.
#[test]
fn messages_not_signed_by_their_author_are_not_kept() {
    let room = Room::new();
    let base = room.baseline();
    let good = room.message(&room.a, &room.a, 10);
    for (label, bad) in [
        ("owner", room.message(&room.owner, &room.a, 11)),
        ("member", room.message(&room.b, &room.a, 12)),
    ] {
        let after = apply(&base, &room.params, vec![bad.clone(), good.clone()]);
        let kept = ids(&after);
        assert!(kept.contains(&good.id()), "{label}");
        assert!(!kept.contains(&bad.id()), "{label}");
        assert_eq!(kept.len(), ids(&base).len() + 1, "{label}");
    }
}

/// Messages that share an id are judged independently: in either order
/// within one delta, and across two deltas in either order, the one whose
/// signature verifies is kept and the other is not.
#[test]
fn messages_that_share_an_id_are_judged_independently() {
    let room = Room::new();
    let base = room.baseline();
    let genuine = room.message(&room.a, &room.a, 10);
    let mut same_id = genuine.clone();
    same_id.message.time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_011);
    assert!(same_id.id() == genuine.id());

    let one_delta = [
        apply(&base, &room.params, vec![same_id.clone(), genuine.clone()]),
        apply(&base, &room.params, vec![genuine.clone(), same_id.clone()]),
    ];
    let two_deltas = [
        apply(
            &apply(&base, &room.params, vec![same_id.clone()]),
            &room.params,
            vec![genuine.clone()],
        ),
        apply(
            &apply(&base, &room.params, vec![genuine.clone()]),
            &room.params,
            vec![same_id.clone()],
        ),
    ];
    for state in one_delta.iter().chain(two_deltas.iter()) {
        assert!(
            state.recent_messages.messages.contains(&genuine),
            "the genuine message is kept"
        );
        assert!(!state.recent_messages.messages.contains(&same_id));
        assert_eq!(state, &one_delta[0], "the order does not matter");
    }
}

/// In a private room, a message whose signature does not verify is dropped
/// even when it would otherwise fail the private-room checks (an unknown
/// secret version, or a public message), and the rest of the delta applies.
/// A correctly signed message that fails those checks still rejects the delta.
#[test]
fn unsigned_messages_do_not_fail_a_private_room_delta() {
    let room = Room::new();
    let mut base = room.baseline();
    base.configuration = AuthorizedConfigurationV1::new(
        Configuration {
            owner_member_id: room.owner.id,
            privacy_mode: PrivacyMode::Private,
            max_members: 20,
            max_recent_messages: 20,
            ..Default::default()
        },
        &room.owner.sk,
    );
    base.secrets = RoomSecretsV1 {
        current_version: 0,
        versions: vec![AuthorizedSecretVersionRecord::new(
            SecretVersionRecordV1 {
                version: 0,
                cipher_spec: RoomCipherSpec::Aes256Gcm,
                created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
            },
            &room.owner.sk,
        )],
        encrypted_secrets: vec![],
    };
    // No messages held, so only the delta's messages are in the result.
    base.recent_messages = MessagesV1::default();

    let private = |author: &Peer, signer: &Peer, secs: u64, version: u32| {
        AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: room.owner.id,
                author: author.id,
                time: SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000 + secs),
                content: RoomMessageBody::private_text(
                    format!("ciphertext {secs}").into_bytes(),
                    [0u8; 12],
                    version,
                ),
            },
            &signer.sk,
        )
    };
    let good = private(&room.a, &room.a, 10, 0);
    let unknown_version = private(&room.b, &room.a, 11, 7);
    let public = room.message(&room.b, &room.a, 12);

    let after = apply(
        &base,
        &room.params,
        vec![unknown_version.clone(), public.clone(), good.clone()],
    );
    assert_eq!(ids(&after), HashSet::from([good.id()]));

    // Validly signed but not admissible in a private room (an unknown
    // secret version, a public body): skipped like the unsigned ones, never
    // failing the delta (freenet/river#703).
    for signed_but_invalid in [
        private(&room.b, &room.b, 13, 7),
        room.message(&room.b, &room.b, 14),
    ] {
        let mut after = base.clone();
        after
            .apply_delta(
                &base,
                &room.params,
                &Some(ChatRoomStateV1Delta {
                    recent_messages: Some(vec![signed_but_invalid, good.clone()]),
                    ..Default::default()
                }),
            )
            .expect("an inadmissible message is skipped, not an error");
        assert_eq!(ids(&after), HashSet::from([good.id()]));
    }
}
