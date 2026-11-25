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

pub(super) const SECRET: [u8; 32] = [42; 32];

pub(super) struct Fixture {
    pub(super) owner: SigningKey,
    pub(super) me: SigningKey,
}

impl Fixture {
    pub(super) fn new() -> Self {
        Fixture {
            owner: SigningKey::from_bytes(&[1; 32]),
            me: SigningKey::from_bytes(&[2; 32]),
        }
    }

    pub(super) fn owner_id(&self) -> MemberId {
        MemberId::from(&self.owner.verifying_key())
    }

    pub(super) fn me_id(&self) -> MemberId {
        MemberId::from(&self.me.verifying_key())
    }

    pub(super) fn seal(&self, text: &str) -> SealedBytes {
        let (ciphertext, nonce) = encrypt_with_symmetric_key(&SECRET, text.as_bytes());
        SealedBytes::private(ciphertext, nonce, 0, text.len() as u32)
    }

    pub(super) fn private_body(
        &self,
        content_type: u32,
        version: u32,
        plain: &[u8],
    ) -> RoomMessageBody {
        let (ciphertext, nonce) = encrypt_with_symmetric_key(&SECRET, plain);
        RoomMessageBody::private(content_type, version, ciphertext, nonce, 0)
    }

    pub(super) fn message(
        &self,
        author: &SigningKey,
        secs: u64,
        content: RoomMessageBody,
    ) -> AuthorizedMessageV1 {
        AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: self.owner_id(),
                author: MemberId::from(&author.verifying_key()),
                time: (SystemTime::UNIX_EPOCH + Duration::from_secs(secs)).into(),
                content,
            },
            author,
        )
    }

    pub(super) fn nickname(&self, who: &SigningKey, name: &str) -> AuthorizedMemberInfo {
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
    pub(super) fn private_state(&self) -> ChatRoomStateV1 {
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
            created_at: (SystemTime::UNIX_EPOCH + Duration::from_secs(1)).into(),
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

    pub(super) fn stored(&self, state: ChatRoomStateV1) -> StoredRoom {
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
            self_authorized_member: None,
            invite_chain: Vec::new(),
            self_member_info: None,
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
    assert!(
        room.messages[1]
            .text
            .contains("doesn't have the room's secret")
    );
    // A nickname that cannot be decrypted is not shown as ciphertext.
    assert!(
        room.members
            .iter()
            .all(|m| m.nickname.starts_with("Member "))
    );

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

/// Add secret `version` to the room, sealed by the owner to `member`.
pub(super) fn add_secret(
    f: &Fixture,
    state: &mut ChatRoomStateV1,
    version: u32,
    secret: &[u8; 32],
    member: &SigningKey,
) {
    if !state
        .secrets
        .versions
        .iter()
        .any(|v| v.record.version == version)
    {
        let record = SecretVersionRecordV1 {
            version,
            cipher_spec: RoomCipherSpec::Aes256Gcm,
            created_at: (SystemTime::UNIX_EPOCH + Duration::from_secs(u64::from(version) + 1))
                .into(),
        };
        state.secrets.versions.push(AuthorizedSecretVersionRecord {
            owner_signature: sign_struct(&record, &f.owner),
            record,
        });
        state.secrets.current_version = state.secrets.current_version.max(version);
    }
    let (ciphertext, nonce, ephemeral) = encrypt_secret_for_member(secret, &member.verifying_key());
    let blob = EncryptedSecretForMemberV1 {
        member_id: MemberId::from(&member.verifying_key()),
        secret_version: version,
        ciphertext,
        nonce,
        sender_ephemeral_public_key: ephemeral.to_bytes(),
        provider: f.owner_id(),
    };
    state
        .secrets
        .encrypted_secrets
        .push(AuthorizedEncryptedSecretForMember {
            owner_signature: sign_struct(&blob, &f.owner),
            secret: blob,
        });
}

fn text_at(
    f: &Fixture,
    secs: u64,
    version: u32,
    secret: &[u8; 32],
    text: &str,
) -> AuthorizedMessageV1 {
    let (ct, nonce) = encrypt_with_symmetric_key(secret, &TextContentV1::new(text.into()).encode());
    f.message(
        &f.owner,
        secs,
        RoomMessageBody::private_text(ct, nonce, version),
    )
}

/// A delta carrying private actions: the edit and the reaction only show if the actions are
/// rebuilt with the decrypted payloads after the delta lands (`MessagesV1::apply_delta` only
/// rebuilds public actions).
#[test]
fn a_delta_with_private_actions_shows_their_effect() {
    let f = Fixture::new();
    let mut live = LiveRoom::new(f.stored(f.private_state()));
    let reply_id = live.stored.room_state.recent_messages.messages[1].id();
    let (react_ct, react_nonce) = encrypt_with_symmetric_key(
        &SECRET,
        &ActionContentV1::reaction(reply_id.clone(), "🎉".into()).encode(),
    );
    let (edit_ct, edit_nonce) = encrypt_with_symmetric_key(
        &SECRET,
        &ActionContentV1::edit(reply_id, "Hi Olive, edited".into()).encode(),
    );
    let delta = river_core::room_state::ChatRoomStateV1Delta {
        recent_messages: Some(vec![
            f.message(
                &f.owner,
                500,
                RoomMessageBody::private_action(react_ct, react_nonce, 0),
            ),
            f.message(
                &f.me,
                510,
                RoomMessageBody::private_action(edit_ct, edit_nonce, 0),
            ),
        ]),
        ..Default::default()
    };
    live.apply_delta(delta).expect("the delta applies");
    let room = live.view(&minutes);
    assert_eq!(room.messages.len(), 2, "actions are not messages");
    let reply = &room.messages[1];
    assert_eq!(reply.text, "Hi Olive, edited");
    assert!(reply.edited);
    assert_eq!(reply.reactions.len(), 1);
    assert_eq!(reply.reactions[0].emoji, "🎉");
    assert_eq!(reply.reactions[0].by, [room.members[0].id]);
}

/// A whole state from the network merges in, and its private messages decrypt.
#[test]
fn a_merged_state_brings_in_new_private_messages() {
    let f = Fixture::new();
    let state = f.private_state();
    let mut live = LiveRoom::new(f.stored(state.clone()));
    let mut incoming = state;
    incoming
        .recent_messages
        .messages
        .push(text_at(&f, 700, 0, &SECRET, "From the network"));
    live.merge_state(&incoming).expect("merges");
    let room = live.view(&minutes);
    assert_eq!(room.messages.last().unwrap().text, "From the network");
    assert_eq!(
        room.messages[0].text, "Hello everyone",
        "actions still apply"
    );
}

#[test]
fn a_public_room_reads_without_secrets() {
    let f = Fixture::new();
    let mut state = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: f.owner_id(),
                display: RoomDisplayMetadata::public("Town".into(), Some("Everyone".into())),
                ..Configuration::default()
            },
            &f.owner,
        ),
        ..Default::default()
    };
    state
        .member_info
        .member_info
        .push(AuthorizedMemberInfo::new_with_member_key(
            MemberInfo {
                member_id: f.owner_id(),
                version: 0,
                preferred_nickname: SealedBytes::public(b"Olive".to_vec()),
                deputies: Vec::new(),
            },
            &f.owner,
        ));
    state.recent_messages.messages =
        vec![f.message(&f.owner, 60, RoomMessageBody::public("Hello town".into()))];
    let room = LiveRoom::new(f.stored(state)).view(&minutes);
    assert!(!room.private);
    assert_eq!(
        (room.name.as_str(), room.description.as_str()),
        ("Town", "Everyone")
    );
    assert_eq!(room.messages[0].text, "Hello town");
    assert_eq!(room.nickname(room.messages[0].author), "Olive");
}

#[test]
fn a_blob_sealed_to_another_member_does_not_decrypt() {
    let f = Fixture::new();
    let mut state = f.private_state();
    state.secrets.encrypted_secrets.clear();
    let stranger = SigningKey::from_bytes(&[9; 32]);
    add_secret(&f, &mut state, 0, &SECRET, &stranger);
    let room = LiveRoom::new(f.stored(state)).view(&minutes);
    assert_eq!(room.name, "Private room");
    assert!(
        room.messages[1]
            .text
            .contains("doesn't have the room's secret")
    );
}

/// The owner-signed blob wins over an invitation's copy of the same version, as in the web UI.
#[test]
fn the_owner_signed_secret_wins_over_a_wrong_invitation_secret() {
    let f = Fixture::new();
    let mut stored = f.stored(f.private_state());
    stored.invitation_secrets.insert(0, [13; 32]);
    let room = LiveRoom::new(stored).view(&minutes);
    assert_eq!(room.name, "Secret garden");
    assert_eq!(room.messages[1].text, "Hi Olive");
}

#[test]
fn messages_under_several_secret_versions_each_decrypt_and_a_missing_one_says_so() {
    let f = Fixture::new();
    let mut state = f.private_state();
    let second = [21u8; 32];
    add_secret(&f, &mut state, 1, &second, &f.me);
    // Version 2 exists in the room, but nothing seals it to this member.
    add_secret(
        &f,
        &mut state,
        2,
        &[22u8; 32],
        &SigningKey::from_bytes(&[9; 32]),
    );
    state
        .recent_messages
        .messages
        .push(text_at(&f, 800, 1, &second, "Under v1"));
    state
        .recent_messages
        .messages
        .push(text_at(&f, 900, 2, &[22u8; 32], "Under v2"));
    let room = LiveRoom::new(f.stored(state)).view(&minutes);
    let texts: Vec<&str> = room.messages.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts[1], "Hi Olive");
    assert_eq!(texts[2], "Under v1");
    assert_eq!(texts[3], "[Encrypted message - secret v2 unavailable]");
}

/// A message's time is its author's choice; no time panics.
#[test]
fn any_message_time_shows_without_panicking() {
    use super::room::local_minutes;
    assert_eq!(
        local_minutes(SystemTime::UNIX_EPOCH - Duration::from_secs(1)),
        0
    );
    // Past what message time can hold (the year 10136) yet inside every platform's
    // SystemTime (Windows' ends near the year 30828): about the year 10680.
    let far = SystemTime::UNIX_EPOCH + Duration::from_secs(1 << 38);
    // Clamped to the last representable minute, give or take the local time zone.
    assert!(local_minutes(far) >= u32::MAX - 24 * 60);
    let now = local_minutes(SystemTime::now());
    let utc_now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        / 60;
    // Local time is within a day of UTC.
    assert!(
        u64::from(now).abs_diff(utc_now) <= 24 * 60,
        "{now} vs {utc_now}"
    );

    let f = Fixture::new();
    let mut state = f.private_state();
    // A time before 1970 cannot be serialized, so it never arrives; one far ahead can.
    for (i, t) in [far].into_iter().enumerate() {
        let (ct, nonce) =
            encrypt_with_symmetric_key(&SECRET, &TextContentV1::new(format!("t{i}")).encode());
        state
            .recent_messages
            .messages
            .push(AuthorizedMessageV1::new(
                MessageV1 {
                    room_owner: f.owner_id(),
                    author: f.owner_id(),
                    time: t.into(),
                    content: RoomMessageBody::private_text(ct, nonce, 0),
                },
                &f.owner,
            ));
    }
    let room = LiveRoom::new(f.stored(state)).view(&local_minutes);
    assert_eq!(room.messages.len(), 3);
}

/// A merge the room's rules reject leaves the room exactly as it was.
#[test]
fn a_rejected_merge_changes_nothing() {
    let f = Fixture::new();
    let state = f.private_state();
    let mut live = LiveRoom::new(f.stored(state.clone()));
    let before = live.stored.room_state.clone();
    let mut incoming = state;
    incoming
        .recent_messages
        .messages
        .push(text_at(&f, 700, 0, &SECRET, "should not land"));
    // An upgrade pointer signed by someone other than the owner.
    incoming.upgrade = river_core::room_state::upgrade::OptionalUpgradeV1(Some(
        river_core::room_state::upgrade::AuthorizedUpgradeV1::new(
            river_core::room_state::upgrade::UpgradeV1 {
                owner_member_id: f.owner_id(),
                version: 1,
                new_chatroom_address: blake3::Hash::from([0; 32]),
            },
            &SigningKey::from_bytes(&[99; 32]),
        ),
    ));
    assert!(live.merge_state(&incoming).is_err());
    assert_eq!(live.stored.room_state, before);
}

/// An upgrade pointer signed by someone other than the owner: every merge refuses it, and the
/// upgrade is the room's last step but two, after the messages.
fn forged_upgrade(f: &Fixture) -> river_core::room_state::upgrade::OptionalUpgradeV1 {
    river_core::room_state::upgrade::OptionalUpgradeV1(Some(
        river_core::room_state::upgrade::AuthorizedUpgradeV1::new(
            river_core::room_state::upgrade::UpgradeV1 {
                owner_member_id: f.owner_id(),
                version: 1,
                new_chatroom_address: blake3::Hash::from([0; 32]),
            },
            &SigningKey::from_bytes(&[99; 32]),
        ),
    ))
}

/// What the network sends is merged as the web UI merges it (`room_synchronizer.rs`), in place:
/// the steps before the one that fails stay. Rolling it all back would keep a message out for
/// good, since every later notification and every read again carries the same failing step
/// (freenet/river#971). The merge still says it failed, so the room is named as not updated.
#[test]
fn a_network_state_that_fails_partway_keeps_the_steps_before_the_failure() {
    let f = Fixture::new();
    let state = f.private_state();
    let mut live = LiveRoom::new(f.stored(state.clone()));
    let mut incoming = state;
    incoming
        .recent_messages
        .messages
        .push(text_at(&f, 700, 0, &SECRET, "lands anyway"));
    incoming.upgrade = forged_upgrade(&f);
    assert!(live.merge_network_state(&incoming).is_err());
    let room = live.view(&minutes);
    assert_eq!(
        room.messages.last().map(|m| m.text.as_str()),
        Some("lands anyway"),
        "the messages step came before the failing one and stays, decrypted"
    );
    assert!(
        live.stored.room_state.upgrade.0.is_none(),
        "the forged pointer is not kept"
    );
}

/// The same for a delta from a notification.
#[test]
fn a_network_delta_that_fails_partway_keeps_the_steps_before_the_failure() {
    let f = Fixture::new();
    let mut live = LiveRoom::new(f.stored(f.private_state()));
    let delta = river_core::room_state::ChatRoomStateV1Delta {
        recent_messages: Some(vec![text_at(&f, 700, 0, &SECRET, "lands anyway")]),
        upgrade: forged_upgrade(&f).0,
        ..Default::default()
    };
    assert!(live.apply_network_delta(delta).is_err());
    let room = live.view(&minutes);
    assert_eq!(
        room.messages.last().map(|m| m.text.as_str()),
        Some("lands anyway")
    );
    assert!(live.stored.room_state.upgrade.0.is_none());
}

/// The private room with its message cap set to the messages it holds, so any new message is
/// one too many until cleanup drops the oldest.
fn full_room(f: &Fixture) -> ChatRoomStateV1 {
    let mut state = f.private_state();
    let mut config = state.configuration.configuration.clone();
    config.max_recent_messages = state.recent_messages.messages.len();
    state.configuration = AuthorizedConfigurationV1::new(config, &f.owner);
    state
}

/// What a partial merge keeps is cleaned up as a whole merge would be: the room's caps hold
/// (here the message cap), so a room that fails partway again and again cannot grow past them.
#[test]
fn a_network_state_that_fails_partway_keeps_only_what_cleanup_allows() {
    let f = Fixture::new();
    let state = full_room(&f);
    let cap = state.configuration.configuration.max_recent_messages;
    let mut live = LiveRoom::new(f.stored(state.clone()));
    let mut incoming = state;
    for (i, text) in ["newer", "newest"].iter().enumerate() {
        incoming
            .recent_messages
            .messages
            .push(text_at(&f, 700 + i as u64, 0, &SECRET, text));
    }
    incoming.upgrade = forged_upgrade(&f);
    assert!(live.merge_network_state(&incoming).is_err());
    let kept = &live.stored.room_state;
    assert_eq!(kept.recent_messages.messages.len(), cap, "the cap holds");
    assert_eq!(
        kept.normalized(&live.params()).unwrap(),
        *kept,
        "cleaned up"
    );
    let room = live.view(&minutes);
    assert_eq!(
        room.messages.last().map(|m| m.text.as_str()),
        Some("newest")
    );
}

/// The same for a delta from a notification.
#[test]
fn a_network_delta_that_fails_partway_keeps_only_what_cleanup_allows() {
    let f = Fixture::new();
    let state = full_room(&f);
    let cap = state.configuration.configuration.max_recent_messages;
    let mut live = LiveRoom::new(f.stored(state));
    let delta = river_core::room_state::ChatRoomStateV1Delta {
        recent_messages: Some(vec![
            text_at(&f, 700, 0, &SECRET, "newer"),
            text_at(&f, 701, 0, &SECRET, "newest"),
        ]),
        upgrade: forged_upgrade(&f).0,
        ..Default::default()
    };
    assert!(live.apply_network_delta(delta).is_err());
    let kept = &live.stored.room_state;
    assert_eq!(kept.recent_messages.messages.len(), cap);
    assert_eq!(kept.normalized(&live.params()).unwrap(), *kept);
}

/// If the cleanup of what a failed merge kept itself fails, nothing is kept: the copy is
/// exactly what it was, and nothing is reported as changed.
#[test]
fn a_failed_merge_whose_cleanup_fails_keeps_nothing() {
    let f = Fixture::new();
    let state = full_room(&f);
    let mut live = LiveRoom::new(f.stored(state.clone()));
    let before = live.stored.room_state.clone();
    let mut incoming = state;
    incoming
        .recent_messages
        .messages
        .push(text_at(&f, 700, 0, &SECRET, "not kept"));
    incoming.upgrade = forged_upgrade(&f);
    let merged = live.merge_network_state_failing_cleanup(&incoming);
    assert!(merged.is_err(), "{merged:?}");
    assert!(!merged.changed);
    assert_eq!(live.stored.room_state, before);
}

/// A secret blob addressed to this member that does not decrypt is skipped, not fatal: the
/// room still opens, with the invitation's copy of that version's secret.
#[test]
fn a_secret_blob_that_does_not_decrypt_is_skipped() {
    let f = Fixture::new();
    let mut state = f.private_state();
    for blob in &mut state.secrets.encrypted_secrets {
        blob.secret.ciphertext = vec![9; 7];
    }
    let mut stored = f.stored(state);
    stored.invitation_secrets = BTreeMap::from([(0, SECRET)]);
    let room = LiveRoom::new(stored).view(&minutes);
    assert_eq!(room.name, "Secret garden");
}

// A reply whose target is not in the room, built through the real conversion.

/// The reply from `private_state` alone: its target ("Hello ...") is not shown.
fn state_with_only_the_reply(mut state: ChatRoomStateV1) -> ChatRoomStateV1 {
    state.recent_messages.messages = state
        .recent_messages
        .messages
        .iter()
        .filter(|m| m.message.time == (SystemTime::UNIX_EPOCH + Duration::from_secs(120)).into())
        .cloned()
        .collect();
    assert_eq!(state.recent_messages.messages.len(), 1);
    state
}

fn strip_of(rt: &rune::Runtime<crate::RiverApp>, message_key: &str) -> rune::ElementInfo {
    rt.elements()
        .into_iter()
        .find(|e| e.path.ends_with(&format!("msg-{message_key}/reply")))
        .unwrap_or_else(|| panic!("no strip on {message_key}"))
}

fn render(room: crate::chat::Room) -> rune::Runtime<crate::RiverApp> {
    let mut s = crate::RiverApp::detached();
    s.setup = crate::Setup::Connected {
        version: "0.2.155".into(),
    };
    s.chat = crate::chat::Chat::new(
        crate::chat::Source::Node(crate::chat::LinkStatus::Live),
        Vec::new(),
        0,
        0,
        0,
    );
    s.chat
        .show_node_rooms(vec![room], None, crate::chat::LinkStatus::Live, 29_000_000);
    let mut rt = rune::Runtime::new(
        s,
        crate::view,
        crate::WINDOW_SIZE,
        rune::Env {
            system_dark: true,
            scale: 1.0,
        },
    );
    rt.frame(0.0);
    for i in 1..=30 {
        rt.frame(f64::from(i) * 0.05);
    }
    rt
}

#[test]
fn a_reply_to_a_message_the_room_does_not_show_is_kept_as_unavailable() {
    let f = Fixture::new();
    let room = LiveRoom::new(f.stored(state_with_only_the_reply(f.private_state()))).view(&minutes);
    assert_eq!(room.messages.len(), 1);
    let reply = &room.messages[0];
    assert!(reply.reply_to.is_none(), "there is no target to quote");
    assert!(reply.reply_gone, "but it is a reply");
    // A plain message is not a reply.
    let plain = f.private_state();
    let room = LiveRoom::new(f.stored(plain)).view(&minutes);
    assert!(!room.messages[0].reply_gone);
    assert!(!room.messages[1].reply_gone);
}

#[test]
fn the_unavailable_strip_is_drawn_for_it_and_cannot_be_pressed() {
    let f = Fixture::new();
    let room = LiveRoom::new(f.stored(state_with_only_the_reply(f.private_state()))).view(&minutes);
    let key = room.messages[0].key.clone();
    let mut rt = render(room);
    let strip = strip_of(&rt, &key);
    assert_ne!(strip.role, "button");
    assert!(
        rt.elements()
            .into_iter()
            .any(|e| e.text.as_deref() == Some("Original message unavailable"))
    );
    // Pressing it does nothing.
    let (x, y) = rt.find(&strip.path).expect("drawn");
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(1.0);
    assert!(rt.state.chat.pending_reveal().is_none());
    assert_eq!(rt.state.chat.flashing(), None);
}

#[test]
fn a_reply_that_arrives_before_its_target_becomes_a_jump_when_the_target_does() {
    let f = Fixture::new();
    let full = f.private_state();
    let target = full
        .recent_messages
        .messages
        .iter()
        .find(|m| m.message.time == (SystemTime::UNIX_EPOCH + Duration::from_secs(60)).into())
        .cloned()
        .expect("the target");
    let mut live = LiveRoom::new(f.stored(state_with_only_the_reply(full.clone())));
    assert!(live.view(&minutes).messages[0].reply_gone);
    live.apply_delta(river_core::room_state::ChatRoomStateV1Delta {
        recent_messages: Some(vec![target]),
        ..Default::default()
    })
    .expect("the target arrives");
    let room = live.view(&minutes);
    let reply = room
        .messages
        .iter()
        .find(|m| m.text == "Hi Olive")
        .expect("the reply");
    assert!(!reply.reply_gone);
    let quoted = reply.reply_to.as_ref().expect("now it quotes");
    assert!(room.messages.iter().any(|m| m.id == quoted.message));
    let key = reply.key.clone();
    let rt = render(room);
    let strip = strip_of(&rt, &key);
    assert_eq!(strip.role, "button");
}

// Join events ----------------------------------------------------------------------------------

/// The private state with join events by `who` at `secs` seconds spliced in, each after the
/// message at index `after` of the original four (hello, reply, reaction, edit).
fn with_joins(f: &Fixture, joins: &[(usize, &SigningKey, u64)]) -> ChatRoomStateV1 {
    with_joins_on(f, f.private_state(), joins)
}

/// [`with_joins`] on a state already built (a state built twice differs: sealing is random).
fn with_joins_on(
    f: &Fixture,
    mut state: ChatRoomStateV1,
    joins: &[(usize, &SigningKey, u64)],
) -> ChatRoomStateV1 {
    let original = state.recent_messages.messages.clone();
    let mut out = Vec::new();
    for (after, who, secs) in joins {
        if *after == usize::MAX {
            out.push(f.message(who, *secs, RoomMessageBody::join_event()));
        }
    }
    for (i, m) in original.into_iter().enumerate() {
        out.push(m);
        for (after, who, secs) in joins {
            if *after == i {
                out.push(f.message(who, *secs, RoomMessageBody::join_event()));
            }
        }
    }
    state.recent_messages.messages = out;
    state
}

#[test]
fn a_join_event_is_a_line_between_the_messages_not_a_message() {
    let f = Fixture::new();
    let room = LiveRoom::new(f.stored(with_joins(&f, &[(0, &f.me, 90)]))).view(&minutes);
    // The same two messages as without it: the event is no message.
    assert_eq!(room.messages.len(), 2);
    assert_eq!(room.joins.len(), 1);
    let join = &room.joins[0];
    assert_eq!(join.names, ["Mia"]);
    assert_eq!(join.before, 1, "after the first message, before the reply");
    assert_eq!(join.line().0, "Mia joined");
}

#[test]
fn joins_with_no_message_between_them_fold_within_an_hour() {
    let f = Fixture::new();
    // Two joins a minute apart, a third 61 minutes after the second.
    let state = with_joins(
        &f,
        &[
            (0, &f.me, 90),
            (0, &f.owner, 150),
            (0, &f.me, 150 + 61 * 60),
        ],
    );
    let room = LiveRoom::new(f.stored(state)).view(&minutes);
    assert_eq!(room.joins.len(), 2);
    assert_eq!(room.joins[0].names, ["Mia", "Olive"]);
    assert_eq!(room.joins[0].line().0, "Mia and Olive joined");
    assert_eq!(room.joins[1].names, ["Mia"]);
    assert_eq!(room.joins[0].before, room.joins[1].before);
}

#[test]
fn a_message_between_two_joins_keeps_them_apart() {
    let f = Fixture::new();
    let state = with_joins(&f, &[(0, &f.me, 90), (1, &f.owner, 130)]);
    let room = LiveRoom::new(f.stored(state)).view(&minutes);
    assert_eq!(room.joins.len(), 2);
    assert_eq!((room.joins[0].before, room.joins[1].before), (1, 2));
}

#[test]
fn three_joins_are_counted_not_named_in_the_line() {
    let join = |names: &[&str]| crate::chat::model::Join {
        key: "k".into(),
        names: names.iter().map(|n| n.to_string()).collect(),
        time: 0,
        before: 0,
    };
    let (line, bold) = join(&["Ann", "Bo", "Cy"]).line();
    assert_eq!(line, "3 people joined");
    assert!(bold.is_empty());
    let (line, bold) = join(&["Ann", "Bo"]).line();
    let named: Vec<&str> = bold.iter().map(|r| &line[r.clone()]).collect();
    assert_eq!(named, ["Ann", "Bo"]);
}

#[test]
fn the_join_line_is_drawn_between_the_messages_with_nothing_to_press() {
    let f = Fixture::new();
    let room = LiveRoom::new(f.stored(with_joins(&f, &[(0, &f.me, 90)]))).view(&minutes);
    let key = format!("join-{}", room.joins[0].key);
    let rt = render(room);
    let all = rt.elements();
    let line = all
        .iter()
        .find(|e| e.key.as_deref() == Some(key.as_str()))
        .expect("the join line is drawn");
    let inside: Vec<_> = all
        .iter()
        .filter(|e| e.path.starts_with(&format!("{}/", line.path)))
        .collect();
    assert!(
        inside.iter().any(|e| e
            .text
            .as_deref()
            .is_some_and(|t| t.starts_with("Mia joined"))),
        "it says who joined: {:?}",
        inside.iter().map(|e| &e.text).collect::<Vec<_>>()
    );
    assert!(
        inside.iter().all(|e| e.role != "button" && !e.focusable),
        "no reply, react, edit or delete on it"
    );
    // It sits after the first message's group and before the second's, with no avatar or name.
    let at = |k: &str| all.iter().position(|e| e.key.as_deref() == Some(k));
    let first = at(&format!("group-{}", "1")).or_else(|| {
        all.iter()
            .position(|e| e.key.as_deref().is_some_and(|k| k.starts_with("group-")))
    });
    let join_at = at(&key).unwrap();
    assert!(first.unwrap() < join_at);
    let groups: Vec<usize> = all
        .iter()
        .enumerate()
        .filter(|(_, e)| e.key.as_deref().is_some_and(|k| k.starts_with("group-")))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(groups.len(), 2, "the join splits nothing into three");
    assert!(groups[0] < join_at && join_at < groups[1]);
}

#[test]
fn a_join_splits_one_authors_messages_into_two_groups() {
    let f = Fixture::new();
    // Two messages by the owner a minute apart (one group alone), a join between them.
    let mut state = f.private_state();
    state.recent_messages.messages.clear();
    let text = |secs, t: &str| f.message(&f.owner, secs, RoomMessageBody::public(t.into()));
    let join = f.message(&f.me, 90, RoomMessageBody::join_event());
    state.recent_messages.messages = vec![text(60, "one"), join, text(120, "two")];
    let room = LiveRoom::new(f.stored(state)).view(&minutes);
    assert_eq!(room.messages.len(), 2);
    assert_eq!(crate::chat::model::groups(&room.messages).len(), 1);
    let rt = render(room);
    let groups = rt
        .elements()
        .iter()
        .filter(|e| e.key.as_deref().is_some_and(|k| k.starts_with("group-")))
        .count();
    assert_eq!(groups, 2, "the join line ends the first group");
}

/// Local minutes of a time, shifted by `shift` minutes from `at` seconds on (a clock change).
fn zone(at: u64, shift: i64) -> impl Fn(SystemTime) -> u32 {
    move |t| {
        let secs = t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        let m = (secs / 60) as i64 + if secs >= at { shift } else { 0 };
        m as u32
    }
}

fn joins_at(
    f: &Fixture,
    secs: &[u64],
    view: &dyn Fn(SystemTime) -> u32,
) -> Vec<crate::chat::model::Join> {
    let list: Vec<(usize, &SigningKey, u64)> = secs.iter().map(|s| (0, &f.me, *s)).collect();
    LiveRoom::new(f.stored(with_joins(f, &list)))
        .view(view)
        .joins
}

#[test]
fn the_fold_compares_instants_with_the_webs_strict_hour() {
    let f = Fixture::new();
    let h = 3600;
    // 59m59s folds; exactly an hour and a second past do not (the web's `< 1 hour`).
    assert_eq!(joins_at(&f, &[1000, 1000 + h - 1], &minutes).len(), 1);
    assert_eq!(joins_at(&f, &[1000, 1000 + h], &minutes).len(), 2);
    assert_eq!(joins_at(&f, &[1000, 1000 + h + 1], &minutes).len(), 2);
    // Each join is compared with the one before it, so a chain folds past an hour in all.
    assert_eq!(joins_at(&f, &[1000, 4000, 7000], &minutes).len(), 1);
}

#[test]
fn seconds_the_minute_clock_drops_do_not_change_the_fold() {
    let f = Fixture::new();
    // 10:00:59 and 11:00:00 are 59m01s apart but 60 whole minutes apart on the minute clock.
    assert_eq!(
        joins_at(&f, &[10 * 3600 + 59, 11 * 3600], &minutes).len(),
        1
    );
}

#[test]
fn a_clock_change_does_not_change_the_fold() {
    let f = Fixture::new();
    let change = 5000;
    // Spring forward: 30 minutes apart in fact, 90 on the local clock. Still folded.
    let spring = zone(change, 60);
    assert_eq!(
        joins_at(&f, &[change - 900, change + 900], &spring).len(),
        1
    );
    // Fall back: 90 minutes apart in fact, 30 on the local clock. Still two lines.
    let fall = zone(change, -60);
    assert_eq!(
        joins_at(&f, &[change - 2700, change + 2700], &fall).len(),
        2
    );
}

#[test]
fn the_line_is_keyed_by_its_first_event_so_a_fold_keeps_the_row() {
    let f = Fixture::new();
    let one = joins_at(&f, &[1000], &minutes);
    let two = joins_at(&f, &[1000, 1100], &minutes);
    assert_eq!(one[0].key, two[0].key);
}

#[test]
fn three_joins_fold_into_one_counted_line_and_one_member_twice_is_named_twice() {
    let f = Fixture::new();
    let three = joins_at(&f, &[1000, 1100, 1200], &minutes);
    assert_eq!(three.len(), 1);
    assert_eq!(three[0].line().0, "3 people joined");
    let twice = joins_at(&f, &[1000, 1100], &minutes);
    assert_eq!(twice[0].line().0, "Mia and Mia joined");
}

fn element_keys(rt: &rune::Runtime<crate::RiverApp>) -> Vec<String> {
    rt.elements().into_iter().filter_map(|e| e.key).collect()
}

#[test]
fn a_join_before_the_first_message_and_one_after_the_last_are_drawn_in_place() {
    let f = Fixture::new();
    let state = with_joins(&f, &[(usize::MAX, &f.me, 30), (3, &f.owner, 300)]);
    let room = LiveRoom::new(f.stored(state)).view(&minutes);
    assert_eq!(room.messages.len(), 2);
    assert_eq!(room.joins.len(), 2);
    assert_eq!((room.joins[0].before, room.joins[1].before), (0, 2));
    let (first, last) = (
        format!("join-{}", room.joins[0].key),
        format!("join-{}", room.joins[1].key),
    );
    let keys = element_keys(&render(room));
    let at = |k: &str| {
        keys.iter()
            .position(|x| x == k)
            .unwrap_or_else(|| panic!("no {k}"))
    };
    let groups: Vec<usize> = keys
        .iter()
        .enumerate()
        .filter(|(_, k)| k.starts_with("group-"))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(groups.len(), 2);
    assert!(
        at(&first) < groups[0],
        "the first join is above the first message"
    );
    assert!(
        at(&last) > groups[1],
        "the last join is below the last message"
    );
}

#[test]
fn a_join_on_another_day_gets_its_own_day_divider() {
    let f = Fixture::new();
    let day = 86_400;
    // Messages are on day 0; the join is on day 2, between the two messages.
    let state = with_joins(&f, &[(0, &f.me, 2 * day + 60)]);
    let keys = element_keys(&render(LiveRoom::new(f.stored(state)).view(&minutes)));
    let day_at = |d: u32| keys.iter().position(|k| *k == format!("day-{d}"));
    let join_at = keys.iter().position(|k| k.starts_with("join-")).unwrap();
    assert!(day_at(0).unwrap() < join_at);
    assert!(
        day_at(2).unwrap() < join_at,
        "day 2 divider is above the join"
    );
    assert!(join_at < keys.iter().rposition(|k| k.starts_with("group-")).unwrap());
}

#[test]
fn the_names_of_a_counted_line_open_on_press_and_are_in_its_label() {
    let f = Fixture::new();
    let room = LiveRoom::new(f.stored(with_joins(
        &f,
        &[(0, &f.me, 90), (0, &f.owner, 100), (0, &f.me, 110)],
    )))
    .view(&minutes);
    let key = format!("join-{}", room.joins[0].key);
    let mut rt = render(room);
    let line = rt
        .elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some(key.as_str()))
        .expect("drawn");
    assert_eq!(line.role, "button");
    assert!(line.label.as_deref().unwrap().contains("Mia, Olive, Mia"));
    assert!(!element_keys(&rt).contains(&format!("{key}-names")));
    let press = |rt: &mut rune::Runtime<crate::RiverApp>, t: f64| {
        let (x, y) = rt.find(&key).expect("drawn");
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        rt.frame(t);
        rt.frame(t + 0.1);
    };
    press(&mut rt, 1.0);
    assert!(element_keys(&rt).contains(&format!("{key}-names")));
    press(&mut rt, 2.0);
    assert!(!element_keys(&rt).contains(&format!("{key}-names")));
}

#[test]
fn a_join_is_never_unread_nor_a_message() {
    let f = Fixture::new();
    // As Mia: the owner's text after the reply is the one unread message; joins after the
    // last read add nothing.
    let mut state = with_joins(&f, &[(1, &f.owner, 130), (3, &f.owner, 300)]);
    state.recent_messages.messages.push(f.message(
        &f.owner,
        400,
        RoomMessageBody::public("later".into()),
    ));
    let mut stored = f.stored(state.clone());
    stored.last_read_message_id = Some(state.recent_messages.messages[0].id());
    let room = LiveRoom::new(stored).view(&minutes);
    assert!(!room.joins.is_empty(), "the joins are in the view");
    assert_eq!(room.messages.len(), 3);
    assert_eq!(room.unread, 1);
    // Notifications and `arrivals` read only `Room::messages`, which joins never enter
    // (asserted above), so a join cannot notify; there is nothing more to assert here.
}

#[test]
fn a_counted_line_is_reached_and_worked_from_the_keyboard_and_a_named_one_is_no_button() {
    use rune::{Key, KeyEvent};
    let f = Fixture::new();
    let room = LiveRoom::new(f.stored(with_joins(
        &f,
        &[
            (0, &f.me, 90),
            (0, &f.owner, 100),
            (0, &f.me, 110),
            (1, &f.me, 130),
        ],
    )))
    .view(&minutes);
    assert_eq!(room.joins.len(), 2);
    let counted = format!("join-{}", room.joins[0].key);
    let named = format!("join-{}", room.joins[1].key);
    let mut rt = render(room);
    let info = |rt: &rune::Runtime<crate::RiverApp>, k: &str| {
        rt.elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some(k))
            .unwrap()
    };
    assert!(info(&rt, &counted).focusable);
    let two = info(&rt, &named);
    assert!(
        two.role != "button" && !two.focusable,
        "one person's line is not a control"
    );
    let focused = |rt: &rune::Runtime<crate::RiverApp>| {
        rt.elements()
            .into_iter()
            .find(|e| e.focused)
            .and_then(|e| e.key)
    };
    let mut t = 0.0;
    let mut tab = |rt: &mut rune::Runtime<crate::RiverApp>, k: Key| {
        t += 0.3;
        rt.key(KeyEvent {
            key: k,
            mods: Default::default(),
        });
        rt.frame(t);
    };
    for _ in 0..200 {
        if focused(&rt).as_deref() == Some(counted.as_str()) {
            break;
        }
        tab(&mut rt, Key::Tab);
    }
    assert_eq!(
        focused(&rt).as_deref(),
        Some(counted.as_str()),
        "Tab reaches it"
    );
    let names = format!("{counted}-names");
    tab(&mut rt, Key::Enter);
    assert!(element_keys(&rt).contains(&names), "Enter opens the list");
    assert_eq!(
        focused(&rt).as_deref(),
        Some(counted.as_str()),
        "focus stays on the line"
    );
    tab(&mut rt, Key::Enter);
    assert!(!element_keys(&rt).contains(&names), "Enter closes it");
    assert_eq!(focused(&rt).as_deref(), Some(counted.as_str()));
}
