//! A private room built the way River builds one, read through [`LiveRoom`].

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use ed25519_dalek::SigningKey;
use freenet_stdlib::prelude::{ContractCode, ContractKey, Parameters};
use river_core::ecies::{encrypt_secret_for_member, encrypt_with_symmetric_key};
use river_core::room_state::ChatRoomStateV1;
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::content::{
    ActionContentV1, CONTENT_TYPE_REPLY, REPLY_CONTENT_VERSION, ReplyContentV1, TextContentV1,
};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
use river_core::room_state::privacy::{
    PrivacyMode, RoomCipherSpec, RoomDisplayMetadata, SealedBytes,
};
use river_core::room_state::secret::{
    AuthorizedEncryptedSecretForMember, AuthorizedSecretVersionRecord, EncryptedSecretForMemberV1,
    SecretVersionRecordV1,
};
use river_core::stored_rooms::StoredRoom;
use river_core::util::sign_struct;

use super::room::LiveRoom;
use crate::chat::model::Role;

const SECRET: [u8; 32] = [42; 32];

struct Fixture {
    owner: SigningKey,
    me: SigningKey,
}

impl Fixture {
    fn new() -> Self {
        Fixture {
            owner: SigningKey::from_bytes(&[1; 32]),
            me: SigningKey::from_bytes(&[2; 32]),
        }
    }

    fn owner_id(&self) -> MemberId {
        MemberId::from(&self.owner.verifying_key())
    }

    fn me_id(&self) -> MemberId {
        MemberId::from(&self.me.verifying_key())
    }

    fn seal(&self, text: &str) -> SealedBytes {
        let (ciphertext, nonce) = encrypt_with_symmetric_key(&SECRET, text.as_bytes());
        SealedBytes::private(ciphertext, nonce, 0, text.len() as u32)
    }

    fn private_body(&self, content_type: u32, version: u32, plain: &[u8]) -> RoomMessageBody {
        let (ciphertext, nonce) = encrypt_with_symmetric_key(&SECRET, plain);
        RoomMessageBody::private(content_type, version, ciphertext, nonce, 0)
    }

    fn message(
        &self,
        author: &SigningKey,
        secs: u64,
        content: RoomMessageBody,
    ) -> AuthorizedMessageV1 {
        AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: self.owner_id(),
                author: MemberId::from(&author.verifying_key()),
                time: SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
                content,
            },
            author,
        )
    }

    fn nickname(&self, who: &SigningKey, name: &str) -> AuthorizedMemberInfo {
        AuthorizedMemberInfo::new_with_member_key(
            MemberInfo {
                member_id: MemberId::from(&who.verifying_key()),
                version: 0,
                preferred_nickname: self.seal(name),
                deputies: Vec::new(),
            },
            who,
        )
    }

    /// A private room on secret v0, with the secret sealed to `me` by the owner.
    fn private_state(&self) -> ChatRoomStateV1 {
        let mut state = ChatRoomStateV1::default();
        let mut config = Configuration {
            owner_member_id: self.owner_id(),
            privacy_mode: PrivacyMode::Private,
            ..Configuration::default()
        };
        let (name_ct, name_nonce) = encrypt_with_symmetric_key(&SECRET, b"Secret garden");
        let (desc_ct, desc_nonce) = encrypt_with_symmetric_key(&SECRET, b"Plants only");
        config.display = RoomDisplayMetadata::private(
            name_ct,
            name_nonce,
            13,
            Some((desc_ct, desc_nonce, 11)),
            0,
        );
        state.configuration = AuthorizedConfigurationV1::new(config, &self.owner);
        state.members.members.push(AuthorizedMember::new(
            Member {
                owner_member_id: self.owner_id(),
                invited_by: self.owner_id(),
                member_vk: self.me.verifying_key(),
            },
            &self.owner,
        ));
        state
            .member_info
            .member_info
            .push(self.nickname(&self.owner, "Olive"));
        state
            .member_info
            .member_info
            .push(self.nickname(&self.me, "Mia"));
        let record = SecretVersionRecordV1 {
            version: 0,
            cipher_spec: RoomCipherSpec::Aes256Gcm,
            created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1),
        };
        state.secrets.versions.push(AuthorizedSecretVersionRecord {
            owner_signature: sign_struct(&record, &self.owner),
            record,
        });
        let (ciphertext, nonce, ephemeral) =
            encrypt_secret_for_member(&SECRET, &self.me.verifying_key());
        let secret = EncryptedSecretForMemberV1 {
            member_id: self.me_id(),
            secret_version: 0,
            ciphertext,
            nonce,
            sender_ephemeral_public_key: ephemeral.to_bytes(),
            provider: self.owner_id(),
        };
        state
            .secrets
            .encrypted_secrets
            .push(AuthorizedEncryptedSecretForMember {
                owner_signature: sign_struct(&secret, &self.owner),
                secret,
            });

        let hello = self.message(
            &self.owner,
            60,
            self.private_body(
                1,
                1,
                &TextContentV1::new("Hello @[Mia](rv:XXXX) and all".into()).encode(),
            ),
        );
        let hello_id = hello.id();
        let reply = self.message(
            &self.me,
            120,
            self.private_body(
                CONTENT_TYPE_REPLY,
                REPLY_CONTENT_VERSION,
                &ReplyContentV1::new("Hi Olive".into(), hello_id.clone(), "x".into(), "y".into())
                    .encode(),
            ),
        );
        let (react_ct, react_nonce) = encrypt_with_symmetric_key(
            &SECRET,
            &ActionContentV1::reaction(hello_id.clone(), "🌱".into()).encode(),
        );
        let reaction = self.message(
            &self.me,
            180,
            RoomMessageBody::private_action(react_ct, react_nonce, 0),
        );
        let (edit_ct, edit_nonce) = encrypt_with_symmetric_key(
            &SECRET,
            &ActionContentV1::edit(hello_id, "Hello everyone".into()).encode(),
        );
        let edit = self.message(
            &self.owner,
            240,
            RoomMessageBody::private_action(edit_ct, edit_nonce, 0),
        );
        state.recent_messages.messages = vec![hello, reply, reaction, edit];
        state
    }

    fn stored(&self, state: ChatRoomStateV1) -> StoredRoom {
        StoredRoom {
            owner_vk: self.owner.verifying_key(),
            room_state: state,
            self_sk: Some(self.me.clone()),
            self_vk: None,
            contract_key: ContractKey::from_params_and_code(
                Parameters::from(vec![1u8]),
                ContractCode::from(vec![0u8, 1, 2]),
            ),
            last_read_message_id: None,
            self_nickname: Some("Mia".into()),
            invitation_secrets: BTreeMap::new(),
        }
    }
}

fn minutes(t: SystemTime) -> u32 {
    (t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() / 60) as u32
}

#[test]
fn a_private_room_decrypts_with_the_secret_sealed_to_this_member() {
    let f = Fixture::new();
    let room = LiveRoom::new(f.stored(f.private_state())).view(&minutes);
    assert_eq!(room.name, "Secret garden");
    assert_eq!(room.description, "Plants only");
    assert!(room.private);
    let names: Vec<(&str, Role)> = room
        .members
        .iter()
        .map(|m| (m.nickname.as_str(), m.role))
        .collect();
    assert_eq!(names, [("Olive", Role::Owner), ("Mia", Role::Member)]);
    assert_eq!(room.nickname(room.me), "Mia");

    // The two actions are not messages; the edit and the reaction apply to the first.
    assert_eq!(room.messages.len(), 2);
    let hello = &room.messages[0];
    assert_eq!(hello.text, "Hello everyone");
    assert!(hello.edited);
    assert_eq!(hello.time, 1);
    assert_eq!(hello.reactions.len(), 1);
    assert_eq!(hello.reactions[0].emoji, "🌱");
    assert_eq!(hello.reactions[0].by, [room.me]);
    let reply = &room.messages[1];
    assert_eq!(reply.text, "Hi Olive");
    assert_eq!(reply.author, room.me);
    let to = reply.reply_to.as_ref().expect("a reply");
    assert_eq!(to.message, hello.id);
    assert_eq!(to.author, room.members[0].id);
    assert_eq!(to.preview, "Hello everyone");
}

#[test]
fn an_invitation_secret_is_used_when_no_blob_is_sealed_to_this_member() {
    let f = Fixture::new();
    let mut state = f.private_state();
    state.secrets.encrypted_secrets.clear();
    let mut stored = f.stored(state.clone());
    let room = LiveRoom::new(stored.clone()).view(&minutes);
    assert_eq!(room.name, "Private room");
    assert!(room.messages[1].text.starts_with("Decrypting messages"));

    stored.invitation_secrets.insert(0, SECRET);
    let room = LiveRoom::new(stored).view(&minutes);
    assert_eq!(room.name, "Secret garden");
    assert_eq!(room.messages[1].text, "Hi Olive");
}

#[test]
fn mentions_render_as_names() {
    let f = Fixture::new();
    let mut state = f.private_state();
    let mention = format!("Hello @[stale](rv:{}) and all", f.me_id());
    state.recent_messages.messages = vec![f.message(
        &f.owner,
        60,
        f.private_body(1, 1, &TextContentV1::new(mention).encode()),
    )];
    let room = LiveRoom::new(f.stored(state)).view(&minutes);
    assert_eq!(room.messages[0].text, "Hello @Mia and all");
}

#[test]
fn unread_counts_others_messages_after_the_last_read() {
    let f = Fixture::new();
    let state = f.private_state();
    let mut stored = f.stored(state.clone());
    let first = state.recent_messages.messages[0].id();
    stored.last_read_message_id = Some(first);
    // The only later message is this member's own reply.
    assert_eq!(LiveRoom::new(stored.clone()).view(&minutes).unread, 0);

    // As the owner, the reply is someone else's.
    stored.self_sk = Some(f.owner.clone());
    assert_eq!(LiveRoom::new(stored).view(&minutes).unread, 1);
}

#[test]
fn a_room_without_a_local_identity_shows_nobody_as_you() {
    let f = Fixture::new();
    let mut stored = f.stored(f.private_state());
    stored.self_sk = None;
    stored.self_vk = None;
    stored.invitation_secrets.insert(0, SECRET);
    let room = LiveRoom::new(stored).view(&minutes);
    assert_eq!(room.me, 0);
    assert!(room.members.iter().all(|m| m.id != 0));
}

#[test]
fn hidden_characters_are_dropped_from_names() {
    let f = Fixture::new();
    let mut state = f.private_state();
    state.member_info.member_info[0] = f.nickname(&f.owner, "Ol\u{202E}ive\u{200B}  \n Smith");
    let room = LiveRoom::new(f.stored(state)).view(&minutes);
    assert_eq!(room.members[0].nickname, "Olive Smith");
}

/// A delta from an update notification reaches the view: the new message shows and the
/// private actions are rebuilt with it.
#[test]
fn a_delta_adds_messages_and_rebuilds_actions() {
    let f = Fixture::new();
    let state = f.private_state();
    let mut live = LiveRoom::new(f.stored(state.clone()));
    let later = f.message(
        &f.owner,
        600,
        f.private_body(1, 1, &TextContentV1::new("Later".into()).encode()),
    );
    let delta = river_core::room_state::ChatRoomStateV1Delta {
        recent_messages: Some(vec![later]),
        ..Default::default()
    };
    live.apply_delta(delta).expect("the delta applies");
    let room = live.view(&minutes);
    assert_eq!(room.messages.len(), 3);
    assert_eq!(room.messages[2].text, "Later");
    assert_eq!(room.messages[0].text, "Hello everyone");
}
