use super::*;
use crate::rooms::room_tests::{Fixture, SECRET};
use crate::rooms::work::stored;
use freenet_stdlib::client_api::ContractRequest;
use river_core::room_state::member::MemberId;

fn now() -> SystemTime {
    SystemTime::now()
}

fn minutes(t: SystemTime) -> u32 {
    (t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        / 60) as u32
}

// A new room --------------------------------------------------------------------------------

/// The room contract is compiled in, so CI must build and test this app when it changes: a new
/// contract changes every room's key.
#[test]
fn apps_yml_runs_when_the_room_contract_changes() {
    const WORKFLOW: &str = include_str!("../../../../.github/workflows/apps.yml");
    assert_eq!(
        WORKFLOW
            .matches("- 'ui/public/contracts/room_contract.wasm'")
            .count(),
        2,
        "in the pull_request and push path lists"
    );
}

#[test]
fn a_new_public_room_is_the_web_uis() {
    let owner = SigningKey::from_bytes(&[40; 32]);
    let owner_vk = owner.verifying_key();
    let plan = new_room(owner.clone(), "Book club", "Ada", false).unwrap();
    let room = &plan.room;
    assert_eq!(room.owner_vk, owner_vk);
    assert_eq!(room.self_verifying_key(), Some(owner_vk));
    assert_eq!(room.self_vk, Some(owner_vk));
    // The web UI's key for this owner and the bundled contract.
    let expected = ContractKey::from_params_and_code(
        Parameters::from(params_bytes(&owner_vk)),
        ContractCode::from(ROOM_CONTRACT_WASM),
    );
    assert_eq!(room.contract_key, expected);
    // The owner holds none of an invitee's credentials.
    assert!(room.self_authorized_member.is_none() && room.invite_chain.is_empty());
    assert!(room.invitation_secrets.is_empty() && room.self_nickname.is_none());
    let state = &room.room_state;
    let params = ChatRoomParametersV1 { owner: owner_vk };
    state.verify(state, &params).expect("the room accepts it");
    assert_eq!(state.normalized(&params).unwrap(), *state);
    let config = &state.configuration.configuration;
    assert_eq!(config.privacy_mode, PrivacyMode::Public);
    assert_eq!(config.owner_member_id, MemberId::from(&owner_vk));
    let view = LiveRoom::new(stored(room)).view(&minutes);
    assert_eq!(view.name, "Book club");
    assert!(!view.private);
    let me = view.members.iter().find(|m| m.id == view.me).unwrap();
    assert_eq!(me.nickname, "Ada");
    assert_eq!(state.member_info.member_info[0].member_info.version, 0);
}

#[test]
fn a_new_private_room_seals_its_name_and_nickname_and_gives_the_owner_the_secret() {
    let owner = SigningKey::from_bytes(&[41; 32]);
    let plan = new_room(owner, "Secret club", "Ada", true).unwrap();
    let state = &plan.room.room_state;
    let config = &state.configuration.configuration;
    assert_eq!(config.privacy_mode, PrivacyMode::Private);
    assert!(matches!(
        config.display.name,
        SealedBytes::Private {
            secret_version: 0,
            declared_len_bytes: 11,
            ..
        }
    ));
    assert!(matches!(
        state.member_info.member_info[0]
            .member_info
            .preferred_nickname,
        SealedBytes::Private {
            secret_version: 0,
            ..
        }
    ));
    assert_eq!(state.secrets.current_version, 0);
    assert_eq!(state.secrets.versions.len(), 1);
    assert_eq!(state.secrets.encrypted_secrets.len(), 1);
    // The owner reads it back from the room alone, as the web UI does after a reload.
    let view = LiveRoom::new(stored(&plan.room)).view(&minutes);
    assert_eq!(view.name, "Secret club");
    assert!(view.private);
    let me = view.members.iter().find(|m| m.id == view.me).unwrap();
    assert_eq!(me.nickname, "Ada");
    // The room's secret is not stored beside the key: the owner's blob is the record.
    assert!(plan.room.invitation_secrets.is_empty());
}

#[test]
fn the_seed_put_carries_the_contract_its_parameters_and_the_normalized_state() {
    let plan = new_room(SigningKey::from_bytes(&[42; 32]), "Room", "Ada", false).unwrap();
    let ContractRequest::Put {
        contract,
        state,
        subscribe,
        ..
    } = plan.put()
    else {
        panic!("a PUT");
    };
    assert!(subscribe);
    assert_eq!(contract.key(), plan.room.contract_key);
    let sent: ChatRoomStateV1 = ciborium::de::from_reader(state.as_ref()).unwrap();
    assert_eq!(sent, plan.room.room_state);
}

#[test]
fn a_room_the_room_would_refuse_is_not_made() {
    let owner = || SigningKey::from_bytes(&[43; 32]);
    for (name, nick) in [
        ("", "Ada"),
        ("   ", "Ada"),
        ("Book\u{200B}club", "Ada"),
        ("Book club", "A\u{202E}da"),
        (&*"n".repeat(1000), "Ada"),
        ("Book club", &*"k".repeat(1000)),
    ] {
        assert!(create_refusal(name, nick).is_some(), "{name:?} / {nick:?}");
        assert!(new_room(owner(), name, nick, false).is_err());
    }
    assert_eq!(create_refusal("Book club", "Ada"), None);
}

// Invitations --------------------------------------------------------------------------------

const WEB_APP: &str = "http://127.0.0.1:7509/v1/contract/web/raAq/";

#[test]
fn an_invitation_is_signed_by_the_inviting_member_and_carries_the_rooms_secrets() {
    let f = Fixture::new();
    let room = LiveRoom::new(f.stored(f.private_state()));
    let (inv, link) = invitation(&room, WEB_APP).unwrap();
    assert_eq!(inv.room, f.owner.verifying_key());
    let member = &inv.invitee.member;
    assert_eq!(member.owner_member_id, f.owner_id());
    // `me` invites, not the owner: the signature is `me`'s.
    assert_eq!(member.invited_by, f.me_id());
    assert_eq!(member.member_vk, inv.invitee_signing_key.verifying_key());
    inv.invitee
        .verify_signature(&f.me.verifying_key())
        .expect("signed by the inviting member");
    assert!(
        inv.invitee
            .verify_signature(&f.owner.verifying_key())
            .is_err()
    );
    assert_eq!(inv.room_secrets, vec![(0, SECRET)]);
    // The link is the web app's, with the code.
    let code = link
        .strip_prefix(&format!("{WEB_APP}?invitation="))
        .unwrap();
    assert!(Invitation::from_encoded_string(code).unwrap() == inv);
    // Each invitation is a new invitee.
    let (again, _) = invitation(&room, WEB_APP).unwrap();
    assert_ne!(
        again.invitee_signing_key.to_bytes(),
        inv.invitee_signing_key.to_bytes()
    );
}

#[test]
fn a_public_room_invitation_carries_no_secrets_and_a_room_without_a_key_makes_none() {
    let plan = new_room(SigningKey::from_bytes(&[44; 32]), "Open", "Ada", false).unwrap();
    let room = LiveRoom::new(stored(&plan.room));
    let (inv, _) = invitation(&room, WEB_APP).unwrap();
    assert!(inv.room_secrets.is_empty());
    let f = Fixture::new();
    let mut keyless = f.stored(f.private_state());
    keyless.self_sk = None;
    keyless.self_vk = Some(f.me.verifying_key());
    assert!(invitation(&LiveRoom::new(keyless), WEB_APP).is_err());
}

#[test]
fn secrets_by_version_are_in_version_order() {
    let f = Fixture::new();
    let mut state = f.private_state();
    crate::rooms::room_tests::add_secret(&f, &mut state, 2, &[9; 32], &f.me);
    crate::rooms::room_tests::add_secret(&f, &mut state, 1, &[8; 32], &f.me);
    let room = LiveRoom::new(f.stored(state));
    let versions: Vec<u32> = room.secrets_by_version().iter().map(|(v, _)| *v).collect();
    assert_eq!(versions, [0, 1, 2]);
}

// Joining ------------------------------------------------------------------------------------

/// A public room owned by `owner`, and an invitation into it from the owner.
fn public_room_and_invitation(owner: &SigningKey) -> (ChatRoomStateV1, Invitation) {
    let plan = new_room(owner.clone(), "Open room", "Olive", false).unwrap();
    let room = LiveRoom::new(stored(&plan.room));
    let (inv, _) = invitation(&room, WEB_APP).unwrap();
    (plan.room.room_state, inv)
}

#[test]
fn joining_adds_the_invitee_their_nickname_and_a_join_event() {
    let owner = SigningKey::from_bytes(&[45; 32]);
    let (state, inv) = public_room_and_invitation(&owner);
    let key = contract_key(&owner.verifying_key());
    let at = now();
    let plan = join(&inv, "Bea", &state, key, at).unwrap();
    let me = MemberId::from(&inv.invitee_signing_key.verifying_key());
    let delta = plan.delta.as_ref().unwrap();
    assert_eq!(
        delta.members.as_ref().unwrap().added(),
        std::slice::from_ref(&inv.invitee)
    );
    let info = &delta.member_info.as_ref().unwrap()[0];
    assert_eq!(info.member_info.member_id, me);
    assert_eq!(info.member_info.version, 0);
    assert_eq!(
        info.member_info.preferred_nickname,
        SealedBytes::public(b"Bea".to_vec())
    );
    info.verify_signature_with_key(&inv.invitee_signing_key.verifying_key())
        .expect("signed by the invitee");
    let messages = delta.recent_messages.as_ref().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].message.author, me);
    assert_eq!(messages[0].message.content, RoomMessageBody::join_event());
    assert_eq!(
        messages[0].message.time,
        crate::signed_time::whole_millis(at)
    );
    // Joining again at the same time sends the same bytes (a resend after a lost answer).
    let again = join(&inv, "Bea", &state, key, at).unwrap();
    let cbor = |d: &ChatRoomStateV1Delta| {
        let mut b = Vec::new();
        ciborium::ser::into_writer(d, &mut b).unwrap();
        b
    };
    assert_eq!(cbor(again.delta.as_ref().unwrap()), cbor(delta));

    // What is stored: the invitation's identity and credentials, the room with the join in.
    let room = &plan.room;
    assert_eq!(room.owner_vk, owner.verifying_key());
    assert_eq!(room.contract_key, key);
    assert_eq!(
        room.self_verifying_key(),
        Some(inv.invitee_signing_key.verifying_key())
    );
    assert_eq!(room.self_authorized_member.as_ref(), Some(&inv.invitee));
    assert_eq!(room.self_member_info.as_ref(), Some(info));
    assert_eq!(room.self_nickname.as_deref(), Some("Bea"));
    let view = LiveRoom::new(stored(room)).view(&minutes);
    assert!(
        view.members
            .iter()
            .any(|m| m.nickname == "Bea" && m.id == view.me)
    );
    assert!(view.send_block.is_none(), "{:?}", view.send_block);
}

#[test]
fn joining_a_private_room_seals_the_nickname_with_the_invitations_secret() {
    let f = Fixture::new();
    let state = f.private_state();
    let room = LiveRoom::new(f.stored(state.clone()));
    let (mut inv, _) = invitation(&room, WEB_APP).unwrap();
    // A secret for a version the room has not reached is dropped.
    inv.room_secrets.push((5, [7; 32]));
    let plan = join(
        &inv,
        "Bea",
        &state,
        contract_key(&f.owner.verifying_key()),
        now(),
    )
    .unwrap();
    assert_eq!(plan.room.invitation_secrets, BTreeMap::from([(0, SECRET)]));
    let info = &plan.delta.as_ref().unwrap().member_info.as_ref().unwrap()[0];
    assert!(matches!(
        info.member_info.preferred_nickname,
        SealedBytes::Private {
            secret_version: 0,
            ..
        }
    ));
    // Readable with the room's secret: the members see the nickname.
    let view = LiveRoom::new(stored(&plan.room)).view(&minutes);
    assert_eq!(view.name, "Secret garden");
    assert!(
        view.members
            .iter()
            .any(|m| m.nickname == "Bea" && m.id == view.me)
    );
}

#[test]
fn a_private_room_with_no_secret_to_seal_with_gets_no_nickname_record() {
    let f = Fixture::new();
    let state = f.private_state();
    let room = LiveRoom::new(f.stored(state.clone()));
    let (mut inv, _) = invitation(&room, WEB_APP).unwrap();
    inv.room_secrets.clear();
    let plan = join(
        &inv,
        "Bea",
        &state,
        contract_key(&f.owner.verifying_key()),
        now(),
    )
    .unwrap();
    assert!(plan.delta.as_ref().unwrap().member_info.is_none());
    assert!(plan.room.self_member_info.is_none());
    // Still a member, with the join event in.
    assert!(plan.delta.as_ref().unwrap().members.is_some());
    assert_eq!(plan.room.self_nickname.as_deref(), Some("Bea"));
}

#[test]
fn a_room_that_would_not_take_the_invitee_refuses_the_join() {
    let owner = SigningKey::from_bytes(&[46; 32]);
    let key = contract_key(&owner.verifying_key());
    let (state, inv) = public_room_and_invitation(&owner);

    // Full.
    let mut full = state.clone();
    let mut config = full.configuration.configuration.clone();
    config.max_members = 0;
    config.configuration_version += 1;
    full.configuration = AuthorizedConfigurationV1::new(config, &owner);
    let Err(e) = join(&inv, "Bea", &full, key, now()) else {
        panic!("a full room refuses");
    };
    assert!(e.why.contains("full") && e.retry, "{e:?}");

    // Banned.
    let mut banned = state.clone();
    let ban = river_core::room_state::ban::AuthorizedUserBan::new(
        river_core::room_state::ban::UserBan {
            owner_member_id: MemberId::from(&owner.verifying_key()),
            banned_at: SystemTime::now(),
            banned_user: MemberId::from(&inv.invitee_signing_key.verifying_key()),
        },
        MemberId::from(&owner.verifying_key()),
        &owner,
    );
    banned.bans.0.push(ban);
    // Banned: final, nothing to try again.
    assert!(join(&inv, "Bea", &banned, key, now()).is_err_and(|e| !e.retry));

    // A room with another owner than the invitation names.
    let (other, _) = public_room_and_invitation(&SigningKey::from_bytes(&[47; 32]));
    assert!(join(&inv, "Bea", &other, key, now()).is_err());

    // A nickname over the room's limit, or none.
    let mut tight = state.clone();
    let mut config = tight.configuration.configuration.clone();
    config.max_nickname_size = 2;
    config.configuration_version += 1;
    tight.configuration = AuthorizedConfigurationV1::new(config, &owner);
    // Over the room's limit: final, and it says what to change.
    assert!(
        join(&inv, "Bea", &tight, key, now())
            .is_err_and(|e| !e.retry && e.why.contains("shorter nickname"))
    );
    assert!(join(&inv, "  ", &state, key, now()).is_err());
    assert!(join(&inv, "B\u{200B}ea", &state, key, now()).is_err());

    // And the room as it was takes them, as a new member.
    let plan = join(&inv, "Bea", &state, key, now()).unwrap();
    assert!(plan.adds_member());
}

#[test]
fn an_invitee_already_in_the_room_is_not_added_twice() {
    let f = Fixture::new();
    let state = f.private_state();
    // An invitation re-sent to `me`, who is already a member: the web UI's "Restore access".
    let owner_room = {
        let mut s = f.stored(state.clone());
        s.self_sk = Some(f.owner.clone());
        LiveRoom::new(s)
    };
    let (mut inv, _) = invitation(&owner_room, WEB_APP).unwrap();
    inv.invitee_signing_key = f.me.clone();
    inv.invitee = state.members.members[0].clone();
    let plan = join(
        &inv,
        "Mia",
        &state,
        contract_key(&f.owner.verifying_key()),
        now(),
    )
    .unwrap();
    // Nothing to send: no second join event, no version-0 nickname record replacing the one
    // the room has, and no invite chain re-added.
    assert!(plan.delta.is_none());
    assert!(!plan.adds_member(), "a join from a message refuses this");
    assert_eq!(
        plan.room.self_member_info.as_ref(),
        state
            .member_info
            .member_info
            .iter()
            .find(|i| i.member_info.member_id == f.me_id())
    );
}

#[test]
fn an_invitation_whose_key_is_not_its_membership_is_refused() {
    let owner = SigningKey::from_bytes(&[48; 32]);
    let (state, mut inv) = public_room_and_invitation(&owner);
    inv.invitee_signing_key = SigningKey::from_bytes(&[49; 32]);
    assert!(invitation_refusal(&inv).is_some());
    let e = join(
        &inv,
        "Bea",
        &state,
        contract_key(&owner.verifying_key()),
        now(),
    )
    .err()
    .unwrap();
    assert!(!e.retry && e.why.contains("damaged"), "{e:?}");
}

/// A secret blob addressed to the invitee that does not decrypt (damaged in the room) is
/// skipped, not fatal: the invitation's copy of the secret seals the nickname.
#[test]
fn a_blob_that_does_not_decrypt_falls_back_to_the_invitations_secret() {
    let f = Fixture::new();
    let mut state = f.private_state();
    let room = LiveRoom::new(f.stored(state.clone()));
    let (inv, _) = invitation(&room, WEB_APP).unwrap();
    // A blob for the invitee whose ciphertext is garbage.
    let me = MemberId::from(&inv.invitee_signing_key.verifying_key());
    let mut bad = state.secrets.encrypted_secrets[0].secret.clone();
    bad.member_id = me;
    bad.ciphertext = vec![1, 2, 3];
    // Signed by the owner, so the room itself takes it: only its ciphertext is damaged.
    state.secrets.encrypted_secrets.push(
        river_core::room_state::secret::AuthorizedEncryptedSecretForMember::new(bad, &f.owner),
    );
    assert!(current_secret_from_state(&state, &inv.invitee_signing_key).is_none());
    let plan = join(
        &inv,
        "Bea",
        &state,
        contract_key(&f.owner.verifying_key()),
        now(),
    )
    .expect("the join goes ahead");
    let delta = plan.delta.expect("something to send");
    let info = &delta.member_info.as_ref().expect("a nickname record")[0];
    let nickname = river_core::ecies::unseal_bytes_with_secrets(
        &info.member_info.preferred_nickname,
        &std::collections::HashMap::from([(0, SECRET)]),
    )
    .expect("sealed under the invitation's secret");
    assert_eq!(nickname, b"Bea");
}

#[test]
fn new_room_trims_what_was_typed() {
    let plan = new_room(
        SigningKey::from_bytes(&[50; 32]),
        "  Book club ",
        " Ada ",
        false,
    )
    .unwrap();
    // What the room stores, not how a view shows it (a view trims what it shows anyway).
    let state = &plan.room.room_state;
    assert_eq!(
        state.configuration.configuration.display.name,
        SealedBytes::public(b"Book club".to_vec())
    );
    assert_eq!(
        state.member_info.member_info[0]
            .member_info
            .preferred_nickname,
        SealedBytes::public(b"Ada".to_vec())
    );
}

// Pasted invitations --------------------------------------------------------------------------

#[test]
fn a_pasted_link_or_code_decodes_and_anything_else_is_refused_without_echoing_it() {
    let owner = SigningKey::from_bytes(&[50; 32]);
    let (_, inv) = public_room_and_invitation(&owner);
    let code = inv.to_encoded_string().unwrap();
    for pasted in [
        code.clone(),
        format!("  {code}\n"),
        format!("{WEB_APP}?invitation={code}"),
    ] {
        assert!(decode_invitation(&pasted).unwrap() == inv, "{pasted}");
    }
    // Garbage that is not base58, is base58 but not an invitation, or is a valid prefix cut
    // short: each is refused, none panics, none repeats the paste.
    let half = &code[..code.len() / 2];
    let secret = "SECRETSECRETSECRET";
    for bad in [
        "",
        "   ",
        "not a code!",
        "3mJr7AoUXx2Wqd",
        half,
        &format!("{WEB_APP}?invitation={half}"),
        &format!("0OIl{secret}"),
        &format!("{WEB_APP}?invitation=%FF%FE{secret}"),
    ] {
        let why = decode_invitation(bad).unwrap_err();
        assert!(!why.is_empty());
        assert!(!why.contains(secret) && !why.contains("3mJr7"), "{why}");
        if bad.trim().len() > 12 {
            assert!(!why.contains(bad.trim()), "{why}");
        }
    }
}

#[test]
fn a_paste_over_the_bound_is_refused_before_it_is_decoded() {
    let long = "z".repeat(MAX_INVITE_PASTE_LEN + 1);
    assert!(decode_invitation(&long).unwrap_err().contains("too long"));
    // At the bound, with the character base58 decodes slowest ("1" decodes for free), it is
    // decoded and refused for what it holds. Base58 decoding is quadratic, which is why the
    // bound is small. No timing is asserted: the decode completes.
    let at = "z".repeat(MAX_INVITE_PASTE_LEN);
    assert!(!decode_invitation(&at).unwrap_err().contains("too long"));
}

/// The bound leaves room for a private room that has rotated its secret 100 times (about 9.9 KB
/// as a code: each version's 32-byte secret costs about 90 characters).
#[test]
fn an_invitation_to_a_room_with_100_secret_versions_fits_the_bound() {
    let owner = SigningKey::from_bytes(&[53; 32]);
    let (_, mut inv) = public_room_and_invitation(&owner);
    inv.room_secrets = (0..100u32).map(|v| (v, [0xF0 ^ v as u8; 32])).collect();
    let code = inv.to_encoded_string().unwrap();
    assert!(code.len() < MAX_INVITE_PASTE_LEN, "{}", code.len());
    assert!(decode_invitation(&format!("{WEB_APP}?invitation={code}")).unwrap() == inv);
}

#[test]
fn an_invitation_whose_membership_is_for_another_room_is_refused() {
    let owner = SigningKey::from_bytes(&[51; 32]);
    let (_, mut inv) = public_room_and_invitation(&owner);
    // Names one room, grants membership in another.
    inv.room = SigningKey::from_bytes(&[52; 32]).verifying_key();
    let why = invitation_refusal(&inv).unwrap();
    assert!(why.contains("different room"), "{why}");
    assert!(decode_invitation(&inv.to_encoded_string().unwrap()).is_err());
}

#[test]
fn the_invitation_message_is_the_web_uis_and_cleans_the_room_name() {
    let msg = invitation_message("Book club", "http://x/?invitation=abc");
    assert!(
        msg.starts_with("You've been invited to join the chat room \"Book club\"!\n\nTo join:\n")
    );
    assert!(msg.contains("1. Install Freenet from https://freenet.org\n"));
    assert!(msg.contains("2. Open this link:\nhttp://x/?invitation=abc\n\n"));
    assert!(msg.contains("for you only"));
    // The owner's text: no control, line-separator, bidi, zero-width or other format
    // characters, bounded, and a stand-in when nothing is left.
    let odd = invitation_message("A\nB\u{7}C", "l");
    assert!(odd.contains("\"A BC\""), "{odd}");
    for hostile in [
        "Ro\u{202E}om",
        "Ro\u{2066}o\u{2069}m",
        "Ro\u{200B}o\u{FEFF}m",
        "Ro\u{AD}o\u{2060}m\u{E0041}",
    ] {
        assert_eq!(plain_room_name(hostile), "Room", "{hostile:?}");
        assert!(invitation_message(hostile, "l").contains("\"Room\""));
    }
    // The sidebar's own cleaning: line and paragraph separators and doubled spaces are single
    // spaces, and the joiners some scripts need (ZWNJ, ZWJ) stay.
    assert_eq!(
        plain_room_name("Book\u{2028}club\u{2029}  night"),
        "Book club night"
    );
    assert_eq!(plain_room_name("  a   b  "), "a b");
    assert_eq!(
        plain_room_name("\u{0645}\u{200C}\u{06CC}"),
        "\u{0645}\u{200C}\u{06CC}"
    );
    assert_eq!(plain_room_name("a\u{200D}b"), "a\u{200D}b");
    for hidden in ["\u{202E}\u{200B}", "\u{2028}", "   ", ""] {
        assert_eq!(plain_room_name(hidden), "");
        assert_eq!(room_name_or_standin(hidden), "this chat room");
        assert!(invitation_message(hidden, "l").contains("\"this chat room\""));
    }
    // The cut is on a character, not a byte, and a name of nothing visible gets the stand-in.
    assert_eq!(plain_room_name(&"\u{e9}".repeat(500)).chars().count(), 80);
    assert!(invitation_message(&"x".repeat(500), "l").len() < 700);
    assert!(invitation_message("  ", "l").contains("\"this chat room\""));
    assert!(invitation_message("\u{202E}\u{200B}", "l").contains("\"this chat room\""));
}

#[test]
fn a_generated_nickname_is_cut_to_the_limit_with_a_fixed_fallback() {
    let cfg = |n: usize| Configuration {
        max_nickname_size: n,
        ..Configuration::default()
    };
    assert_eq!(
        fit_nickname("Cipher Daylight", &cfg(50)).as_deref(),
        Some("Cipher Daylight")
    );
    assert_eq!(
        fit_nickname("Cipher Daylight", &cfg(7)).as_deref(),
        Some("Cipher")
    );
    // Never inside a character: a 2-byte character at a 3-byte limit leaves one.
    assert_eq!(
        fit_nickname("\u{e9}\u{e9}", &cfg(3)).as_deref(),
        Some("\u{e9}")
    );
    // Nothing left: "Member" when it fits, else nothing.
    assert_eq!(fit_nickname("\u{e9}x", &cfg(1)), None);
    assert_eq!(
        fit_nickname("       Bea", &cfg(6)).as_deref(),
        Some("Member")
    );
    assert_eq!(fit_nickname("x", &cfg(0)), None);
}
