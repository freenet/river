use super::*;
use crate::rooms::create;
use crate::rooms::work;
use ed25519_dalek::SigningKey;
use river_core::room_state::member_info::MemberInfo;
use river_core::room_state::privacy::SealedBytes;
use std::collections::HashMap;

/// A public room "Open" owned by seed 60, and a member of it (seed 61) the owner invited.
pub(in crate::rooms) struct World {
    pub(in crate::rooms) owner: SigningKey,
    pub(in crate::rooms) member: SigningKey,
    pub(in crate::rooms) state: ChatRoomStateV1,
    pub(in crate::rooms) key: ContractKey,
    pub(in crate::rooms) invitee: AuthorizedMember,
}

pub(in crate::rooms) fn world() -> World {
    let owner = SigningKey::from_bytes(&[60; 32]);
    let member = SigningKey::from_bytes(&[61; 32]);
    let plan = create::new_room(owner.clone(), "Open", "Olive", false).unwrap();
    let owner_id = MemberId::from(&owner.verifying_key());
    let invitee = AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: member.verifying_key(),
        },
        &owner,
    );
    World {
        key: create::contract_key(&owner.verifying_key()),
        state: plan.room.room_state,
        owner,
        member,
        invitee,
    }
}

pub(in crate::rooms) fn member_export(w: &World) -> IdentityExport {
    IdentityExport {
        room_owner: w.owner.verifying_key(),
        signing_key: w.member.clone(),
        authorized_member: w.invitee.clone(),
        invite_chain: vec![],
        member_info: None,
        room_name: Some("Open".into()),
        self_nickname: Some("Bea".into()),
        invitation_secrets: HashMap::new(),
    }
}

fn info(w: &World, version: u32, name: &str) -> AuthorizedMemberInfo {
    AuthorizedMemberInfo::new_with_member_key(
        MemberInfo {
            member_id: MemberId::from(&w.member.verifying_key()),
            version,
            preferred_nickname: SealedBytes::public(name.as_bytes().to_vec()),
            deputies: Vec::new(),
        },
        &w.member,
    )
}

/// The slot the store would hold for the member's own identity, as a join stores it.
fn slot_of(w: &World, export: &IdentityExport) -> Vec<u8> {
    encode_new_room(&new_room(export, &w.state, w.key).unwrap()).unwrap()
}

fn stored(bytes: &[u8]) -> river_core::stored_rooms::StoredRoom {
    match decode_room_slot(bytes).unwrap() {
        StoredRoomSlot::Present(r) => *r,
        StoredRoomSlot::Tombstone => panic!("a present room"),
    }
}

fn live(w: &World, export: &IdentityExport) -> LiveRoom {
    LiveRoom::new(work::stored(&new_room(export, &w.state, w.key).unwrap()))
}

// Export -----------------------------------------------------------------------------------------

#[test]
fn a_members_export_parses_back_to_everything_it_held() {
    let w = world();
    let mut e = member_export(&w);
    e.member_info = Some(info(&w, 3, "Bea"));
    e.invitation_secrets.insert(0, [7; 32]);
    e.invitation_secrets.insert(2, [9; 32]);
    let token = export_token(&live(&w, &e)).unwrap();
    assert!(
        token.starts_with("-----BEGIN RIVER IDENTITY-----\n"),
        "{token}"
    );
    assert!(token.ends_with("-----END RIVER IDENTITY-----"));
    let back = parse_token(&token).unwrap();
    assert_eq!(back.room_owner, e.room_owner);
    assert_eq!(back.signing_key.to_bytes(), e.signing_key.to_bytes());
    assert_eq!(back.authorized_member, e.authorized_member);
    assert_eq!(back.invite_chain, e.invite_chain);
    assert_eq!(back.member_info, e.member_info);
    assert_eq!(back.room_name.as_deref(), Some("Open"));
    assert_eq!(back.self_nickname.as_deref(), Some("Bea"));
    assert_eq!(back.invitation_secrets, e.invitation_secrets);
}

#[test]
fn an_owners_export_is_self_signed_with_no_chain() {
    let w = world();
    let plan = create::new_room(w.owner.clone(), "Open", "Olive", false).unwrap();
    let room = LiveRoom::new(work::stored(&plan.room));
    let back = parse_token(&export_token(&room).unwrap()).unwrap();
    let owner_id = MemberId::from(&w.owner.verifying_key());
    assert_eq!(back.authorized_member.member.invited_by, owner_id);
    assert_eq!(
        back.authorized_member.member.member_vk,
        w.owner.verifying_key()
    );
    assert!(back.invite_chain.is_empty());
    assert_eq!(back.signing_key.to_bytes(), w.owner.to_bytes());
}

#[test]
fn a_member_with_no_cached_record_exports_what_the_room_state_holds_for_them() {
    let w = world();
    let mut state = w.state.clone();
    state.members.members.push(w.invitee.clone());
    let mut e = member_export(&w);
    let mut room = new_room(&e, &state, w.key).unwrap();
    room.self_authorized_member = None;
    let back = parse_token(&export_token(&LiveRoom::new(work::stored(&room))).unwrap()).unwrap();
    assert_eq!(back.authorized_member, w.invitee);
    e.room_name = None;
}

#[test]
fn an_export_is_refused_without_the_key_or_without_any_membership_record() {
    let w = world();
    let e = member_export(&w);
    let mut room = new_room(&e, &w.state, w.key).unwrap();
    // No cached record, and the room state does not hold the member either.
    room.self_authorized_member = None;
    assert_eq!(
        export_token(&LiveRoom::new(work::stored(&room))).unwrap_err(),
        "Cannot export: membership data not available. Try sending a message first."
    );
    // A slot that keeps the key elsewhere has nothing to export.
    let mut keyless = work::stored(&new_room(&e, &w.state, w.key).unwrap());
    keyless.self_sk = None;
    assert_eq!(
        export_token(&LiveRoom::new(keyless)).unwrap_err(),
        "Cannot export: the signing key for this room is not stored here."
    );
}

// The web UI's format ----------------------------------------------------------------------------

/// A token made from fixed keys. It pins river-core's encoding of `IdentityExport` byte for
/// byte. The web UI, `riverctl` and this app all use that type; the web UI's use of it is pinned
/// only by the substring scrape below, not by running the web UI.
const GOLDEN_RAW: &str = include_str!("identity_golden.txt");

/// The golden token with Windows line endings (a checkout with autocrlf) undone.
fn golden() -> String {
    GOLDEN_RAW.replace("\r\n", "\n")
}

fn golden_export() -> IdentityExport {
    let w = world();
    let mut e = member_export(&w);
    e.member_info = Some(info(&w, 1, "Bea"));
    e.invitation_secrets.insert(0, [7; 32]);
    e
}

#[test]
fn the_token_encoding_is_river_cores_and_has_not_changed() {
    let token = golden_export().to_armored_string();
    assert_eq!(token.trim(), golden().trim(), "the armored format changed");
    // And it imports.
    let e = parse_token(&golden()).unwrap();
    assert_eq!(e.self_nickname.as_deref(), Some("Bea"));
    assert_eq!(e.invitation_secrets.get(&0), Some(&[7; 32]));
}

/// Lines are 64 characters, base58, between the web UI's markers.
#[test]
fn the_armor_is_the_web_uis() {
    let g = golden();
    let lines: Vec<&str> = g.trim().lines().collect();
    assert_eq!(lines.first(), Some(&"-----BEGIN RIVER IDENTITY-----"));
    assert_eq!(lines.last(), Some(&"-----END RIVER IDENTITY-----"));
    for l in &lines[1..lines.len() - 1] {
        assert!(l.len() <= 64);
        assert!(bs58::decode(l).into_vec().is_ok());
    }
}

/// The web UI still builds and reads its token with river-core's type, with the same fields,
/// and this app's export fills each one. Adding a field to `IdentityExport` fails the build
/// here, and the web UI changing how it builds or reads a token fails the scrape.
#[test]
fn the_web_ui_still_exports_and_imports_with_the_same_type() {
    #[allow(dead_code)]
    fn every_field(e: IdentityExport) {
        let IdentityExport {
            room_owner: _,
            signing_key: _,
            authorized_member: _,
            invite_chain: _,
            member_info: _,
            room_name: _,
            self_nickname: _,
            invitation_secrets: _,
        } = e;
    }
    let web = include_str!("../../../../ui/src/components/members.rs").replace("\r\n", "\n");
    assert!(web.contains("use river_core::room_state::identity::IdentityExport;"));
    assert!(web.contains("export.to_armored_string()"));
    assert!(web.contains("IdentityExport::from_armored_string(&input)"));
    let literal = web
        .split("let export = IdentityExport {")
        .nth(1)
        .and_then(|s| s.split("};").next())
        .expect("the web UI builds an IdentityExport literal");
    for field in [
        "room_owner",
        "signing_key",
        "authorized_member",
        "invite_chain",
        "member_info",
        "room_name",
        "self_nickname",
        "invitation_secrets",
    ] {
        assert!(
            literal.contains(&format!("{field}:")) || literal.contains(&format!("{field},")),
            "the web UI's export no longer fills {field}"
        );
    }
    // The warning and the replace rule this app copies.
    assert!(web.contains("This token contains your private key. Treat it like a password"));
    assert!(web.contains("Importing will REPLACE it"));
    assert!(web.contains("Close any other tabs or devices open to this room first."));
    assert!(web.contains("Invalid token: {}"));
    assert!(web.contains("Copy to Clipboard"));
}

#[test]
fn the_screens_say_what_the_web_ui_says() {
    use crate::chat::identity_texts as t;
    let web = include_str!("../../../../ui/src/components/members.rs").replace("\r\n", "\n");
    assert!(web.contains(t::EXPORT_INTRO));
    assert!(web.contains(t::EXPORT_WARNING));
    assert!(web.contains(t::IMPORT_INTRO));
    assert!(web.contains(t::REPLACE_WARNING.replace('\u{2014}', "\\u{2014}").as_str()));
    assert!(web.contains(t::REPLACE_SESSIONS_WARNING));
}

// Checking a pasted token ----------------------------------------------------------------------

#[test]
fn a_token_is_checked_before_anything_else_happens() {
    assert_eq!(
        parse_token("   \n").unwrap_err(),
        "Paste an identity token."
    );
    assert!(
        parse_token("not a token")
            .unwrap_err()
            .starts_with("Invalid token: ")
    );
    assert!(
        parse_token("-----BEGIN RIVER IDENTITY-----\n-----END RIVER IDENTITY-----")
            .unwrap_err()
            .contains("Empty identity token")
    );
    let long = "1".repeat(MAX_TOKEN_LEN + 1);
    assert!(parse_token(&long).unwrap_err().contains("far longer"));
    // Truncated: decodes as base58 but not as an identity.
    let cut: String = golden()
        .trim()
        .lines()
        .take(2)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        parse_token(&cut)
            .unwrap_err()
            .starts_with("Invalid token: ")
    );
}

#[test]
fn a_token_whose_key_is_not_the_members_is_refused() {
    let w = world();
    let mut e = member_export(&w);
    e.signing_key = SigningKey::from_bytes(&[99; 32]);
    let err = parse_token(&e.to_armored_string()).unwrap_err();
    assert!(err.contains("does not match"), "{err}");
}

#[test]
fn a_token_with_a_forged_invite_signature_is_refused() {
    let w = world();
    let mut e = member_export(&w);
    // The owner's signature, but over another member's record.
    let owner_id = MemberId::from(&w.owner.verifying_key());
    let someone_else = AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: SigningKey::from_bytes(&[98; 32]).verifying_key(),
        },
        &w.owner,
    );
    e.authorized_member.signature = someone_else.signature;
    let err = parse_token(&e.to_armored_string()).unwrap_err();
    assert!(err.contains("Invalid authorized_member signature"), "{err}");
}

#[test]
fn a_record_for_another_owner_is_refused() {
    let w = world();
    let mut e = member_export(&w);
    let other = SigningKey::from_bytes(&[97; 32]);
    // The record names a different owner than the room the token says it is for.
    let other_id = MemberId::from(&other.verifying_key());
    e.authorized_member = AuthorizedMember::new(
        Member {
            owner_member_id: other_id,
            invited_by: other_id,
            member_vk: w.member.verifying_key(),
        },
        &other,
    );
    let err = parse_token(&e.to_armored_string()).unwrap_err();
    assert!(err.contains("different room owner"), "{err}");
}

#[test]
fn the_room_from_the_network_must_be_the_one_the_token_is_for() {
    let w = world();
    let e = member_export(&w);
    let other = create::new_room(SigningKey::from_bytes(&[55; 32]), "Other", "O", false).unwrap();
    let r = new_room(&e, &other.room.room_state, w.key).err().unwrap();
    assert!(r.why.contains("isn't the one") && !r.retry, "{r:?}");
}

// Writing the slot -----------------------------------------------------------------------------

/// `bytes` with one extra field in the present room's map, as a newer web UI might write.
fn with_unknown_field(bytes: &[u8]) -> Vec<u8> {
    let Value::Map(mut outer) = ciborium::de::from_reader::<Value, _>(bytes).unwrap() else {
        panic!("a map");
    };
    let Some((_, Value::Map(fields))) = outer
        .iter_mut()
        .find(|(k, _)| k.as_text() == Some("Present"))
    else {
        panic!("present");
    };
    fields.push((Value::Text("future_field".into()), Value::from(7u32)));
    let mut out = Vec::new();
    ciborium::ser::into_writer(&Value::Map(outer), &mut out).unwrap();
    out
}

fn fields_of(bytes: &[u8]) -> Vec<(String, Value)> {
    let Value::Map(outer) = ciborium::de::from_reader::<Value, _>(bytes).unwrap() else {
        panic!("a map");
    };
    let Some((_, Value::Map(fields))) = outer
        .into_iter()
        .find(|(k, _)| k.as_text() == Some("Present"))
    else {
        panic!("present");
    };
    fields
        .into_iter()
        .map(|(k, v)| (k.as_text().unwrap().to_owned(), v))
        .collect()
}

fn store(w: ImportWrite) -> Vec<u8> {
    match w {
        ImportWrite::Store(b) => b,
        other => panic!("expected a store, got {other:?}"),
    }
}

#[test]
fn replacing_with_a_different_identity_changes_only_the_identity_fields() {
    let w = world();
    // The stored room: the OWNER's identity, with a future field and a read position.
    let old = member_export(&w);
    let mut other_sk = SigningKey::from_bytes(&[62; 32]);
    let owner_id = MemberId::from(&w.owner.verifying_key());
    let old_member = AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: other_sk.verifying_key(),
        },
        &w.owner,
    );
    let mut stored_export = old.clone();
    stored_export.signing_key = other_sk.clone();
    stored_export.authorized_member = old_member;
    stored_export.self_nickname = Some("Old".into());
    stored_export.invitation_secrets.insert(1, [1; 32]);
    let slot = with_unknown_field(&slot_of(&w, &stored_export));
    other_sk = SigningKey::from_bytes(&[0; 32]);
    drop(other_sk);

    let mut incoming = member_export(&w);
    incoming.invitation_secrets.insert(0, [5; 32]);
    let out = store(slot_with_identity(Some(&slot), &incoming).unwrap());

    let now = stored(&out);
    assert_eq!(now.self_verifying_key(), Some(w.member.verifying_key()));
    assert_eq!(now.self_vk, Some(w.member.verifying_key()));
    assert_eq!(now.self_authorized_member, Some(w.invitee.clone()));
    assert_eq!(now.self_nickname.as_deref(), Some("Bea"));
    assert_eq!(now.invitation_secrets.get(&0), Some(&[5; 32]));
    assert!(
        !now.invitation_secrets.contains_key(&1),
        "the old identity's secrets are not carried to the new one"
    );
    // Everything else is as it was, byte for byte as CBOR, including a field this app does not
    // know.
    let before = fields_of(&slot);
    let after = fields_of(&out);
    let identity = [
        "self_sk",
        "self_vk",
        "self_authorized_member",
        "invite_chain",
        "self_member_info",
        "self_nickname",
        "invitation_secrets",
    ];
    for (name, value) in &before {
        if identity.contains(&name.as_str()) {
            continue;
        }
        let kept = after.iter().find(|(n, _)| n == name).map(|(_, v)| v);
        assert_eq!(kept, Some(value), "{name} changed");
    }
    assert!(after.iter().any(|(n, _)| n == "future_field"));
    assert_eq!(before.len(), after.len(), "no field was added or lost");
}

#[test]
fn replacing_with_the_same_identity_never_erases_what_the_slot_has() {
    let w = world();
    let mut held = member_export(&w);
    held.member_info = Some(info(&w, 5, "Newer"));
    held.self_nickname = Some("Chosen".into());
    held.invitation_secrets.insert(1, [1; 32]);
    let slot = slot_of(&w, &held);
    // An older token of the same identity: older record, no nickname, other secrets.
    let mut stale = member_export(&w);
    stale.member_info = Some(info(&w, 2, "Older"));
    stale.self_nickname = None;
    stale.invitation_secrets.insert(1, [2; 32]);
    stale.invitation_secrets.insert(3, [3; 32]);
    // Nothing the slot lacks except secret 3.
    let out = store(slot_with_identity(Some(&slot), &stale).unwrap());
    let now = stored(&out);
    assert_eq!(now.self_member_info.unwrap().member_info.version, 5);
    assert_eq!(now.self_nickname.as_deref(), Some("Chosen"));
    assert_eq!(
        now.invitation_secrets.get(&1),
        Some(&[1; 32]),
        "the slot's wins"
    );
    assert_eq!(now.invitation_secrets.get(&3), Some(&[3; 32]));
    // A newer record in the token wins, and a nickname it holds is a deliberate choice.
    let mut fresh = member_export(&w);
    fresh.member_info = Some(info(&w, 9, "Newest"));
    fresh.self_nickname = Some("Fresh".into());
    let now = stored(&store(slot_with_identity(Some(&out), &fresh).unwrap()));
    assert_eq!(now.self_member_info.unwrap().member_info.version, 9);
    assert_eq!(now.self_nickname.as_deref(), Some("Fresh"));
}

#[test]
fn the_same_import_twice_changes_nothing_the_second_time() {
    let w = world();
    let old = {
        let mut e = member_export(&w);
        e.self_nickname = Some("Old".into());
        e
    };
    let slot = slot_of(&w, &old);
    let e = member_export(&w);
    let once = store(slot_with_identity(Some(&slot), &e).unwrap());
    assert_eq!(
        slot_with_identity(Some(&once), &e).unwrap(),
        ImportWrite::Unchanged
    );
}

#[test]
fn a_slot_that_is_gone_left_unreadable_or_another_rooms_is_never_written() {
    let w = world();
    let e = member_export(&w);
    for current in [None, Some(Vec::new())] {
        let r = slot_with_identity(current.as_deref(), &e).unwrap();
        assert!(
            matches!(&r, ImportWrite::Refuse(why) if why.contains("no longer in River's storage")),
            "{r:?}"
        );
    }
    let mut tomb = Vec::new();
    ciborium::ser::into_writer(&Value::Text("Tombstone".into()), &mut tomb).unwrap();
    let r = slot_with_identity(Some(&tomb), &e).unwrap();
    assert!(matches!(r, ImportWrite::Refuse(_)));
    let r = slot_with_identity(Some(b"\xff\xfe garbage"), &e).unwrap();
    assert!(
        matches!(&r, ImportWrite::Refuse(why) if why.contains("can't read")),
        "{r:?}"
    );
    // A slot for a different owner (under the wrong key).
    let other = create::new_room(SigningKey::from_bytes(&[55; 32]), "Other", "O", false).unwrap();
    let theirs = encode_new_room(&other.room).unwrap();
    let r = slot_with_identity(Some(&theirs), &e).unwrap();
    assert!(
        matches!(&r, ImportWrite::Refuse(why) if why.contains("different room")),
        "{r:?}"
    );
}

#[test]
fn adding_a_room_goes_over_nothing_or_a_tombstone_and_never_over_a_room() {
    let w = world();
    let e = member_export(&w);
    let room = new_room(&e, &w.state, w.key).unwrap();
    assert!(matches!(add_room(None, &room, &e), ImportWrite::Store(_)));
    assert!(matches!(
        add_room(Some(&[]), &room, &e),
        ImportWrite::Store(_)
    ));
    let mut tomb = Vec::new();
    ciborium::ser::into_writer(&Value::Text("Tombstone".into()), &mut tomb).unwrap();
    assert!(matches!(
        add_room(Some(&tomb), &room, &e),
        ImportWrite::Store(_)
    ));
    // The same identity already there: an earlier store landed.
    let slot = encode_new_room(&room).unwrap();
    assert!(!matches!(
        add_room(Some(&slot), &room, &e),
        ImportWrite::Refuse(_)
    ));
    // Another identity there: never replaced without the user saying so.
    let owners = create::new_room(w.owner.clone(), "Open", "Olive", false).unwrap();
    let theirs = encode_new_room(&owners.room).unwrap();
    match add_room(Some(&theirs), &room, &e) {
        ImportWrite::Refuse(why) => assert_eq!(
            why,
            "You already have this room. Import again and choose to replace your identity in it."
        ),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        add_room(Some(b"\xff garbage"), &room, &e),
        ImportWrite::Refuse(_)
    ));
}

/// The same identity already stored, imported again as an add: merged as the web UI merges a
/// re-import (a newer record and a nickname the token carries are not dropped).
#[test]
fn adding_over_the_same_identity_merges_the_token_in() {
    let w = world();
    let mut held = member_export(&w);
    held.member_info = Some(info(&w, 1, "Old"));
    held.self_nickname = Some("Old".into());
    let slot = slot_of(&w, &held);
    let mut token = member_export(&w);
    token.member_info = Some(info(&w, 4, "Newer"));
    token.self_nickname = Some("Newer".into());
    token.invitation_secrets.insert(2, [2; 32]);
    let room = new_room(&token, &w.state, w.key).unwrap();
    let out = store(add_room(Some(&slot), &room, &token));
    let now = stored(&out);
    assert_eq!(now.self_member_info.unwrap().member_info.version, 4);
    assert_eq!(now.self_nickname.as_deref(), Some("Newer"));
    assert_eq!(now.invitation_secrets.get(&2), Some(&[2; 32]));
}

// Open-ended imports keep what a rejoin needs ----------------------------------------------------

/// A member invited by a member, with an empty chain, imported against a room that lists the
/// inviter: the stored identity carries the inviter and the records above them, so after the
/// room prunes the member AND the inviter, the rejoin still re-adds the whole chain.
#[test]
fn an_open_ended_import_stores_the_verified_chain_for_a_later_rejoin() {
    let w = world();
    let top = SigningKey::from_bytes(&[87; 32]);
    let mid = SigningKey::from_bytes(&[80; 32]);
    let mut e = member_export(&w);
    e.authorized_member = record(&w.owner, &mid, &w.member);
    e.invite_chain = vec![];
    let mut state = w.state.clone();
    state.members.members.push(record(&w.owner, &w.owner, &top));
    state.members.members.push(record(&w.owner, &top, &mid));
    // Add path.
    let room = new_room(&e, &state, w.key).unwrap();
    let ids: Vec<_> = room
        .invite_chain
        .iter()
        .map(|m| m.member.member_vk)
        .collect();
    assert_eq!(ids, [mid.verifying_key(), top.verifying_key()]);
    // The room prunes the member and both inviters: the rejoin re-adds all of them.
    let mut pruned = room.clone();
    pruned.room_state = w.state.clone();
    let live = LiveRoom::new(work::stored(&pruned));
    let (members, _) = crate::rooms::outbox::rejoin(&live);
    let added: Vec<_> = members
        .expect("a members delta")
        .added()
        .iter()
        .map(|m| m.member.member_vk)
        .collect();
    for vk in [
        w.member.verifying_key(),
        mid.verifying_key(),
        top.verifying_key(),
    ] {
        assert!(added.contains(&vk), "a rejoin carries every ancestor");
    }
    // Replace path: the slot gets the same chain.
    let other = encode_new_room(&{
        let mut r = new_room(&member_export(&w), &w.state, w.key).unwrap();
        r.room_state = state;
        r.self_sk = Some(SigningKey::from_bytes(&[99; 32]));
        r.self_vk = None;
        r
    })
    .unwrap();
    let out = store(slot_with_identity(Some(&other), &e).unwrap());
    assert_eq!(stored(&out).invite_chain.len(), 2);
}

#[test]
fn an_inviter_that_is_the_member_itself_is_refused() {
    let w = world();
    let mut e = member_export(&w);
    e.authorized_member = record(&w.owner, &w.member, &w.member);
    let err = parse_token(&e.to_armored_string()).unwrap_err();
    assert!(err.contains("own inviter"), "{err}");
    let r = new_room(&e, &w.state, w.key).err().unwrap();
    assert!(r.why.contains("own inviter") && !r.retry, "{r:?}");
}

/// The slot already holds this identity's record: a re-import merges even though the inviter has
/// been pruned from the stored state since.
#[test]
fn a_same_identity_reimport_needs_no_inviter_in_the_stored_state() {
    let w = world();
    let mid = SigningKey::from_bytes(&[80; 32]);
    let mut held = member_export(&w);
    held.authorized_member = record(&w.owner, &mid, &w.member);
    held.invite_chain = vec![record(&w.owner, &w.owner, &mid)];
    held.self_nickname = Some("Old".into());
    // The stored state lists nobody (the inviter was pruned).
    let slot = slot_of(&w, &held);
    let mut again = held.clone();
    again.invite_chain = vec![];
    again.self_nickname = Some("New".into());
    let out = store(slot_with_identity(Some(&slot), &again).unwrap());
    let now = stored(&out);
    assert_eq!(now.self_nickname.as_deref(), Some("New"));
    assert_eq!(now.invite_chain.len(), 1, "the slot's own chain stands");
    // A DIFFERENT identity with an unlisted inviter is still refused.
    let mut other = held.clone();
    other.signing_key = SigningKey::from_bytes(&[88; 32]);
    other.authorized_member = record(&w.owner, &mid, &other.signing_key);
    other.invite_chain = vec![];
    match slot_with_identity(Some(&slot), &other).unwrap() {
        ImportWrite::Refuse(why) => assert!(why.contains("can't be checked"), "{why}"),
        o => panic!("{o:?}"),
    }
}

// What a crafted token cannot do ---------------------------------------------------------------

fn record(owner: &SigningKey, invited_by: &SigningKey, member: &SigningKey) -> AuthorizedMember {
    AuthorizedMember::new(
        Member {
            owner_member_id: MemberId::from(&owner.verifying_key()),
            invited_by: MemberId::from(&invited_by.verifying_key()),
            member_vk: member.verifying_key(),
        },
        invited_by,
    )
}

#[test]
fn a_chain_that_leads_to_the_owner_link_by_link_is_accepted() {
    let w = world();
    let mid = SigningKey::from_bytes(&[80; 32]);
    let mut e = member_export(&w);
    e.authorized_member = record(&w.owner, &mid, &w.member);
    e.invite_chain = vec![record(&w.owner, &w.owner, &mid)];
    parse_token(&e.to_armored_string()).unwrap();
}

/// river-core skips a signature whose inviter it cannot find, so this token passes its
/// decoder: the member's record names an inviter that is nowhere, signed by anyone.
#[test]
fn a_record_whose_inviter_is_nowhere_is_refused_once_the_room_is_known() {
    let w = world();
    let nobody = SigningKey::from_bytes(&[81; 32]);
    let mut e = member_export(&w);
    e.authorized_member = record(&w.owner, &nobody, &w.member);
    assert!(
        IdentityExport::from_armored_string(&e.to_armored_string()).is_ok(),
        "river-core's decoder alone lets it through"
    );
    // The paste alone cannot say (a real token can hold an empty chain), so it is read.
    let e = parse_token(&e.to_armored_string()).unwrap();
    // The room does not list the inviter: refused, final.
    let r = new_room(&e, &w.state, w.key).err().unwrap();
    assert!(
        r.why.contains("can't be checked against the room") && !r.retry,
        "{r:?}"
    );
    // And a replace over a stored room that does not list them.
    let owners = create::new_room(w.owner.clone(), "Open", "Olive", false).unwrap();
    let slot = encode_new_room(&owners.room).unwrap();
    match slot_with_identity(Some(&slot), &e).unwrap() {
        ImportWrite::Refuse(why) => assert!(why.contains("can't be checked"), "{why}"),
        other => panic!("{other:?}"),
    }
}

/// A member invited by a member, with an EMPTY chain (the web UI sets the record and the chain
/// separately, so a real token can look like this): accepted when the room lists the inviter
/// with a chain to the owner and the record is signed by them; refused when it does not show.
#[test]
fn an_empty_chain_is_accepted_when_the_room_shows_the_inviter_and_refused_when_not() {
    let w = world();
    let mid = SigningKey::from_bytes(&[80; 32]);
    let mut e = member_export(&w);
    e.authorized_member = record(&w.owner, &mid, &w.member);
    e.invite_chain = vec![];
    parse_token(&e.to_armored_string()).unwrap();
    // Resolvable in the room read from the network.
    let mut state = w.state.clone();
    state.members.members.push(record(&w.owner, &w.owner, &mid));
    new_room(&e, &state, w.key).unwrap();
    // The inviter is there but did not sign this record.
    let mut forged = e.clone();
    forged.authorized_member.signature = record(&w.owner, &w.owner, &w.member).signature;
    let r = new_room(&forged, &state, w.key).err().unwrap();
    assert!(
        r.why.contains("not signed by the member the room lists"),
        "{}",
        r.why
    );
    // The inviter is there but their own chain does not reach the owner.
    let top = SigningKey::from_bytes(&[86; 32]);
    let mut orphan = w.state.clone();
    orphan.members.members.push(record(&w.owner, &top, &mid));
    let r = new_room(&e, &orphan, w.key).err().unwrap();
    assert!(!r.retry && r.why.contains("Invalid token"), "{r:?}");
    // Not listed at all: cannot be shown.
    let r = new_room(&e, &w.state, w.key).err().unwrap();
    assert!(
        r.why.contains("can't be checked against the room"),
        "{}",
        r.why
    );
    // A replace is decided the same way on the stored room.
    let held = encode_new_room(&new_room(&e, &state, w.key).unwrap()).unwrap();
    assert!(matches!(
        slot_with_identity(Some(&held), &e).unwrap(),
        ImportWrite::Unchanged | ImportWrite::Store(_)
    ));
}

#[test]
fn a_chain_with_a_missing_or_unsigned_link_is_refused() {
    let w = world();
    let mid = SigningKey::from_bytes(&[80; 32]);
    let top = SigningKey::from_bytes(&[82; 32]);
    // The member's inviter is in the chain, but the chain's top names someone who is not.
    let mut e = member_export(&w);
    e.authorized_member = record(&w.owner, &mid, &w.member);
    e.invite_chain = vec![record(&w.owner, &top, &mid)];
    let r = new_room(
        &parse_token(&e.to_armored_string()).unwrap(),
        &w.state,
        w.key,
    )
    .err()
    .unwrap();
    assert!(
        r.why.contains("can't be checked against the room"),
        "{}",
        r.why
    );
    // A link signed by the wrong key.
    let mut e = member_export(&w);
    e.authorized_member = record(&w.owner, &mid, &w.member);
    let mut link = record(&w.owner, &w.owner, &mid);
    link.signature = record(&w.owner, &w.owner, &top).signature;
    e.invite_chain = vec![link];
    let err = parse_token(&e.to_armored_string()).unwrap_err();
    assert!(
        err.contains("not signed by the room's owner") || err.contains("Invalid"),
        "{err}"
    );
}

#[test]
fn a_chain_member_for_another_owner_is_refused() {
    let w = world();
    let mid = SigningKey::from_bytes(&[80; 32]);
    let other = SigningKey::from_bytes(&[83; 32]);
    let mut e = member_export(&w);
    e.authorized_member = record(&w.owner, &mid, &w.member);
    e.invite_chain = vec![record(&other, &other, &mid)];
    let err = parse_token(&e.to_armored_string()).unwrap_err();
    assert!(err.contains("different room owner"), "{err}");
}

#[test]
fn a_chain_cannot_loop() {
    let w = world();
    let a = SigningKey::from_bytes(&[84; 32]);
    let b = SigningKey::from_bytes(&[85; 32]);
    let mut e = member_export(&w);
    e.authorized_member = record(&w.owner, &a, &w.member);
    e.invite_chain = vec![record(&w.owner, &b, &a), record(&w.owner, &a, &b)];
    // The walk ends (no hang) with a refusal once the room is asked.
    let e = parse_token(&e.to_armored_string()).unwrap_or(e);
    assert!(new_room(&e, &w.state, w.key).is_err());
}

#[test]
fn what_the_decoder_says_about_the_token_is_bounded_and_plain() {
    let err = parse_token("-----BEGIN RIVER IDENTITY-----\nabc\n-----END RIVER IDENTITY-----")
        .unwrap_err();
    assert!(
        err.chars().count() <= 200 && !err.chars().any(char::is_control),
        "{err}"
    );
    assert_eq!(plain("a\u{202e}b\nc", 10), "a\u{202e}b c");
    assert_eq!(plain(&"x".repeat(300), 160).chars().count(), 161);
}

#[test]
fn the_paste_cap_is_far_above_a_real_token_and_enforced() {
    assert!(
        golden().len() * 8 < MAX_TOKEN_LEN,
        "a real token is a small part of the cap"
    );
    assert!(
        parse_token(&"1".repeat(MAX_TOKEN_LEN + 1))
            .unwrap_err()
            .contains("far longer")
    );
    // Just under the cap is decoded, not refused for length (it is not a token).
    let err = parse_token(&"1".repeat(MAX_TOKEN_LEN)).unwrap_err();
    assert!(!err.contains("far longer"), "{err}");
}

fn ban_of(w: &World, who: &SigningKey) -> river_core::room_state::ban::AuthorizedUserBan {
    river_core::room_state::ban::AuthorizedUserBan::new(
        river_core::room_state::ban::UserBan {
            owner_member_id: MemberId::from(&w.owner.verifying_key()),
            banned_at: std::time::SystemTime::now().into(),
            banned_user: MemberId::from(&who.verifying_key()),
        },
        MemberId::from(&w.owner.verifying_key()),
        &w.owner,
    )
}

#[test]
fn a_banned_identity_is_refused_for_an_add_with_try_again_and_for_a_replace() {
    let w = world();
    let e = member_export(&w);
    // An enforced ban of the member, who is no longer in the members.
    let mut state = w.state.clone();
    state.bans.0.push(ban_of(&w, &w.member));
    let r = new_room(&e, &state, w.key).err().unwrap();
    assert_eq!(
        (r.why.as_str(), r.retry),
        (BANNED, true),
        "the copy read may be stale"
    );
    // The same for a replace, from the stored room's state.
    let mut banned_room = new_room(&e, &w.state, w.key).unwrap();
    banned_room.room_state = state.clone();
    banned_room.self_sk = Some(SigningKey::from_bytes(&[99; 32]));
    banned_room.self_vk = None;
    let slot = encode_new_room(&banned_room).unwrap();
    assert_eq!(
        slot_with_identity(Some(&slot), &e).unwrap(),
        ImportWrite::Refuse(BANNED.into())
    );
    // A member merely absent from the state (pruned, or a partial copy) is not banned.
    assert!(!banned_in(&w.state, &e));
    new_room(&e, &w.state, w.key).unwrap();
    // The owner is never banned.
    let mut owner_e = e.clone();
    owner_e.signing_key = w.owner.clone();
    assert!(!banned_in(&state, &owner_e));
}

// Not printing the key -------------------------------------------------------------------------

#[test]
fn nothing_prints_a_token() {
    let token = golden_export().to_armored_string();
    let body: String = token.lines().filter(|l| !l.starts_with("---")).collect();
    let marker = &body[..24];
    let shown = [
        format!("{:?}", IdentityToken(token.clone())),
        format!(
            "{:?}",
            crate::rooms::Action::ImportId {
                token: IdentityToken(token.clone()),
                replace: true
            }
        ),
        format!(
            "{:?}",
            crate::rooms::RoomsUpdate::Export {
                room: "r".into(),
                token: Ok(token.clone())
            }
        ),
        format!("{:?}", crate::chat::identity_texts::Secret(token.clone())),
    ];
    for s in shown {
        assert!(!s.contains(marker) && !s.contains("BEGIN"), "{s}");
    }
}
