//! The ban, checked against the room contract's own `apply_delta` and `verify`: what the app
//! signs must be what the contract takes, and must do what the web UI's Ban button does.

use std::time::{Duration, SystemTime};

use ed25519_dalek::SigningKey;
use freenet_scaffold::ComposableState;
use river_core::ecies::{encrypt_secret_for_member, try_decrypt_secret_from_member_blob_raw};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_core::room_state::privacy::{
    PrivacyMode, RoomCipherSpec, RoomDisplayMetadata, SealedBytes,
};
use river_core::room_state::secret::{
    AuthorizedEncryptedSecretForMember, AuthorizedSecretVersionRecord, EncryptedSecretForMemberV1,
    SecretVersionRecordV1,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
use river_core::util::sign_struct;

use super::*;
use crate::chat::model::{BanOffer, BanPreview};
use crate::rooms::room::LiveRoom;
use crate::rooms::room_tests::{Fixture, SECRET};

pub(super) const TIME: u64 = 1_700_000_000;

pub(super) fn at(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

pub(super) fn k(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

pub(super) fn id(sk: &SigningKey) -> MemberId {
    MemberId::from(&sk.verifying_key())
}

/// The people: the owner (key 1), Mia (2, invited by the owner), Ann (3, by the owner), Bo (4, by
/// Ann), Cy (5, by the owner), Di (6, by Bo).
pub(in crate::rooms) struct World {
    pub f: Fixture,
    pub ann: SigningKey,
    pub bo: SigningKey,
    pub cy: SigningKey,
    pub di: SigningKey,
}

impl World {
    pub fn new() -> Self {
        World {
            f: Fixture::new(),
            ann: k(3),
            bo: k(4),
            cy: k(5),
            di: k(6),
        }
    }

    pub fn owner(&self) -> &SigningKey {
        &self.f.owner
    }

    pub fn mia(&self) -> &SigningKey {
        &self.f.me
    }

    fn admit(&self, state: &mut ChatRoomStateV1, who: &SigningKey, by: &SigningKey) {
        state.members.members.push(AuthorizedMember::new(
            Member {
                owner_member_id: self.f.owner_id(),
                invited_by: id(by),
                member_vk: who.verifying_key(),
            },
            by,
        ));
    }

    fn info(&self, who: &SigningKey, name: &str, private: bool) -> AuthorizedMemberInfo {
        AuthorizedMemberInfo::new_with_member_key(
            MemberInfo {
                member_id: id(who),
                version: 0,
                preferred_nickname: if private {
                    self.f.seal(name)
                } else {
                    SealedBytes::public(name.as_bytes().to_vec())
                },
                deputies: Vec::new(),
            },
            who,
        )
    }

    /// The room, public or private (secret version 0 for everyone including the owner).
    pub fn state(&self, private: bool) -> ChatRoomStateV1 {
        let mut state = ChatRoomStateV1::default();
        let mut config = Configuration {
            owner_member_id: self.f.owner_id(),
            ..Configuration::default()
        };
        if private {
            config.privacy_mode = PrivacyMode::Private;
            config.display = match self.f.seal("Garden") {
                SealedBytes::Private {
                    ciphertext,
                    nonce,
                    secret_version,
                    declared_len_bytes,
                } => RoomDisplayMetadata::private(
                    ciphertext,
                    nonce,
                    declared_len_bytes,
                    None,
                    secret_version,
                ),
                _ => unreachable!(),
            };
        } else {
            config.display = RoomDisplayMetadata::public("Town".into(), None);
        }
        state.configuration = AuthorizedConfigurationV1::new(config, self.owner());
        let (o, m, a, b, c, d) = (
            self.owner().clone(),
            self.mia().clone(),
            self.ann.clone(),
            self.bo.clone(),
            self.cy.clone(),
            self.di.clone(),
        );
        self.admit(&mut state, &m, &o);
        self.admit(&mut state, &a, &o);
        self.admit(&mut state, &b, &a);
        self.admit(&mut state, &c, &o);
        self.admit(&mut state, &d, &b);
        for (who, name) in [
            (&o, "Olive"),
            (&m, "Mia"),
            (&a, "Ann"),
            (&b, "Bo"),
            (&c, "Cy"),
            (&d, "Di"),
        ] {
            state
                .member_info
                .member_info
                .push(self.info(who, name, private));
        }
        // Everyone has spoken, so the room does not prune anyone for inactivity.
        for (n, who) in [&o, &m, &a, &b, &c, &d].into_iter().enumerate() {
            let body = if private {
                self.f.private_body(
                    1,
                    1,
                    &river_core::room_state::content::TextContentV1::new("hi".into()).encode(),
                )
            } else {
                river_core::room_state::message::RoomMessageBody::public("hi".into())
            };
            state
                .recent_messages
                .messages
                .push(self.f.message(who, 100 + n as u64, body));
        }
        if private {
            let record = SecretVersionRecordV1 {
                version: 0,
                cipher_spec: RoomCipherSpec::Aes256Gcm,
                created_at: at(1),
            };
            state.secrets.versions.push(AuthorizedSecretVersionRecord {
                owner_signature: sign_struct(&record, self.owner()),
                record,
            });
            for who in [&o, &m, &a, &b, &c, &d] {
                blob(&mut state, self.owner(), who, 0, &SECRET);
            }
        }
        state
    }

    /// `state` as `viewer` sees it.
    pub fn room(&self, state: ChatRoomStateV1, viewer: &SigningKey) -> LiveRoom {
        let mut stored = self.f.stored(state);
        stored.self_sk = Some(viewer.clone());
        LiveRoom::new(stored)
    }
}

pub(super) fn blob(
    state: &mut ChatRoomStateV1,
    owner: &SigningKey,
    who: &SigningKey,
    version: u32,
    secret: &[u8; 32],
) {
    let (ciphertext, nonce, ephemeral) = encrypt_secret_for_member(secret, &who.verifying_key());
    let b = EncryptedSecretForMemberV1 {
        member_id: id(who),
        secret_version: version,
        ciphertext,
        nonce,
        sender_ephemeral_public_key: ephemeral.to_bytes(),
        provider: id(owner),
    };
    state
        .secrets
        .encrypted_secrets
        .push(AuthorizedEncryptedSecretForMember {
            owner_signature: sign_struct(&b, owner),
            secret: b,
        });
}

/// The contract's own answer: `state` after `delta`, which must also verify.
pub(super) fn contract_apply(
    w: &World,
    state: &ChatRoomStateV1,
    delta: ChatRoomStateV1Delta,
) -> ChatRoomStateV1 {
    let params = ChatRoomParametersV1 {
        owner: w.owner().verifying_key(),
    };
    let mut next = state.clone();
    next.apply_delta(&ChatRoomStateV1::default(), &params, &Some(delta))
        .expect("the contract takes the delta");
    next.verify(&next, &params)
        .expect("the resulting state verifies");
    next
}

pub(super) fn member_ids(state: &ChatRoomStateV1) -> Vec<MemberId> {
    state
        .members
        .members
        .iter()
        .map(|m| m.member.id())
        .collect()
}

/// Whether `who` can read secret `version` from `state` with their own key.
pub(super) fn secret_for(
    state: &ChatRoomStateV1,
    who: &SigningKey,
    version: u32,
) -> Option<[u8; 32]> {
    state
        .secrets
        .encrypted_secrets
        .iter()
        .filter(|b| b.secret.member_id == id(who) && b.secret.secret_version == version)
        .find_map(|b| {
            try_decrypt_secret_from_member_blob_raw(
                &b.secret.ciphertext,
                &b.secret.nonce,
                &b.secret.sender_ephemeral_public_key,
                who,
            )
            .ok()
        })
}

#[test]
fn the_owner_bans_in_a_public_room_and_the_contract_removes_the_member_and_their_invitees() {
    let w = World::new();
    let state = w.state(false);
    let room = w.room(state.clone(), w.owner());
    let built = build(&room, id(&w.ann), at(TIME)).unwrap();
    assert!(
        built.secrets.is_none(),
        "a public room has nothing to rotate"
    );
    let delta = built.delta();
    assert_eq!(delta.bans.as_ref().unwrap().len(), 1);
    assert!(delta.secrets.is_none());
    let next = contract_apply(&w, &state, delta);
    // Ann, Bo (Ann's) and Di (Bo's) go; Mia and Cy stay.
    let left = member_ids(&next);
    assert!(!left.contains(&id(&w.ann)));
    assert!(!left.contains(&id(&w.bo)));
    assert!(!left.contains(&id(&w.di)));
    assert!(left.contains(&id(&w.cy)) && left.contains(&id(w.mia())));
    assert_eq!(next.bans.0.len(), 1);
    assert_eq!(next.bans.0[0].ban.banned_user, id(&w.ann));
    assert_eq!(next.bans.0[0].banned_by, id(w.owner()));
    assert_eq!(next.bans.0[0].ban.banned_at, at(TIME));
    assert!(built.held_in(&LiveRoom::new({
        let mut s = room.stored.clone();
        s.room_state = next;
        s
    })));
}

fn whole_ms(t: SystemTime) {
    let d = t.duration_since(SystemTime::UNIX_EPOCH).unwrap();
    assert_eq!(d.subsec_nanos() % 1_000_000, 0, "{d:?}");
}

/// A queued ban can carry a time with nanoseconds (an entry stored before the fix); what is
/// signed from it must be a whole millisecond (freenet/river#971).
#[test]
fn a_ban_and_its_rotation_are_signed_a_whole_millisecond() {
    let w = World::new();
    let state = w.state(true);
    let room = w.room(state, w.owner());
    let t = at(TIME) + Duration::new(0, 123_456_789);
    let built = build(&room, id(&w.ann), t).unwrap();
    whole_ms(built.ban.as_ref().unwrap().ban.banned_at);
    let versions = &built.secrets.as_ref().unwrap().new_versions;
    assert!(!versions.is_empty());
    for r in versions {
        whole_ms(r.record.created_at);
    }
    let rotated = build_rotation(&room, t).unwrap();
    let versions = &rotated.secrets.as_ref().unwrap().new_versions;
    assert!(!versions.is_empty());
    for r in versions {
        whole_ms(r.record.created_at);
    }
}

#[test]
fn a_ban_in_a_private_room_rotates_the_secret_in_the_same_update() {
    let w = World::new();
    let state = w.state(true);
    let room = w.room(state.clone(), w.owner());
    let built = build(&room, id(&w.ann), at(TIME)).unwrap();
    let delta = built.delta();
    assert!(
        delta.bans.is_some() && delta.secrets.is_some(),
        "one update"
    );
    let next = contract_apply(&w, &state, delta);
    assert_eq!(next.secrets.current_version, 1);
    // The new secret is the one the web UI derives from the owner's key.
    let new = river_core::key_derivation::derive_room_secret(
        &w.owner().to_bytes(),
        &w.owner().verifying_key(),
        1,
    );
    // Everyone who remains has it, and the earlier one too.
    for who in [w.owner(), w.mia(), &w.cy] {
        assert_eq!(secret_for(&next, who, 1), Some(new), "new secret");
        assert_eq!(secret_for(&next, who, 0), Some(SECRET), "old secret kept");
    }
    // The banned member and the members the ban removes have nothing at any version.
    for who in [&w.ann, &w.bo, &w.di] {
        assert_eq!(secret_for(&next, who, 1), None);
        assert_eq!(secret_for(&next, who, 0), None);
        assert!(
            next.secrets
                .encrypted_secrets
                .iter()
                .all(|b| b.secret.member_id != id(who))
        );
    }
    // What the banned member held before cannot give them version 1.
    assert_ne!(new, SECRET);
    // The room, read by the owner after the ban, is sealed under the new version.
    let after = LiveRoom::new({
        let mut s = room.stored.clone();
        s.room_state = next;
        s
    });
    assert_eq!(after.current_secret().map(|(_, v)| v), Some(1));
    assert!(built.held_in(&after));
    // The room's name, sealed at version 0, is left as the web UI leaves it and still reads.
    assert_eq!(after.name(), "Garden");
}

#[test]
fn a_remaining_member_reads_the_new_version_with_their_own_key() {
    let w = World::new();
    let state = w.state(true);
    let owner_room = w.room(state.clone(), w.owner());
    let built = build(&owner_room, id(&w.cy), at(TIME)).unwrap();
    let next = contract_apply(&w, &state, built.delta());
    let mia = w.room(next, w.mia());
    assert_eq!(mia.current_secret().map(|(_, v)| v), Some(1));
}

#[test]
fn an_ancestor_and_a_deputy_may_ban_in_a_public_room_and_others_may_not() {
    let w = World::new();
    let mut state = w.state(false);
    // Ann bans Bo (her invitee): a strict ancestor.
    let room = w.room(state.clone(), &w.ann);
    assert_eq!(gate(&room, id(&w.ann), id(&w.bo)), Some(BanOffer::Allowed));
    let built = build(&room, id(&w.bo), at(TIME)).unwrap();
    let next = contract_apply(&w, &state, built.delta());
    assert!(!member_ids(&next).contains(&id(&w.bo)));
    assert!(!member_ids(&next).contains(&id(&w.di)));
    assert!(member_ids(&next).contains(&id(&w.ann)));
    // Ann has no say over Cy, the owner, or Mia.
    for other in [&w.cy, w.owner(), w.mia()] {
        assert_eq!(gate(&room, id(&w.ann), id(other)), None);
    }
    assert!(build(&room, id(&w.cy), at(TIME)).is_err());
    // Cy, made a deputy of the owner, may ban anyone but the owner.
    let mut owner_info = w.info(w.owner(), "Olive", false);
    owner_info.member_info.version = 1;
    owner_info.member_info.deputies = vec![id(&w.cy)];
    let owner_info = AuthorizedMemberInfo::new(owner_info.member_info.clone(), w.owner());
    state
        .member_info
        .member_info
        .retain(|i| i.member_info.member_id != id(w.owner()));
    state.member_info.member_info.push(owner_info);
    let room = w.room(state.clone(), &w.cy);
    assert_eq!(gate(&room, id(&w.cy), id(&w.bo)), Some(BanOffer::Allowed));
    let built = build(&room, id(&w.bo), at(TIME)).unwrap();
    let next = contract_apply(&w, &state, built.delta());
    assert!(!member_ids(&next).contains(&id(&w.bo)));
    assert_eq!(next.bans.0[0].banned_by, id(&w.cy));
    assert_eq!(gate(&room, id(&w.cy), id(w.owner())), None);
}

#[test]
fn nobody_may_ban_themselves_or_whoever_they_joined_through() {
    let w = World::new();
    let room = w.room(w.state(false), &w.bo);
    // Not offered at all for yourself.
    assert_eq!(gate(&room, id(&w.bo), id(&w.bo)), None);
    // Bo has no authority over Ann (an ancestor), so nothing is offered or said.
    assert_eq!(gate(&room, id(&w.bo), id(&w.ann)), None);
    // The owner banning Ann is fine; a deputy of the owner banning Ann, who invited them, is
    // refused with the reason.
    let mut state = w.state(false);
    let mut info = w.info(w.owner(), "Olive", false).member_info;
    info.version = 1;
    info.deputies = vec![id(&w.bo)];
    state
        .member_info
        .member_info
        .push(AuthorizedMemberInfo::new(info, w.owner()));
    let room = w.room(state, &w.bo);
    match gate(&room, id(&w.bo), id(&w.ann)) {
        Some(BanOffer::Withheld(why)) => assert!(why.contains("joined the room through")),
        other => panic!("{other:?}"),
    }
    assert!(build(&room, id(&w.ann), at(TIME)).is_err());
}

#[test]
fn a_member_too_deep_in_the_invite_tree_is_told_their_ban_would_do_nothing() {
    let w = World::new();
    let mut state = w.state(false);
    // Di (depth 3) invites e (4), f (5), g (6).
    let (e, f, g) = (k(7), k(8), k(9));
    w.admit(&mut state, &e, &w.di);
    w.admit(&mut state, &f, &e);
    w.admit(&mut state, &g, &f);
    let room = w.room(state, &f);
    assert_eq!(
        gate(&room, id(&f), id(&g)),
        Some(BanOffer::Withheld(ISSUER_TOO_DEEP.into()))
    );
    assert_eq!(
        build(&room, id(&g), at(TIME)).err().as_deref(),
        Some(ISSUER_TOO_DEEP)
    );
}

#[test]
fn in_a_private_room_only_the_owner_can_ban_and_the_others_are_told_why() {
    let w = World::new();
    let room = w.room(w.state(true), &w.ann);
    assert_eq!(
        gate(&room, id(&w.ann), id(&w.bo)),
        Some(BanOffer::Withheld(PRIVATE_OWNER_ONLY.into()))
    );
    assert_eq!(
        build(&room, id(&w.bo), at(TIME)).err().as_deref(),
        Some(PRIVATE_OWNER_ONLY)
    );
    // The owner is offered it.
    let room = w.room(w.state(true), w.owner());
    assert_eq!(
        gate(&room, id(w.owner()), id(&w.bo)),
        Some(BanOffer::Allowed)
    );
}

/// A public room that allows `max` bans.
fn capped(w: &World, max: usize) -> ChatRoomStateV1 {
    let mut state = w.state(false);
    let mut config = state.configuration.configuration.clone();
    config.max_user_bans = max;
    config.configuration_version += 1;
    state.configuration = AuthorizedConfigurationV1::new(config, w.owner());
    state
}

#[test]
fn at_the_ban_cap_the_preview_says_what_the_contract_then_does() {
    let w = World::new();
    let state = capped(&w, 1);
    let first = build(&w.room(state.clone(), w.owner()), id(&w.cy), at(TIME)).unwrap();
    assert!(
        first.displaced.is_empty(),
        "room under its cap lifts nothing"
    );
    let state = contract_apply(&w, &state, first.delta());
    assert_eq!(state.bans.0.len(), 1);
    let room = w.room(state.clone(), w.owner());
    // Cy was pruned with the ban, so the room has no nickname for them: the stand-in is named.
    let cy = room.nickname(id(&w.cy), None);
    assert!(cy.starts_with("Member "), "{cy}");
    for target in [&w.ann, w.mia()] {
        assert_eq!(
            preview(&room, id(target)),
            BanPreview {
                lifts: vec![cy.clone()],
                refused: false
            }
        );
    }
    // The ban is still allowed, built with the same names the preview showed, and the contract
    // does exactly what the preview said.
    let second = build(&room, id(&w.ann), at(TIME + 10)).unwrap();
    assert_eq!(second.displaced, vec![cy.clone()]);
    let next = contract_apply(&w, &state, second.delta());
    assert_eq!(next.bans.0.len(), 1);
    assert_eq!(
        next.bans.0[0].ban.banned_user,
        id(&w.ann),
        "Cy's ban is the one gone"
    );
    assert_eq!(
        lifted_notice(std::slice::from_ref(&cy), 1),
        format!("The room keeps at most 1 bans, so the ban on {cy} was lifted.")
    );
    // Under the cap nothing is lifted, so nothing is said.
    let roomy = w.room(capped(&w, 10), w.owner());
    assert_eq!(preview(&roomy, id(&w.ann)), BanPreview::default());
}

#[test]
fn at_the_cap_a_ban_the_room_would_drop_is_said_and_refused_not_published_as_done() {
    let w = World::new();
    let state = capped(&w, 1);
    let owner_first = build(&w.room(state.clone(), w.owner()), id(&w.cy), at(TIME)).unwrap();
    let state = contract_apply(&w, &state, owner_first.delta());
    // Ann (an ancestor of Bo) is outranked by the owner's ban: the room keeps the owner's and
    // drops hers. The preview says the ban itself would not be kept, and building it refuses.
    let room = w.room(state, &w.ann);
    assert_eq!(
        preview(&room, id(&w.bo)),
        BanPreview {
            lifts: Vec::new(),
            refused: true
        }
    );
    assert_eq!(
        build(&room, id(&w.bo), at(TIME + 10)).err().as_deref(),
        Some(NOT_KEPT)
    );
}

#[test]
fn a_member_with_another_ban_kept_is_not_called_lifted() {
    let w = World::new();
    let ban = |who: &SigningKey, secs: u64| {
        AuthorizedUserBan::new(
            UserBan {
                owner_member_id: w.f.owner_id(),
                banned_at: at(secs),
                banned_user: id(who),
            },
            id(w.owner()),
            w.owner(),
        )
    };
    let (c0, c1, d0, new) = (ban(&w.cy, 1), ban(&w.cy, 2), ban(&w.di, 3), ban(&w.ann, 4));
    // Cy has two bans; only one goes: Cy is still banned. Di's only ban goes: Di is lifted,
    // once, however many of their bans go.
    assert!(
        lost_targets(
            &[c0.clone(), c1.clone(), d0.clone()],
            &[c1.clone(), new.clone()]
        )
        .len()
            == 1
    );
    assert_eq!(
        lost_targets(&[c0.clone(), c1.clone(), d0.clone()], &[c1, new.clone()]),
        vec![id(&w.di)]
    );
    assert_eq!(
        lost_targets(&[c0.clone(), c0.clone()], &[new]),
        vec![id(&w.cy)],
        "listed once"
    );
}

#[test]
fn only_a_ban_the_room_enforces_on_a_member_who_is_gone_counts() {
    let w = World::new();
    let state = w.state(false);
    let owner = w.room(state.clone(), w.owner());
    // Enforced: the owner's ban, taken by the contract.
    let enforced = contract_apply(
        &w,
        &state,
        build(&owner, id(&w.cy), at(TIME)).unwrap().delta(),
    );
    assert!(banned_and_gone(&w.room(enforced, w.owner()), id(&w.cy)));
    // Only pruned: no ban, so nothing is in force.
    let mut pruned = state.clone();
    pruned
        .members
        .members
        .retain(|m| m.member.id() != id(&w.cy));
    assert!(!banned_and_gone(
        &w.room(pruned.clone(), w.owner()),
        id(&w.cy)
    ));
    // A stored ban nothing authorises (Ann is not above Cy) is inert: not in force.
    let inert = AuthorizedUserBan::new(
        UserBan {
            owner_member_id: w.f.owner_id(),
            banned_at: at(TIME),
            banned_user: id(&w.cy),
        },
        id(&w.ann),
        &w.ann,
    );
    pruned.bans.0.push(inert);
    assert!(!banned_and_gone(&w.room(pruned, w.owner()), id(&w.cy)));
}

#[test]
fn a_ban_the_node_took_but_the_room_does_not_show_stays_unconfirmed_and_says_why() {
    let w = World::new();
    let state = capped(&w, 1);
    // Ann bans Bo (room empty of bans: kept when built).
    let room = w.room(state.clone(), &w.ann);
    let mut o = outbox();
    o.command(&room, ban_of(&w.bo));
    next(&mut o, &room).expect("goes out");
    // Before the answer, the owner's ban of Cy fills the room's one slot and outranks Ann's.
    let owner_ban = build(&w.room(state.clone(), w.owner()), id(&w.cy), at(TIME + 50)).unwrap();
    let mut live = w.room(contract_apply(&w, &state, owner_ban.delta()), &w.ann);
    assert!(o.answered(&mut live, Ok(())));
    let view = o.view(&live, &minutes);
    assert_eq!(view.notices.len(), 1);
    assert!(
        view.notices[0]
            .text
            .starts_with("Your ban of Bo: Freenet took it, but the room here doesn't show it."),
        "{}",
        view.notices[0].text
    );
    assert!(view.notices[0].text.contains(NOT_KEPT));
    assert!(
        next(&mut o, &live).is_none(),
        "an unconfirmed ban is not sent again"
    );
}

#[test]
fn a_settled_ban_that_lifted_another_says_so_in_a_notice() {
    let w = World::new();
    let state = capped(&w, 1);
    let first = build(&w.room(state.clone(), w.owner()), id(&w.cy), at(TIME)).unwrap();
    let state = contract_apply(&w, &state, first.delta());
    let mut room = w.room(state, w.owner());
    let mut o = outbox();
    o.command(&room, ban_of(&w.ann));
    next(&mut o, &room).unwrap();
    assert!(o.answered(&mut room, Ok(())));
    let view = o.view(&room, &minutes);
    assert_eq!(view.notices.len(), 1);
    let cy = room.nickname(id(&w.cy), None);
    assert_eq!(
        view.notices[0].text,
        format!("Your ban: The room keeps at most 1 bans, so the ban on {cy} was lifted.")
    );
    // Discardable.
    let entry = view.notices[0].entry;
    assert!(o.command(&room, Action::Discard { entry }));
    assert!(o.is_empty());
}

#[test]
fn an_equivalent_ban_from_the_owners_other_device_settles_this_one() {
    let w = World::new();
    let state = w.state(false);
    let room = w.room(state.clone(), w.owner());
    let mut o = outbox();
    o.command(&room, ban_of(&w.cy));
    next(&mut o, &room).unwrap();
    // The other device's ban (a different signature, same member) arrives before the answer.
    let other = build(&room, id(&w.cy), at(TIME + 500)).unwrap();
    let mut live = w.room(contract_apply(&w, &state, other.delta()), w.owner());
    assert!(o.reconcile(&mut live), "settled by the network");
    assert!(o.is_empty(), "no false 'not kept', no Retry that fails");
}

#[test]
fn a_waiting_ban_the_other_device_already_made_is_dropped_and_a_gone_member_is_said() {
    let w = World::new();
    let state = w.state(false);
    let room = w.room(state.clone(), w.owner());
    // Banned elsewhere: done, silently, nothing sent.
    let mut o = outbox();
    o.command(&room, ban_of(&w.cy));
    let other = build(&room, id(&w.cy), at(TIME + 500)).unwrap();
    let gone = w.room(contract_apply(&w, &state, other.delta()), w.owner());
    assert!(next(&mut o, &gone).is_none());
    assert!(o.is_empty());
    // Gone with no ban (pruned): said, nothing sent.
    let mut o = outbox();
    o.command(&room, ban_of(&w.cy));
    let mut pruned_state = state;
    pruned_state
        .members
        .members
        .retain(|m| m.member.id() != id(&w.cy));
    let pruned = w.room(pruned_state, w.owner());
    assert!(next(&mut o, &pruned).is_none());
    let view = o.view(&pruned, &minutes);
    assert_eq!(view.notices.len(), 1);
    assert!(view.notices[0].text.contains("no longer in the room"));
}

#[test]
fn the_next_version_never_wraps() {
    let w = World::new();
    let mut state = w.state(true);
    // A room already at the highest version.
    let record = SecretVersionRecordV1 {
        version: u32::MAX,
        cipher_spec: RoomCipherSpec::Aes256Gcm,
        created_at: at(5),
    };
    state.secrets.versions.push(AuthorizedSecretVersionRecord {
        owner_signature: sign_struct(&record, w.owner()),
        record,
    });
    state.secrets.current_version = u32::MAX;
    let owner = w.owner().clone();
    blob(&mut state, &owner, &owner, u32::MAX, &SECRET);
    let room = w.room(state, w.owner());
    assert_eq!(room.current_secret().map(|(_, v)| v), Some(u32::MAX));
    assert_eq!(
        build(&room, id(&w.ann), at(TIME)).err().as_deref(),
        Some(NO_SECRET_VERSION_LEFT)
    );
}

#[test]
fn a_member_who_is_not_listed_cannot_be_banned_from_here() {
    let w = World::new();
    let room = w.room(w.state(false), w.owner());
    assert_eq!(
        build(&room, id(&k(77)), at(TIME)).err().as_deref(),
        Some(NOT_A_MEMBER)
    );
    // Nor the owner.
    assert!(build(&room, id(w.owner()), at(TIME)).is_err());
}

#[test]
fn offers_cover_the_members_the_viewer_may_ban() {
    let w = World::new();
    let room = w.room(w.state(false), w.owner());
    let o = offers(&room);
    assert_eq!(o.len(), 5);
    assert!(o.values().all(|v| *v == BanOffer::Allowed));
    let room = w.room(w.state(false), &w.ann);
    let o = offers(&room);
    // Ann may ban Bo and Di only.
    let keys: std::collections::BTreeSet<_> = o.keys().cloned().collect();
    assert_eq!(
        keys,
        [id(&w.bo), id(&w.di)]
            .iter()
            .map(|i| i.to_string())
            .collect()
    );
}

// ---- through the outbox ----

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::rooms::outbox::{Action, Outbox};

fn outbox() -> Outbox {
    let ticks = Arc::new(AtomicU64::new(0));
    Outbox::with_clock(Box::new(move || {
        at(TIME + ticks.fetch_add(1, Ordering::Relaxed))
    }))
}

fn minutes(t: SystemTime) -> u32 {
    (t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() / 60) as u32
}

fn ban_of(who: &SigningKey) -> Action {
    Action::Ban {
        target: id(who).to_string(),
    }
}

fn next(o: &mut Outbox, room: &LiveRoom) -> Option<ChatRoomStateV1Delta> {
    let mut changed = false;
    o.next_update(room, Instant::now(), &mut changed)
}

fn names(room: &LiveRoom, o: &Outbox) -> Vec<String> {
    o.view(room, &minutes)
        .members
        .iter()
        .map(|m| m.nickname.clone())
        .collect()
}

#[test]
fn a_ban_shows_at_once_goes_out_as_one_update_and_is_done_when_the_node_takes_it() {
    let w = World::new();
    let state = w.state(true);
    let mut room = w.room(state.clone(), w.owner());
    let mut o = outbox();
    assert!(o.command(&room, ban_of(&w.ann)));
    // Shown as done: Ann and the members her invitations carried are gone from the list.
    let view = o.view(&room, &minutes);
    let shown = names(&room, &o);
    assert!(
        !shown.iter().any(|n| n == "Ann" || n == "Bo" || n == "Di"),
        "{shown:?}"
    );
    assert!(shown.iter().any(|n| n == "Cy"));
    assert!(view.notices.is_empty());
    assert!(!view.ban_offers.contains_key(&id(&w.ann).to_string()));
    // One update, which the contract takes and which locks Ann out.
    let delta = next(&mut o, &room).expect("goes out");
    assert!(delta.bans.is_some() && delta.secrets.is_some());
    let expected = contract_apply(&w, &state, delta);
    assert_eq!(expected.secrets.current_version, 1);
    assert!(next(&mut o, &room).is_none(), "one at a time");
    assert!(o.answered(&mut room, Ok(())));
    assert!(o.is_empty(), "taken and shown: done");
    assert_eq!(room.stored.room_state.bans, expected.bans);
    assert_eq!(room.stored.room_state.secrets, expected.secrets);
    assert_eq!(names(&room, &o).len(), 3, "Olive, Mia, Cy");
}

#[test]
fn a_refused_ban_puts_the_members_back_says_whom_and_retry_signs_it_again_later() {
    let w = World::new();
    let room = w.room(w.state(false), w.owner());
    let mut o = outbox();
    let mut live = room.clone();
    o.command(&room, ban_of(&w.ann));
    let first = next(&mut o, &room).unwrap();
    o.answered(&mut live, Err("no".into()));
    let view = o.view(&room, &minutes);
    assert!(
        names(&room, &o).iter().any(|n| n == "Ann"),
        "back in the list"
    );
    assert_eq!(view.notices.len(), 1);
    assert_eq!(
        view.notices[0].text,
        "Your ban of Ann couldn't be sent. Freenet refused it: no"
    );
    assert!(view.notices[0].retry);
    let entry = view.notices[0].entry;
    // Retry: a new ban, stamped now, which is a different signature from the refused one.
    assert!(o.command(&room, Action::Retry { entry }));
    let second = next(&mut o, &room).unwrap();
    assert_ne!(
        first.bans.as_ref().unwrap()[0].signature,
        second.bans.as_ref().unwrap()[0].signature
    );
    assert!(
        second.bans.as_ref().unwrap()[0].ban.banned_at
            > first.bans.as_ref().unwrap()[0].ban.banned_at
    );
    // Refused again, then discarded.
    o.answered(&mut live, Err("still no".into()));
    let entry = o.view(&room, &minutes).notices[0].entry;
    assert!(o.command(&room, Action::Discard { entry }));
    assert!(o.is_empty());
}

#[test]
fn banning_someone_already_being_banned_is_refused_and_cannot_be_retried() {
    let w = World::new();
    let room = w.room(w.state(false), w.owner());
    let mut o = outbox();
    o.command(&room, ban_of(&w.ann));
    o.command(&room, ban_of(&w.bo)); // Bo goes with Ann
    let view = o.view(&room, &minutes);
    assert_eq!(view.notices.len(), 1);
    assert!(
        view.notices[0].text.contains("already being banned"),
        "{}",
        view.notices[0].text
    );
    assert!(!view.notices[0].retry);
    let entry = view.notices[0].entry;
    assert!(
        !o.command(&room, Action::Retry { entry }),
        "a refused entry is not revived"
    );
    // The first ban is untouched and still goes out.
    assert!(next(&mut o, &room).unwrap().bans.is_some());
}

#[test]
fn a_new_ban_of_the_same_member_replaces_an_older_refused_one() {
    let w = World::new();
    let room = w.room(w.state(false), w.owner());
    let mut live = room.clone();
    let mut o = outbox();
    o.command(&room, ban_of(&w.cy));
    next(&mut o, &room).unwrap();
    o.answered(&mut live, Err("no".into()));
    assert_eq!(o.view(&room, &minutes).notices.len(), 1);
    o.command(&room, ban_of(&w.cy));
    let view = o.view(&room, &minutes);
    assert!(view.notices.is_empty(), "the refused one is replaced");
    assert!(!names(&room, &o).iter().any(|n| n == "Cy"));
}

#[test]
fn a_ban_the_app_cannot_finish_is_refused_before_it_is_queued() {
    let w = World::new();
    let room = w.room(w.state(true), &w.ann);
    let mut o = outbox();
    o.command(&room, ban_of(&w.bo));
    let view = o.view(&room, &minutes);
    assert_eq!(view.notices.len(), 1);
    assert!(
        view.notices[0].text.contains("only the owner's computer"),
        "{}",
        view.notices[0].text
    );
    assert!(next(&mut o, &room).is_none(), "nothing is published");
    assert!(!view.notices[0].retry);
}

#[test]
fn a_ban_waiting_in_a_room_that_turns_private_goes_out_with_the_rotation() {
    let w = World::new();
    let public = w.room(w.state(false), w.owner());
    let mut o = outbox();
    o.command(&public, ban_of(&w.cy));
    let private = w.room(w.state(true), w.owner());
    let delta = next(&mut o, &private).expect("goes out");
    assert!(
        delta.secrets.is_some(),
        "a private room's ban rotates the secret"
    );
    let next_state = contract_apply(&w, &private.stored.room_state, delta);
    assert_eq!(next_state.secrets.current_version, 1);
    assert_eq!(secret_for(&next_state, &w.cy, 1), None);
}

#[test]
fn a_ban_waiting_in_a_room_that_turns_public_goes_out_without_one() {
    let w = World::new();
    let private = w.room(w.state(true), w.owner());
    let mut o = outbox();
    o.command(&private, ban_of(&w.cy));
    let public = w.room(w.state(false), w.owner());
    let delta = next(&mut o, &public).expect("goes out");
    assert!(delta.secrets.is_none());
}

#[test]
fn a_waiting_ban_is_built_again_on_the_room_as_it_is_when_it_goes_out() {
    let w = World::new();
    let state = w.state(true);
    let at_queue = w.room(state.clone(), w.owner());
    let mut o = outbox();
    o.command(&at_queue, ban_of(&w.ann));
    // Meanwhile the owner's other device banned Cy and rotated to version 1.
    let other = build(&at_queue, id(&w.cy), at(TIME + 5)).unwrap();
    let moved = contract_apply(&w, &state, other.delta());
    let at_send = w.room(moved.clone(), w.owner());
    let delta = next(&mut o, &at_send).expect("goes out");
    let secrets = delta.secrets.clone().unwrap();
    assert_eq!(
        secrets.current_version,
        Some(2),
        "above what the room holds"
    );
    let end = contract_apply(&w, &moved, delta);
    assert_eq!(end.secrets.current_version, 2);
    // Mia has version 2; neither Ann nor Cy do.
    assert!(secret_for(&end, w.mia(), 2).is_some());
    assert_eq!(secret_for(&end, &w.cy, 2), None);
    assert_eq!(secret_for(&end, &w.ann, 2), None);
}

#[test]
fn two_bans_in_a_row_in_a_private_room_rotate_twice_and_exclude_both() {
    let w = World::new();
    let state = w.state(true);
    let mut room = w.room(state, w.owner());
    let mut o = outbox();
    o.command(&room, ban_of(&w.cy));
    o.command(&room, ban_of(&w.ann));
    let first = next(&mut o, &room).unwrap();
    assert_eq!(first.secrets.as_ref().unwrap().current_version, Some(1));
    o.answered(&mut room, Ok(()));
    let second = next(&mut o, &room).unwrap();
    assert_eq!(second.secrets.as_ref().unwrap().current_version, Some(2));
    let end = contract_apply(&w, &room.stored.room_state, second);
    assert_eq!(end.secrets.current_version, 2);
    assert!(secret_for(&end, w.mia(), 2).is_some());
    for gone in [&w.cy, &w.ann, &w.bo, &w.di] {
        assert_eq!(secret_for(&end, gone, 2), None);
    }
}

#[test]
fn ban_offers_in_the_view_are_the_rooms_not_the_optimistic_ones() {
    let w = World::new();
    let room = w.room(w.state(false), w.owner());
    let mut o = outbox();
    assert_eq!(o.view(&room, &minutes).ban_offers.len(), 5);
    o.command(&room, ban_of(&w.ann));
    let view = o.view(&room, &minutes);
    // Ann, Bo and Di are no longer listed, so no longer offered.
    assert_eq!(view.ban_offers.len(), 2);
}

// ---- the owner's Rotate ----

#[test]
fn rotate_is_the_next_secret_version_for_everyone_and_only_the_owners_in_a_private_room() {
    let w = World::new();
    let state = w.state(true);
    let owner = w.room(state.clone(), w.owner());
    let built = build_rotation(&owner, at(TIME)).unwrap();
    let delta = built.delta();
    assert!(delta.bans.is_none() && delta.secrets.is_some());
    let next = contract_apply(&w, &state, delta);
    assert_eq!(next.secrets.current_version, 1);
    let new = river_core::key_derivation::derive_room_secret(
        &w.owner().to_bytes(),
        &w.owner().verifying_key(),
        1,
    );
    // Every member and the owner gets it; nobody is removed.
    for who in [w.owner(), w.mia(), &w.ann, &w.bo, &w.cy, &w.di] {
        assert_eq!(secret_for(&next, who, 1), Some(new));
        assert_eq!(secret_for(&next, who, 0), Some(SECRET));
    }
    assert_eq!(member_ids(&next).len(), member_ids(&state).len());
    assert!(built.held_in(&w.room(next, w.owner())));
    // Not a member's, not a public room's.
    let member = w.room(state, &w.ann);
    assert_eq!(
        build_rotation(&member, at(TIME)).err().as_deref(),
        Some(NOT_OWNER)
    );
    let public = w.room(w.state(false), w.owner());
    assert_eq!(
        build_rotation(&public, at(TIME)).err().as_deref(),
        Some(NOT_PRIVATE)
    );
}

#[test]
fn rotate_does_not_give_an_enforced_ban_the_new_secret() {
    let w = World::new();
    let state = w.state(true);
    let owner = w.room(state.clone(), w.owner());
    // Cy was banned by the owner earlier (and pruned), then the owner rotates by hand.
    let ban = build(&owner, id(&w.cy), at(TIME)).unwrap();
    let banned_state = contract_apply(&w, &state, ban.delta());
    let owner = w.room(banned_state.clone(), w.owner());
    let rot = build_rotation(&owner, at(TIME + 5)).unwrap();
    let next = contract_apply(&w, &banned_state, rot.delta());
    assert_eq!(next.secrets.current_version, 2);
    assert_eq!(secret_for(&next, &w.cy, 2), None);
    assert!(secret_for(&next, &w.ann, 2).is_some());
}

#[test]
fn rotate_through_the_outbox_shows_its_state_and_is_built_at_send_time() {
    let w = World::new();
    let state = w.state(true);
    let mut room = w.room(state.clone(), w.owner());
    let mut o = outbox();
    assert!(o.command(&room, Action::RotateSecret));
    // On its way: shown as the next version, with a pending mark.
    let view = o.view(&room, &minutes);
    assert_eq!(view.details.secret_version, 1);
    assert!(view.details.rotation.as_ref().unwrap().failed.is_none());
    // The room moves on to version 1 (another device rotated) before it goes out: it goes out
    // as version 2, never as a second version 1.
    let other = build_rotation(&room, at(TIME + 5)).unwrap();
    let moved = contract_apply(&w, &state, other.delta());
    let at_send = w.room(moved, w.owner());
    let delta = next(&mut o, &at_send).unwrap();
    assert_eq!(delta.secrets.as_ref().unwrap().current_version, Some(2));
    // Taken: done.
    let mut live = at_send.clone();
    assert!(o.answered(&mut live, Ok(())));
    assert!(o.is_empty());
    assert_eq!(live.stored.room_state.secrets.current_version, 2);
    room = live;
    assert_eq!(o.view(&room, &minutes).details.rotation, None);
}

#[test]
fn a_refused_rotation_says_so_and_retry_builds_it_again() {
    let w = World::new();
    let room = w.room(w.state(true), w.owner());
    let mut live = room.clone();
    let mut o = outbox();
    o.command(&room, Action::RotateSecret);
    next(&mut o, &room).unwrap();
    o.answered(&mut live, Err("no".into()));
    let view = o.view(&room, &minutes);
    let p = view.details.rotation.clone().unwrap();
    assert_eq!(
        p.failed.as_deref(),
        Some("Not saved. Freenet refused it: no")
    );
    assert!(p.retry);
    assert_eq!(view.details.secret_version, 0, "not shown as done");
    assert!(o.command(&room, Action::Retry { entry: p.entry }));
    assert!(next(&mut o, &room).unwrap().secrets.is_some());
    // A second Rotate while one waits replaces it.
    let mut o = outbox();
    o.command(&room, Action::RotateSecret);
    o.command(&room, Action::RotateSecret);
    let delta = next(&mut o, &room).unwrap();
    assert_eq!(delta.secrets.unwrap().current_version, Some(1));
    assert!(next(&mut o, &room).is_none());
}

#[test]
fn rotating_where_the_app_cannot_is_refused_before_it_is_queued() {
    let w = World::new();
    for (room, why) in [
        (w.room(w.state(true), &w.ann), NOT_OWNER),
        (w.room(w.state(false), w.owner()), NOT_PRIVATE),
    ] {
        let mut o = outbox();
        o.command(&room, Action::RotateSecret);
        let view = o.view(&room, &minutes);
        assert_eq!(view.notices.len(), 1);
        assert!(
            view.notices[0].text.contains(why),
            "{}",
            view.notices[0].text
        );
        assert!(next(&mut o, &room).is_none());
    }
}

#[test]
fn the_view_offers_rotate_only_to_the_owner_of_a_private_room() {
    use crate::chat::model::RotateOffer;
    let w = World::new();
    let d = |room: &LiveRoom| room.view(&minutes).details.rotate;
    assert_eq!(d(&w.room(w.state(true), w.owner())), RotateOffer::Allowed);
    assert_eq!(d(&w.room(w.state(true), &w.ann)), RotateOffer::No);
    assert_eq!(d(&w.room(w.state(false), w.owner())), RotateOffer::No);
    let mut keyless = w.room(w.state(true), w.owner());
    keyless.stored.self_sk = None;
    keyless.stored.self_vk = Some(w.owner().verifying_key());
    assert!(matches!(d(&keyless), RotateOffer::Blocked(_)));
}

#[test]
fn leaving_a_room_drops_its_waiting_ban_and_rotation_and_forgets_their_answers() {
    let w = World::new();
    let room = w.room(w.state(true), w.owner());
    let mut o = outbox();
    o.command(&room, ban_of(&w.cy));
    o.command(&room, Action::RotateSecret);
    // One goes out and owes an answer; the other waits.
    next(&mut o, &room).expect("goes out");
    assert!(!o.is_empty());
    let contract = *room.stored.contract_key.id();
    o.purge_room(&room.stored.owner_vk, contract);
    assert!(o.is_empty(), "nothing of the room stays");
    assert!(next(&mut o, &room).is_none(), "nothing more is sent to it");
    // The owed answer is the left room's, taken and forgotten, never given to a later entry.
    assert!(o.take_stale(&contract));
    assert!(!o.take_stale(&contract));
}

#[test]
fn a_waiting_private_ban_that_a_deputy_already_made_still_gets_its_rotation() {
    let w = World::new();
    let state = w.state(true);
    let owner = w.room(state.clone(), w.owner());
    let mut o = outbox();
    o.command(&owner, ban_of(&w.cy));
    // While the owner's computer was offline, a deputy banned Cy through the web UI, which
    // leaves the rotation to the owner's delegate: the ban is in the room, the secret is not.
    let deputy_ban = AuthorizedUserBan::new(
        UserBan {
            owner_member_id: w.f.owner_id(),
            banned_at: at(TIME + 5),
            banned_user: id(&w.cy),
        },
        id(w.owner()),
        w.owner(),
    );
    let moved = contract_apply(
        &w,
        &state,
        ChatRoomStateV1Delta {
            bans: Some(vec![deputy_ban]),
            ..Default::default()
        },
    );
    assert_eq!(moved.secrets.current_version, 0);
    assert!(!member_ids(&moved).contains(&id(&w.cy)));
    let at_send = w.room(moved.clone(), w.owner());
    // The ban is not sent again, but the rotation is: Cy must not read what follows.
    let delta = next(&mut o, &at_send).expect("the rotation goes out");
    assert!(delta.bans.is_none());
    assert_eq!(delta.secrets.as_ref().unwrap().current_version, Some(1));
    let end = contract_apply(&w, &moved, delta);
    assert_eq!(end.secrets.current_version, 1);
    assert_eq!(secret_for(&end, &w.cy, 1), None);
    assert!(secret_for(&end, &w.ann, 1).is_some());
}

/// A deputy's (or the owner's other device's) ban of `who`, with no rotation, as the web UI
/// publishes one from a non-owner.
fn ban_only(w: &World, state: &ChatRoomStateV1, who: &SigningKey, secs: u64) -> ChatRoomStateV1 {
    let ban = AuthorizedUserBan::new(
        UserBan {
            owner_member_id: w.f.owner_id(),
            banned_at: at(secs),
            banned_user: id(who),
        },
        id(w.owner()),
        w.owner(),
    );
    contract_apply(
        w,
        state,
        ChatRoomStateV1Delta {
            bans: Some(vec![ban]),
            ..Default::default()
        },
    )
}

#[test]
fn a_rotation_that_landed_while_the_member_was_still_in_does_not_count_as_covering_the_ban() {
    let w = World::new();
    let state = w.state(true);
    let owner = w.room(state.clone(), w.owner());
    let mut o = outbox();
    // The owner's computer queues a ban of Cy, built to move the secret to version 1.
    o.command(&owner, ban_of(&w.cy));
    // An unrelated rotation to version 1 lands while Cy is still a member: Cy gets a blob.
    let unrelated = build_rotation(&owner, at(TIME + 5)).unwrap();
    let v1 = contract_apply(&w, &state, unrelated.delta());
    assert!(secret_for(&v1, &w.cy, 1).is_some(), "Cy holds version 1");
    // Another device then bans Cy with no rotation: version 1 is current, Cy is gone.
    let moved = ban_only(&w, &v1, &w.cy, TIME + 9);
    assert_eq!(moved.secrets.current_version, 1);
    assert!(!member_ids(&moved).contains(&id(&w.cy)));
    let at_send = w.room(moved.clone(), w.owner());
    // Version 1 must not be taken as covering the ban: Cy read it. A rotation to 2 goes out.
    let delta = next(&mut o, &at_send).expect("a rotation goes out");
    assert!(delta.bans.is_none());
    assert_eq!(delta.secrets.as_ref().unwrap().current_version, Some(2));
    let end = contract_apply(&w, &moved, delta);
    assert_eq!(secret_for(&end, &w.cy, 2), None);
    assert!(secret_for(&end, &w.ann, 2).is_some());
}

#[test]
fn two_waiting_bans_of_members_already_banned_make_one_rotation() {
    let w = World::new();
    let state = w.state(true);
    let owner = w.room(state.clone(), w.owner());
    let mut o = outbox();
    o.command(&owner, ban_of(&w.cy));
    o.command(&owner, ban_of(&w.ann));
    let moved = ban_only(&w, &ban_only(&w, &state, &w.cy, TIME + 8), &w.ann, TIME + 9);
    let at_send = w.room(moved.clone(), w.owner());
    let delta = next(&mut o, &at_send).expect("one rotation goes out");
    assert_eq!(delta.secrets.as_ref().unwrap().current_version, Some(1));
    // Already out: nothing else is waiting, and no second version is burnt.
    let mut live = at_send.clone();
    assert!(o.answered(&mut live, Ok(())));
    assert!(next(&mut o, &live).is_none());
    assert_eq!(live.stored.room_state.secrets.current_version, 1);
}

#[test]
fn a_rotation_that_stands_in_for_a_ban_says_so_when_it_goes_through_and_when_it_fails() {
    let w = World::new();
    let state = w.state(true);
    let owner = w.room(state.clone(), w.owner());
    let moved = ban_only(&w, &state, &w.cy, TIME + 8);
    let at_send = w.room(moved, w.owner());
    // Taken: a notice says the member was already banned and the secret is rotated.
    let mut o = outbox();
    o.command(&owner, ban_of(&w.cy));
    next(&mut o, &at_send).unwrap();
    let mut live = at_send.clone();
    o.answered(&mut live, Ok(()));
    let notices = o.view(&live, &minutes).notices;
    assert_eq!(notices.len(), 1);
    assert!(
        notices[0].text.contains("was already banned"),
        "{}",
        notices[0].text
    );
    assert!(notices[0].text.contains("cannot read new messages"));
    // Refused: the failure still says the banned member can read, so discarding it hides nothing.
    let mut o = outbox();
    o.command(&owner, ban_of(&w.cy));
    next(&mut o, &at_send).unwrap();
    let mut live = at_send.clone();
    o.answered(&mut live, Err("no".into()));
    let notices = o.view(&live, &minutes).notices;
    assert_eq!(notices.len(), 1);
    assert!(
        notices[0].text.contains("after the ban of"),
        "{}",
        notices[0].text
    );
    assert!(
        notices[0]
            .text
            .contains("is banned but can still read new messages")
    );
    assert!(notices[0].retry);
}

#[test]
fn a_non_owners_ban_in_a_room_that_turned_private_says_the_owner_must_rotate() {
    let w = World::new();
    // Ann (above Bo) queues a ban of Bo in a public room.
    let public = w.room(w.state(false), &w.ann);
    let mut o = outbox();
    o.command(&public, ban_of(&w.bo));
    // The room turns private and Bo was banned elsewhere meanwhile.
    let private = w.state(true);
    let moved = ban_only(&w, &private, &w.bo, TIME + 8);
    let at_send = w.room(moved, &w.ann);
    assert!(next(&mut o, &at_send).is_none(), "nothing is sent");
    let notices = o.view(&at_send, &minutes).notices;
    assert_eq!(notices.len(), 1);
    assert!(
        notices[0]
            .text
            .contains("is banned. This room is now private, so only the owner can change its secret; ask the owner to rotate it."),
        "{}",
        notices[0].text
    );
    assert!(!notices[0].retry);
    let entry = notices[0].entry;
    assert!(o.command(&at_send, Action::Discard { entry }));
    assert!(o.is_empty());
}

/// What `build` says banning `target` would lift, as names, or that the room would not keep it.
fn built_lifts(room: &LiveRoom, target: MemberId) -> Result<Vec<String>, String> {
    build(room, target, at(TIME + 100)).map(|b| b.displaced)
}

#[test]
fn the_preview_is_what_building_the_ban_does_even_with_an_inert_ban_or_one_on_its_way() {
    let w = World::new();
    // An old ban the room holds that nothing authorises (Ann is not above Cy): inert, and
    // the room is "full" at one ban.
    let mut state = capped(&w, 1);
    state.bans.0.push(AuthorizedUserBan::new(
        UserBan {
            owner_member_id: w.f.owner_id(),
            banned_at: at(5),
            banned_user: id(&w.cy),
        },
        id(&w.ann),
        &w.ann,
    ));
    let room = w.room(state, w.owner());
    for target in [&w.bo, w.mia()] {
        let p = preview(&room, id(target));
        match built_lifts(&room, id(target)) {
            Ok(names) => {
                assert!(!p.refused);
                assert_eq!(p.lifts, names);
            }
            Err(why) => {
                assert_eq!(why, NOT_KEPT);
                assert!(p.refused);
            }
        }
    }
    // One ban on its way fills the room's second slot: the next lifts someone, and the
    // preview (taken from the view, with the bans on their way applied) says so before the send.
    let state = capped(&w, 2);
    let first = build(&w.room(state.clone(), w.owner()), id(&w.cy), at(TIME)).unwrap();
    let state = contract_apply(&w, &state, first.delta());
    let room = w.room(state, w.owner());
    let mut o = outbox();
    o.command(&room, ban_of(&w.ann));
    next(&mut o, &room).expect("Ann's ban is on its way");
    assert!(o.command(
        &room,
        Action::PreviewBan {
            target: id(w.mia()).to_string()
        }
    ));
    let view = o.view(&room, &minutes);
    let shown = view.ban_preview[&id(w.mia()).to_string()].clone();
    assert!(!shown.refused);
    assert_eq!(
        shown.lifts.len(),
        1,
        "the on-the-way ban fills the room: {shown:?}"
    );
    // The raw room, without it, would have said nothing is lifted.
    assert!(preview(&room, id(w.mia())).lifts.is_empty());
    // What the real send then does is exactly that.
    let mut live = room.clone();
    o.answered(&mut live, Ok(()));
    assert_eq!(built_lifts(&live, id(w.mia())).unwrap(), shown.lifts);
}

#[test]
fn asking_for_nobody_clears_the_preview() {
    let w = World::new();
    let room = w.room(w.state(false), w.owner());
    let mut o = outbox();
    assert!(o.command(
        &room,
        Action::PreviewBan {
            target: id(&w.ann).to_string()
        }
    ));
    assert!(o.command(
        &room,
        Action::PreviewBan {
            target: String::new()
        }
    ));
    assert!(o.view(&room, &minutes).ban_preview.is_empty());
}

#[test]
fn the_ban_gate_follows_the_invite_chain_through_the_ban_evidence_too() {
    let w = World::new();
    let mut state = w.state(false);
    // Bo (invited by Ann, who the owner invited) is held only as ban evidence; Eve is invited by
    // Bo and is the owner's deputy. Ann is above Eve through the evidence record.
    let bo = state
        .members
        .members
        .iter()
        .find(|m| m.member.id() == id(&w.bo))
        .cloned()
        .unwrap();
    state.members.members.retain(|m| m.member.id() != id(&w.bo));
    state.members.members.retain(|m| m.member.id() != id(&w.di));
    state.ban_evidence.members.push(bo);
    let eve = k(9);
    w.admit(&mut state, &eve, &w.bo);
    let mut info = w.info(w.owner(), "Olive", false).member_info;
    info.version = 1;
    info.deputies = vec![id(&eve)];
    state
        .member_info
        .member_info
        .push(AuthorizedMemberInfo::new(info, w.owner()));
    let room = w.room(state, &eve);
    // The walk reaches Ann through Bo's evidence record, so a ban of Ann removes Eve too.
    let gate = Gate::new(&room, id(&eve));
    assert!(gate.chain.contains(&id(&w.bo)));
    assert!(gate.chain.contains(&id(&w.ann)));
}

#[test]
fn the_view_previews_only_the_member_asked_about() {
    let w = World::new();
    let state = capped(&w, 1);
    let first = build(&w.room(state.clone(), w.owner()), id(&w.cy), at(TIME)).unwrap();
    let state = contract_apply(&w, &state, first.delta());
    let room = w.room(state, w.owner());
    let mut o = outbox();
    assert!(
        o.view(&room, &minutes).ban_preview.is_empty(),
        "nothing asked"
    );
    assert!(o.command(
        &room,
        Action::PreviewBan {
            target: id(&w.ann).to_string()
        }
    ));
    let view = o.view(&room, &minutes);
    assert_eq!(view.ban_preview.len(), 1);
    assert_eq!(view.ban_preview[&id(&w.ann).to_string()].lifts.len(), 1);
    // Asking again for the same member changes nothing; another member replaces it.
    assert!(!o.command(
        &room,
        Action::PreviewBan {
            target: id(&w.ann).to_string()
        }
    ));
    assert!(o.command(
        &room,
        Action::PreviewBan {
            target: id(w.mia()).to_string()
        }
    ));
    assert!(
        o.view(&room, &minutes)
            .ban_preview
            .contains_key(&id(w.mia()).to_string())
    );
    assert_eq!(o.view(&room, &minutes).ban_preview.len(), 1);
}
