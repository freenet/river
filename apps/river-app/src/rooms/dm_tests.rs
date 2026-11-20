//! Direct-message threads against messages built by river-core's own `compose_direct_message`,
//! the helper the web UI and `riverctl` build theirs with.

use ed25519_dalek::{SigningKey, VerifyingKey};
use river_core::ecies::seal_dm_for_recipient;
use river_core::room_state::ChatRoomStateV1;
use river_core::room_state::direct_messages::{
    AuthorizedDirectMessage, DirectMessagesV1, MAX_DM_CIPHERTEXT_BYTES, compose_direct_message,
    sign_direct_message,
};
use river_core::room_state::dm_body::{
    DM_BODY_MAGIC, DirectMessageBody, InvitePayload, encode_body,
};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId};

use super::dm::{
    DmKey, DmThreadsCache, MAX_THREAD_MESSAGES, MAX_THREADS, MAX_TOTAL_MESSAGES, Source, Threads,
    threads, token_hex,
};
use super::dm_cache::OutboundCache;
use crate::chat::dm_model::{DmBody, DmMessage, DmThread, Placeholder};

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

fn vk(n: u8) -> VerifyingKey {
    sk(n).verifying_key()
}

fn id(n: u8) -> MemberId {
    MemberId::from(&vk(n))
}

const OWNER: u8 = 1;
const ME: u8 = 2;
const PEER: u8 = 3;
const OTHER: u8 = 4;
/// Signed a message once and is no longer a member.
const GONE: u8 = 5;

const T0: u64 = 1_700_000_000;

/// A room with the owner, `ME`, `PEER` and `OTHER` as members.
struct Room {
    members: ChatRoomStateV1,
    dms: DirectMessagesV1,
    cache: OutboundCache,
    label: Option<String>,
}

impl Room {
    fn new() -> Self {
        let mut members = ChatRoomStateV1::default();
        for n in [ME, PEER, OTHER] {
            members.members.members.push(AuthorizedMember::new(
                Member {
                    owner_member_id: id(OWNER),
                    invited_by: id(OWNER),
                    member_vk: vk(n),
                },
                &sk(OWNER),
            ));
        }
        Room {
            members,
            dms: DirectMessagesV1::default(),
            cache: OutboundCache::default(),
            label: None,
        }
    }

    /// A message composed as the web UI composes plain text: the raw UTF-8 bytes.
    fn send_raw(&mut self, from: u8, to: u8, ts: u64, text: &str) -> AuthorizedDirectMessage {
        self.send_bytes(from, to, ts, text.as_bytes())
    }

    fn send_bytes(&mut self, from: u8, to: u8, ts: u64, body: &[u8]) -> AuthorizedDirectMessage {
        let m = compose_direct_message(&sk(from), &vk(to), &vk(OWNER), ts, ts, body).unwrap();
        self.dms.messages.push(m.clone());
        m
    }

    fn send_body(&mut self, from: u8, to: u8, ts: u64, body: &DirectMessageBody) {
        self.send_bytes(from, to, ts, &encode_body(body).unwrap());
    }

    /// The user's own sent text, remembered in the cache as the web UI remembers it.
    fn remember(&mut self, m: &AuthorizedDirectMessage, text: &str) {
        self.cache
            .insert_for_test(&vk(OWNER), m.message.recipient, m.purge_token(), text);
    }

    fn read(&self, me: u8, with_key: bool) -> Threads {
        let name_of = |m: MemberId| format!("name-{}", m);
        let label = |_: &VerifyingKey| self.label.clone();
        let me_sk = sk(me);
        let owner = vk(OWNER);
        threads(&Source {
            room_key: "roomkey",
            room_name: "Room",
            owner: &owner,
            members: &self.members.members,
            messages: &self.dms,
            self_sk: with_key.then_some(&me_sk),
            self_vk: Some(me_sk.verifying_key()),
            cache: &self.cache,
            name_of: &name_of,
            room_label: &label,
        })
    }

    fn mine(&self) -> Threads {
        self.read(ME, true)
    }
}

fn texts(t: &DmThread) -> Vec<String> {
    t.messages
        .iter()
        .map(|m| match &m.body {
            DmBody::Text(s) => s.clone(),
            other => format!("{other:?}"),
        })
        .collect()
}

fn thread_with(t: &Threads, peer: u8) -> &DmThread {
    t.threads
        .iter()
        .find(|th| th.peer_key == crate::rooms::dm::peer_key(id(peer)))
        .expect("a thread with that peer")
}

fn placeholder(body: &DmBody) -> Placeholder {
    match body {
        DmBody::Placeholder(p) => *p,
        other => panic!("not a placeholder: {other:?}"),
    }
}

// Interop vectors ------------------------------------------------------------------------

#[test]
fn raw_utf8_text_as_the_web_ui_sends_it_reads_as_text() {
    let mut room = Room::new();
    room.send_raw(PEER, ME, T0, "hello from the web ui");
    room.send_raw(PEER, ME, T0 + 1, "café, 日本語, 🙂");
    let t = room.mine();
    assert!(t.identity_known && !t.truncated);
    assert_eq!(t.threads.len(), 1);
    let th = &t.threads[0];
    assert_eq!(th.room, "roomkey");
    assert_eq!(th.room_name, "Room");
    assert_eq!(th.peer, id(PEER));
    assert_eq!(th.peer_name, format!("name-{}", id(PEER)));
    assert!(th.peer_is_member);
    assert_eq!(texts(th), ["hello from the web ui", "café, 日本語, 🙂"]);
    assert!(th.messages.iter().all(|m| !m.outgoing));
    assert_eq!((th.last_ts, th.last_inbound_ts), (T0 + 1, T0 + 1));
}

#[test]
fn cbor_text_bodies_read_as_text() {
    let mut room = Room::new();
    room.send_body(
        PEER,
        ME,
        T0,
        &DirectMessageBody::Text {
            text: "structured".into(),
        },
    );
    assert_eq!(texts(&room.mine().threads[0]), ["structured"]);
}

#[test]
fn legacy_bodies_decode_lossily_and_a_bad_structured_body_is_a_placeholder() {
    let mut room = Room::new();
    // Not UTF-8, and not the magic byte: the legacy path replaces what it cannot read.
    room.send_bytes(PEER, ME, T0, &[b'a', 0xff, b'b']);
    // The magic byte with nothing valid after it.
    room.send_bytes(PEER, ME, T0 + 1, &[DM_BODY_MAGIC, 0xff, 0xff]);
    // The magic byte alone.
    room.send_bytes(PEER, ME, T0 + 2, &[DM_BODY_MAGIC]);
    // Nothing at all.
    room.send_bytes(PEER, ME, T0 + 3, &[]);
    let t = room.mine();
    let m = &t.threads[0].messages;
    assert_eq!(m[0].body, DmBody::Text("a\u{fffd}b".into()));
    assert_eq!(placeholder(&m[1].body), Placeholder::CannotDecode);
    assert_eq!(placeholder(&m[2].body), Placeholder::CannotDecode);
    assert_eq!(m[3].body, DmBody::Text(String::new()));
}

#[test]
fn an_invitation_shows_its_room_and_its_note() {
    let mut room = Room::new();
    let payload = InvitePayload {
        room_owner_vk: vk(9),
        invitation_payload: b"BEARER-CREDENTIAL".to_vec(),
        personal_message: Some("  join us  ".into()),
    };
    room.send_body(
        PEER,
        ME,
        T0,
        &DirectMessageBody::Invite(Box::new(payload.clone())),
    );
    let t = room.mine();
    let body = &t.threads[0].messages[0].body;
    assert_eq!(
        *body,
        DmBody::Invite {
            room: bs58::encode(vk(9).as_bytes()).into_string(),
            room_label: format!(
                "Room {}",
                &bs58::encode(vk(9).as_bytes()).into_string()[..8]
            ),
            note: Some("join us".into()),
        }
    );
    // With the room known, its name is shown.
    room.label = Some("The pub".into());
    let t = room.mine();
    assert!(matches!(
        &t.threads[0].messages[0].body,
        DmBody::Invite { room_label, .. } if room_label == "The pub"
    ));
    // The body has a room label and a note and nowhere to carry the credential; neither holds it.
    let DmBody::Invite {
        room_label, note, ..
    } = &t.threads[0].messages[0].body
    else {
        panic!("an invitation");
    };
    assert!(!room_label.contains("BEARER") && !note.clone().unwrap_or_default().contains("BEARER"));
    // No note, or a blank one, is no note.
    let mut room = Room::new();
    for note in [None, Some("   ".to_string())] {
        room.send_body(
            PEER,
            ME,
            T0,
            &DirectMessageBody::Invite(Box::new(InvitePayload {
                personal_message: note,
                ..payload.clone()
            })),
        );
    }
    for m in &room.mine().threads[0].messages {
        assert!(matches!(&m.body, DmBody::Invite { note: None, .. }));
    }
}

// Failures are rows, never panics ----------------------------------------------------------

/// Sealed to someone else's key, but addressed to the user and signed properly.
fn sealed_to_wrong_key(room: &mut Room, ts: u64) {
    let envelope = seal_dm_for_recipient(&vk(OTHER), b"not for you");
    let m = sign_direct_message(&sk(PEER), id(PEER), id(ME), &vk(OWNER), ts, envelope).unwrap();
    room.dms.messages.push(m);
}

#[test]
fn a_message_sealed_to_another_key_is_a_placeholder() {
    let mut room = Room::new();
    sealed_to_wrong_key(&mut room, T0);
    let t = room.mine();
    assert_eq!(
        placeholder(&t.threads[0].messages[0].body),
        Placeholder::CannotDecrypt
    );
    assert!(!format!("{t:?}").contains("not for you"));
}

#[test]
fn a_message_that_does_not_verify_is_dropped_and_counted() {
    let mut room = Room::new();
    let good = room.send_raw(PEER, ME, T0, "genuine");
    // The ciphertext changed after signing.
    let mut bad_ct = good.clone();
    bad_ct.message.ciphertext[50] ^= 1;
    // The signature changed.
    let mut bad_sig = good.clone();
    let mut sig = bad_sig.sender_signature.to_bytes();
    sig[3] ^= 1;
    bad_sig.sender_signature = ed25519_dalek::Signature::from_bytes(&sig);
    // Re-signed by the real sender over altered ciphertext: the signature holds, the AEAD does not.
    let mut ct = good.message.ciphertext.clone();
    let last = ct.len() - 1;
    ct[last] ^= 1;
    let resigned =
        sign_direct_message(&sk(PEER), id(PEER), id(ME), &vk(OWNER), T0 + 3, ct).unwrap();
    // Signed by someone other than the named sender.
    let forged = sign_direct_message(
        &sk(OTHER),
        id(OTHER),
        id(ME),
        &vk(OWNER),
        T0 + 4,
        good.message.ciphertext.clone(),
    )
    .unwrap();
    let mut forged_as_peer = forged.clone();
    forged_as_peer.message.sender = id(PEER);
    room.dms
        .messages
        .extend([bad_ct, bad_sig, resigned, forged_as_peer]);
    let t = room.mine();
    let m = &t.threads[0].messages;
    assert_eq!(m.len(), 2, "the genuine one and the re-signed one");
    assert_eq!(m[0].body, DmBody::Text("genuine".into()));
    assert_eq!(placeholder(&m[1].body), Placeholder::CannotDecrypt);
    assert_eq!(t.unverifiable, 3);
}

#[test]
fn a_forged_row_from_me_makes_no_thread_row_time_or_count() {
    let mut room = Room::new();
    // A real thread with the peer, and a forged row claiming I sent something to OTHER, whose
    // thread therefore must not exist.
    let genuine = room.send_raw(ME, PEER, T0, "mine");
    room.remember(&genuine, "mine");
    let mut forged = room.send_raw(ME, OTHER, T0 + 500, "forged");
    forged.message.ciphertext[50] ^= 1;
    room.dms.messages.pop();
    room.dms.messages.push(forged.clone());
    // Many forged rows to the real peer: none is drawn as mine, orders anything, or uses up the
    // bound, and none moves the thread's time.
    for i in 0..(MAX_THREAD_MESSAGES + 50) {
        let mut f = genuine.clone();
        f.message.timestamp = T0 + 1000 + i as u64;
        room.dms.messages.push(f);
    }
    let t = room.mine();
    assert_eq!(t.threads.len(), 1, "no thread for the forged row's peer");
    let th = &t.threads[0];
    assert_eq!(th.peer, id(PEER));
    assert_eq!(texts(th), ["mine"]);
    assert_eq!((th.last_ts, th.last_inbound_ts), (T0, 0));
    assert!(!t.truncated);
    assert_eq!(t.unverifiable, MAX_THREAD_MESSAGES + 51);
}

#[test]
fn a_user_the_room_no_longer_lists_cannot_verify_their_own_messages() {
    // As the contract resolves keys: the owner or a current member. A pruned user's sends do
    // not verify, so they are not shown as sent.
    let mut room = Room::new();
    room.send_raw(ME, PEER, T0, "sent while a member");
    room.send_raw(PEER, ME, T0 + 1, "received");
    room.members
        .members
        .members
        .retain(|m| m.member.id() != id(ME));
    let t = room.mine();
    assert_eq!(t.unverifiable, 1);
    assert_eq!(texts(&t.threads[0]), ["received"]);
}

#[test]
fn the_read_position_time_ignores_the_bounds() {
    let mut room = Room::new();
    room.send_raw(PEER, ME, T0, "their last word");
    // More newer messages from me than a thread keeps.
    for i in 0..(MAX_THREAD_MESSAGES + 30) {
        room.send_raw(ME, PEER, T0 + 10 + i as u64, "mine");
    }
    let t = room.mine();
    let th = &t.threads[0];
    assert!(t.truncated);
    assert_eq!(th.messages.len(), MAX_THREAD_MESSAGES);
    assert!(th.messages.iter().all(|m| m.outgoing), "theirs was cut");
    assert_eq!(th.last_inbound_ts, T0);
}

#[test]
fn an_oversized_envelope_is_dropped_unopened() {
    let mut room = Room::new();
    let mut m = room.send_raw(PEER, ME, T0, "x");
    m.message.ciphertext = vec![7; MAX_DM_CIPHERTEXT_BYTES + 1];
    room.dms.messages[0] = m;
    let t = room.mine();
    assert!(t.threads.is_empty());
    assert_eq!(t.unverifiable, 1);
}

#[test]
fn without_a_signing_key_what_was_sent_to_the_user_cannot_be_opened() {
    let mut room = Room::new();
    room.send_raw(PEER, ME, T0, "to me");
    let sent = room.send_raw(ME, PEER, T0 + 1, "from me");
    room.remember(&sent, "from me");
    let t = room.read(ME, false);
    let m = &t.threads[0].messages;
    assert_eq!(placeholder(&m[0].body), Placeholder::NoKey);
    assert!(!m[0].outgoing);
    // The user's own words still come from the cache, and still on the right side.
    assert_eq!(m[1].body, DmBody::Text("from me".into()));
    assert!(m[1].outgoing);
}

#[test]
fn arbitrary_bytes_in_the_room_never_panic() {
    // A deterministic stream: no clock, no randomness.
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let lengths = [
        0,
        1,
        31,
        32,
        33,
        43,
        44,
        45,
        59,
        60,
        61,
        200,
        1000,
        MAX_DM_CIPHERTEXT_BYTES,
        MAX_DM_CIPHERTEXT_BYTES + 1,
    ];
    let mut room = Room::new();
    let mut ts = T0;
    let mut all: Vec<usize> = lengths.to_vec();
    all.extend((0..40).map(|_| (next() % 300) as usize));
    for len in all {
        let junk: Vec<u8> = (0..len).map(|_| next() as u8).collect();
        ts += 1;
        // Properly signed by a member, so the envelope is reached.
        if let Ok(m) =
            sign_direct_message(&sk(PEER), id(PEER), id(ME), &vk(OWNER), ts, junk.clone())
        {
            room.dms.messages.push(m);
        }
        // Not signed at all, in both directions.
        for (from, to) in [(PEER, ME), (ME, PEER), (GONE, ME)] {
            let mut m =
                sign_direct_message(&sk(from), id(from), id(to), &vk(OWNER), ts, vec![0]).unwrap();
            m.message.ciphertext = junk.clone();
            room.dms.messages.push(m);
        }
    }
    // Plaintexts that really are sealed to the user, so the envelope opens and the body is
    // decoded: random, structured, and hostile CBOR.
    let sealed = |room: &mut Room, ts: u64, plain: &[u8]| {
        let envelope = seal_dm_for_recipient(&vk(ME), plain);
        if let Ok(m) = sign_direct_message(&sk(PEER), id(PEER), id(ME), &vk(OWNER), ts, envelope) {
            room.dms.messages.push(m);
        }
    };
    let mut plains: Vec<Vec<u8>> = Vec::new();
    for len in [0usize, 1, 2, 50, 500, 5000] {
        plains.push((0..len).map(|_| next() as u8).collect());
        let mut magic = vec![DM_BODY_MAGIC];
        magic.extend((0..len).map(|_| next() as u8));
        plains.push(magic);
    }
    // The magic byte, then: deep nesting, huge declared lengths, truncated structures.
    plains.push([vec![DM_BODY_MAGIC], vec![0x81; 20_000]].concat());
    plains.push(
        [
            vec![DM_BODY_MAGIC],
            vec![0xa1, 0x64],
            b"Text".to_vec(),
            vec![0xa1, 0x64],
            b"text".to_vec(),
            vec![0x7b],
            vec![0xff; 8],
        ]
        .concat(),
    );
    plains.push(
        [
            vec![DM_BODY_MAGIC],
            vec![0xa1, 0x66],
            b"Invite".to_vec(),
            vec![0x9b],
            vec![0xff; 8],
        ]
        .concat(),
    );
    plains.push(
        [
            vec![DM_BODY_MAGIC],
            vec![0xa1, 0x66],
            b"Invite".to_vec(),
            vec![0xa3, 0x6d],
            b"room_owner_vk".to_vec(),
            vec![0x5b],
            vec![0xff; 8],
        ]
        .concat(),
    );
    plains.push(vec![DM_BODY_MAGIC, 0xa1]);
    plains.push(vec![DM_BODY_MAGIC, 0xbf, 0xbf]);
    // Largest the envelope allows.
    plains.push(vec![
        b'a';
        MAX_DM_CIPHERTEXT_BYTES
            - river_core::ecies::DM_ENVELOPE_OVERHEAD_BYTES
    ]);
    for plain in &plains {
        ts += 1;
        sealed(&mut room, ts, plain);
    }
    let opened_before = room.dms.messages.len();
    let t = room.mine();
    assert!(t.identity_known);
    let rows: Vec<_> = t.threads.iter().flat_map(|t| &t.messages).collect();
    for m in &rows {
        assert!(matches!(
            m.body,
            DmBody::Placeholder(_) | DmBody::Text(_) | DmBody::Invite { .. }
        ));
    }
    // The sealed ones were reached: they opened (as text or a placeholder for a bad body), not
    // all failing at the envelope.
    let opened = rows
        .iter()
        .filter(|m| !matches!(m.body, DmBody::Placeholder(Placeholder::CannotDecrypt)))
        .count();
    assert!(
        opened >= plains.len().min(MAX_THREAD_MESSAGES) / 2,
        "opened {opened}"
    );
    assert!(opened_before > plains.len());
    // And with the signing key absent, and with no identity.
    let _ = room.read(ME, false);
    let _ = room.read(99, false);
}

// Frozen interop vectors -------------------------------------------------------------------

fn from_hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// Envelopes sealed to `sk(ME)` by a separate implementation of the documented wire format (a
/// Python script using `cryptography`: ephemeral X25519 key, SHA-256 of the shared secret,
/// AES-256-GCM, `ephemeral_pub(32) || nonce(12) || ciphertext+tag`), not by river-core. They must
/// open to known text. They are not captured from the web UI or riverctl, which this checkout
/// has no way to run here; they pin the format and the key conversion from the outside.
/// Regenerate with `tests/make_dm_interop_vectors.py`.
const PYTHON_RAW: &str = "7b4e909bbe7ffe44c465a220037d608ee35897d31ef972f07f74892cb0f73f13212121212121212121212121c5f7c4575aaccf6eb67866feec618b93f38f181e977c020c4232641a3c915eea5b11408d4ff9e0a914f1c080bdd4a828671f3ea00e0a108b";
const PYTHON_CBOR: &str = "052a50773ac8d91773f2dc9662e12f0defe915e415b8a1c8e20a5a3d6ab2b84322222222222222222222222225f2f5684961ba0a8c35fffec975fea158d58b0f4c3afead7e423d533718deeb0bd9082c4cd01fa7416168b94aeb";

#[test]
fn envelopes_from_an_independent_implementation_open_to_known_text() {
    // At the lowest layer.
    let raw = river_core::ecies::unseal_dm_from_sender(&sk(ME), &from_hex(PYTHON_RAW)).unwrap();
    assert_eq!(raw, b"hello from an independent implementation");
    // Through the thread builder, signed as a peer would.
    let mut room = Room::new();
    for (ts, hex) in [(T0, PYTHON_RAW), (T0 + 1, PYTHON_CBOR)] {
        let m = sign_direct_message(&sk(PEER), id(PEER), id(ME), &vk(OWNER), ts, from_hex(hex))
            .unwrap();
        room.dms.messages.push(m);
    }
    assert_eq!(
        texts(&room.mine().threads[0]),
        [
            "hello from an independent implementation",
            "structured hello"
        ]
    );
}

// Not recomputing every frame ----------------------------------------------------------------

#[test]
fn an_unchanged_room_is_not_rebuilt_and_a_changed_one_is() {
    let mut room = Room::new();
    room.send_raw(PEER, ME, T0, "a");
    let mut memo = DmThreadsCache::default();
    let me_sk = sk(ME);
    let owner = vk(OWNER);
    let build = |room: &Room, names: u64, memo: &mut DmThreadsCache| -> Threads {
        let name_of = |m: MemberId| format!("name-{m}");
        let label = |_: &VerifyingKey| None;
        let src = Source {
            room_key: "roomkey",
            room_name: "Room",
            owner: &owner,
            members: &room.members.members,
            messages: &room.dms,
            self_sk: Some(&me_sk),
            self_vk: Some(me_sk.verifying_key()),
            cache: &room.cache,
            name_of: &name_of,
            room_label: &label,
        };
        memo.get(DmKey::of(&src, names), || threads(&src)).clone()
    };
    let first = build(&room, 0, &mut memo);
    for _ in 0..5 {
        assert_eq!(build(&room, 0, &mut memo), first);
    }
    assert_eq!(memo.builds(), 1, "unchanged input is not rebuilt");
    // A new message, new names, a changed cache, a changed member list: each rebuilds.
    room.send_raw(PEER, ME, T0 + 1, "b");
    assert_eq!(texts(&build(&room, 0, &mut memo).threads[0]), ["a", "b"]);
    assert_eq!(memo.builds(), 2);
    build(&room, 1, &mut memo);
    assert_eq!(memo.builds(), 3);
    let m = room.send_raw(ME, PEER, T0 + 2, "mine");
    build(&room, 1, &mut memo);
    room.remember(&m, "mine");
    let after = build(&room, 1, &mut memo);
    assert_eq!(memo.builds(), 5);
    assert_eq!(texts(&after.threads[0]), ["a", "b", "mine"]);
    room.members
        .members
        .members
        .retain(|m| m.member.id() != id(OTHER));
    build(&room, 1, &mut memo);
    assert_eq!(memo.builds(), 6);
}

// Who is in a thread ----------------------------------------------------------------------

#[test]
fn a_message_to_a_third_party_is_in_none_of_my_threads() {
    let mut room = Room::new();
    room.send_raw(PEER, OTHER, T0, "secret between them");
    room.send_raw(OTHER, PEER, T0 + 1, "and back");
    room.send_raw(OWNER, OTHER, T0 + 2, "owner to other");
    room.send_raw(PEER, ME, T0 + 3, "to me");
    let t = room.mine();
    assert_eq!(t.threads.len(), 1);
    assert_eq!(texts(&t.threads[0]), ["to me"]);
    let shown = format!("{t:?}");
    assert!(!shown.contains("secret between them"));
    // From their side, the same room has one thread, with the other person.
    let theirs = room.read(PEER, true);
    assert_eq!(theirs.threads.len(), 2);
    assert!(
        theirs
            .threads
            .iter()
            .all(|t| t.peer_key != crate::rooms::dm::peer_key(id(PEER)))
    );
}

#[test]
fn with_no_known_identity_there_are_no_threads_and_nothing_is_labelled() {
    let mut room = Room::new();
    room.send_raw(PEER, ME, T0, "hi");
    let label = |_: &VerifyingKey| None;
    let owner = vk(OWNER);
    let t = threads(&Source {
        room_key: "k",
        room_name: "r",
        owner: &owner,
        members: &room.members.members,
        messages: &room.dms,
        self_sk: None,
        self_vk: None,
        cache: &room.cache,
        name_of: &|_| String::new(),
        room_label: &label,
    });
    assert!(!t.identity_known);
    assert!(t.threads.is_empty());
}

#[test]
fn a_message_from_a_user_to_themselves_is_in_no_thread() {
    let mut room = Room::new();
    // The contract refuses it, so build it by hand: the key and recipient edited afterwards.
    let mut m = room.send_raw(ME, OTHER, T0, "to myself");
    m.message.recipient = id(ME);
    room.dms.messages.clear();
    room.dms.messages.push(m);
    assert!(room.mine().threads.is_empty());
    assert!(room.mine().identity_known);
}

#[test]
fn the_owner_is_a_peer_and_the_owner_can_be_me() {
    let mut room = Room::new();
    room.send_raw(OWNER, ME, T0, "welcome");
    let sent = room.send_raw(ME, OWNER, T0 + 1, "thanks");
    room.remember(&sent, "thanks");
    let t = room.mine();
    let th = thread_with(&t, OWNER);
    assert!(
        th.peer_is_member,
        "the owner is not in the member list but is a member"
    );
    assert_eq!(texts(th), ["welcome", "thanks"]);
    assert_eq!(
        th.messages.iter().map(|m| m.outgoing).collect::<Vec<_>>(),
        [false, true]
    );

    // The owner's own view: they are the sender of "welcome".
    let t = room.read(OWNER, true);
    let th = thread_with(&t, ME);
    assert_eq!(
        th.messages.iter().map(|m| m.outgoing).collect::<Vec<_>>(),
        [true, false]
    );
    // What the owner sent has no cache copy here: ciphertext only. What they were sent opens.
    assert_eq!(
        placeholder(&th.messages[0].body),
        Placeholder::CiphertextOnly
    );
    assert_eq!(th.messages[1].body, DmBody::Text("thanks".into()));
}

#[test]
fn a_member_who_has_left_cannot_be_verified_and_is_not_trusted() {
    let mut room = Room::new();
    room.send_raw(GONE, ME, T0, "from a banned member");
    room.send_raw(PEER, ME, T0 + 1, "from a member");
    let t = room.mine();
    assert_eq!(t.threads.len(), 1);
    assert_eq!(t.unverifiable, 1);
    assert_eq!(texts(thread_with(&t, PEER)), ["from a member"]);
    assert!(!format!("{:?}", t.threads[0].messages).contains("banned member"));
}

#[test]
fn threads_group_per_peer_and_sort_by_time() {
    let mut room = Room::new();
    room.send_raw(PEER, ME, T0 + 5, "peer second");
    room.send_raw(OTHER, ME, T0 + 9, "other only");
    room.send_raw(PEER, ME, T0 + 1, "peer first");
    let mine = room.send_raw(ME, PEER, T0 + 3, "my reply");
    room.remember(&mine, "my reply");
    let t = room.mine();
    // Newest thread first.
    assert_eq!(
        t.threads
            .iter()
            .map(|t| t.peer_key.clone())
            .collect::<Vec<_>>(),
        [
            crate::rooms::dm::peer_key(id(OTHER)),
            crate::rooms::dm::peer_key(id(PEER))
        ]
    );
    let th = thread_with(&t, PEER);
    assert_eq!(texts(th), ["peer first", "my reply", "peer second"]);
    assert_eq!(
        th.messages.iter().map(|m| m.timestamp).collect::<Vec<_>>(),
        [T0 + 1, T0 + 3, T0 + 5]
    );
    assert_eq!((th.last_ts, th.last_inbound_ts), (T0 + 5, T0 + 5));
    // A thread where the last word is mine has an older inbound time.
    room.send_raw(ME, PEER, T0 + 20, "last word");
    let t = room.mine();
    let th = thread_with(&t, PEER);
    assert_eq!((th.last_ts, th.last_inbound_ts), (T0 + 20, T0 + 5));
}

#[test]
fn a_message_in_the_room_twice_shows_once() {
    let mut room = Room::new();
    let m = room.send_raw(PEER, ME, T0, "once");
    room.dms.messages.push(m);
    assert_eq!(texts(&room.mine().threads[0]), ["once"]);
}

// What I sent -----------------------------------------------------------------------------

#[test]
fn what_i_sent_comes_from_the_cache_by_recipient_and_token() {
    let mut room = Room::new();
    let a = room.send_raw(ME, PEER, T0, "cached");
    let b = room.send_raw(ME, PEER, T0 + 1, "never cached");
    room.remember(&a, "cached");
    // Another recipient's entry with the same token is not this message's.
    room.cache
        .insert_for_test(&vk(OWNER), id(OTHER), b.purge_token(), "wrong recipient");
    // The same token in another room is not this room's.
    room.cache
        .insert_for_test(&vk(77), id(PEER), b.purge_token(), "wrong room");
    let t = room.mine();
    let m = &t.threads[0].messages;
    assert_eq!(m[0].body, DmBody::Text("cached".into()));
    assert_eq!(placeholder(&m[1].body), Placeholder::CiphertextOnly);
    assert_eq!(m[0].key, token_hex(&a.purge_token()));
    assert_eq!(m[0].key.len(), 32);
    assert!(m.iter().all(|m| m.outgoing));
    assert_eq!(placeholder(&m[1].body).text(), "sent - ciphertext only");
}

#[test]
fn a_cached_invitation_summary_reads_as_a_sent_invitation() {
    let mut room = Room::new();
    let a = room.send_raw(ME, PEER, T0, "x");
    let b = room.send_raw(ME, PEER, T0 + 1, "y");
    room.remember(&a, "[Invitation] come along");
    room.remember(&b, "[Invitation]");
    let t = room.mine();
    let m = &t.threads[0].messages;
    assert_eq!(
        m[0].body,
        DmBody::SentInvite {
            note: Some("come along".into())
        }
    );
    assert_eq!(m[1].body, DmBody::SentInvite { note: None });
}

// Bounds ----------------------------------------------------------------------------------

#[test]
fn a_thread_is_bounded_to_its_newest_messages() {
    let mut room = Room::new();
    let n = MAX_THREAD_MESSAGES + 20;
    for i in 0..n {
        room.send_raw(PEER, ME, T0 + i as u64, &format!("m{i}"));
    }
    let t = room.mine();
    assert!(t.truncated);
    let th = &t.threads[0];
    let expected: Vec<String> = (20..n).map(|i| format!("m{i}")).collect();
    assert_eq!(
        texts(th),
        expected,
        "exactly the newest are kept, oldest first"
    );
}

#[test]
fn the_number_of_threads_and_of_messages_opened_is_bounded() {
    let mut room = Room::new();
    // More peers than threads shown; peer i's one message is at T0 + i, so the newest are the
    // highest i.
    let total = MAX_THREADS as u8 + 20;
    for i in 0..total {
        room.send_raw(ME, 100 + i, T0 + i as u64, "hi");
    }
    let t = room.mine();
    assert!(t.truncated);
    let kept: Vec<MemberId> = t.threads.iter().map(|t| t.peer).collect();
    let expected: Vec<MemberId> = (20..total).rev().map(|i| id(100 + i)).collect();
    assert_eq!(kept, expected, "the newest threads, newest first");

    // Few peers, many messages each, peer p's at T0 + p * 1000 + i: the total opened is bounded,
    // spent on the newest threads first.
    let mut room = Room::new();
    let peers = MAX_TOTAL_MESSAGES / MAX_THREAD_MESSAGES + 2;
    for p in 0..peers as u8 {
        for i in 0..MAX_THREAD_MESSAGES {
            room.send_raw(ME, 100 + p, T0 + p as u64 * 1000 + i as u64, "hi");
        }
    }
    let t = room.mine();
    assert!(t.truncated);
    let full = MAX_TOTAL_MESSAGES / MAX_THREAD_MESSAGES;
    let kept: Vec<MemberId> = t.threads.iter().map(|t| t.peer).collect();
    let expected: Vec<MemberId> = (peers - full..peers)
        .rev()
        .map(|p| id(100 + p as u8))
        .collect();
    assert_eq!(kept, expected);
    assert!(
        t.threads
            .iter()
            .all(|t| t.messages.len() == MAX_THREAD_MESSAGES)
    );
    // Funds spent, an older thread gets nothing.
    let mut room = Room::new();
    for p in 0..full as u8 {
        for i in 0..MAX_THREAD_MESSAGES {
            room.send_raw(ME, 100 + p, T0 + p as u64 * 1000 + i as u64, "hi");
        }
    }
    room.send_raw(ME, 150, T0 + 10, "old");
    let t = room.mine();
    assert!(
        t.threads.iter().all(|t| t.peer != id(150)),
        "nothing left for the oldest"
    );
}

// Secrets ---------------------------------------------------------------------------------

#[test]
fn nothing_prints_what_was_written() {
    let mut room = Room::new();
    room.send_raw(PEER, ME, T0, "WORDS-FROM-PEER");
    let mine = room.send_raw(ME, PEER, T0 + 1, "x");
    room.remember(&mine, "WORDS-FROM-ME");
    let t = room.mine();
    // The rows really hold the words, so the checks below are not vacuous.
    assert!(texts(&t.threads[0]).contains(&"WORDS-FROM-PEER".to_string()));
    assert!(texts(&t.threads[0]).contains(&"WORDS-FROM-ME".to_string()));
    let mut shown = format!("{t:?} {:?} {:?}", t.threads[0], t.threads[0].messages);
    for m in &t.threads[0].messages {
        shown.push_str(&format!("{m:?} {:?}", m.body));
    }
    shown.push_str(&format!("{:?}", room.cache));
    assert!(!shown.contains("WORDS"), "{shown}");
}

#[test]
fn a_body_does_not_print_its_text_however_it_is_formatted() {
    for body in [
        DmBody::Text("SECRET-BODY".into()),
        DmBody::Invite {
            room: "SECRET-ROOM".into(),
            room_label: "SECRET-LABEL".into(),
            note: Some("SECRET-NOTE".into()),
        },
        DmBody::SentInvite {
            note: Some("SECRET-NOTE".into()),
        },
    ] {
        let message = DmMessage {
            key: "k".into(),
            outgoing: false,
            timestamp: 1,
            body: body.clone(),
            pending: None,
        };
        for shown in [
            format!("{body:?}"),
            format!("{body:#?}"),
            format!("{message:?}"),
            format!("{message:#?}"),
        ] {
            assert!(!shown.contains("SECRET"), "{shown}");
        }
    }
}

/// normalised.
fn source(path: &str) -> String {
    let text = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/rooms")
            .join(path),
    )
    .unwrap()
    .replace("\r\n", "\n");
    match text.find("#[cfg(test)]\nmod tests") {
        Some(i) => text[..i].to_string(),
        None => text,
    }
}

/// Code lines only: comments and doc lines dropped.
fn code(path: &str) -> String {
    source(path)
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_direct_message_code_neither_logs_nor_talks_to_the_node() {
    for file in ["dm.rs", "dm_cache.rs"] {
        let code = code(file);
        for banned in [
            "println!",
            "eprintln!",
            "print!(",
            "dbg!",
            "tracing::",
            "log::",
            "ClientRequest",
            "super::delegate",
            "crate::rooms::delegate",
            "super::socket",
            "super::session",
            "CasStore",
            "StoreRequest",
            "Versioned::Store",
            "NodeSocket",
        ] {
            assert!(!code.contains(banned), "{file} contains {banned}");
        }
    }
}

#[test]
fn reading_twice_gives_the_same_threads() {
    let mut room = Room::new();
    room.send_raw(PEER, ME, T0, "a");
    room.send_raw(OTHER, ME, T0, "b");
    let first = room.mine();
    for _ in 0..3 {
        assert_eq!(room.mine(), first);
    }
}

#[test]
fn renaming_another_room_rebuilds_when_the_callers_names_value_changes() {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut room = Room::new();
    let invited_to = vk(9);
    room.send_body(
        PEER,
        ME,
        T0,
        &DirectMessageBody::Invite(Box::new(InvitePayload {
            room_owner_vk: invited_to,
            invitation_payload: vec![],
            personal_message: None,
        })),
    );
    // A caller that knows other rooms by name, and hashes them (and this room's nicknames) into
    // the `names` value it passes.
    let others = std::cell::RefCell::new(String::from("Pub"));
    let names_hash = || {
        let mut h = DefaultHasher::new();
        others.borrow().hash(&mut h);
        h.finish()
    };
    let me_sk = sk(ME);
    let owner = vk(OWNER);
    let mut memo = DmThreadsCache::default();
    let label_of_invite = |room: &Room, memo: &mut DmThreadsCache| -> String {
        let name_of = |m: MemberId| format!("name-{m}");
        let label = |k: &VerifyingKey| (*k == invited_to).then(|| others.borrow().clone());
        let src = Source {
            room_key: "roomkey",
            room_name: "Room",
            owner: &owner,
            members: &room.members.members,
            messages: &room.dms,
            self_sk: Some(&me_sk),
            self_vk: Some(me_sk.verifying_key()),
            cache: &room.cache,
            name_of: &name_of,
            room_label: &label,
        };
        let t = memo.get(DmKey::of(&src, names_hash()), || threads(&src));
        match &t.threads[0].messages[0].body {
            DmBody::Invite { room_label, .. } => room_label.clone(),
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(label_of_invite(&room, &mut memo), "Pub");
    assert_eq!(label_of_invite(&room, &mut memo), "Pub");
    assert_eq!(memo.builds(), 1);
    *others.borrow_mut() = "Tavern".into();
    assert_eq!(label_of_invite(&room, &mut memo), "Tavern");
    assert_eq!(memo.builds(), 2);
}

#[test]
fn the_key_does_not_confuse_where_the_room_key_ends_and_its_name_begins() {
    let room = Room::new();
    let owner = vk(OWNER);
    let me_sk = sk(ME);
    let key = |room_key: &str, room_name: &str| {
        let name_of = |_: MemberId| String::new();
        let label = |_: &VerifyingKey| None;
        DmKey::of(
            &Source {
                room_key,
                room_name,
                owner: &owner,
                members: &room.members.members,
                messages: &room.dms,
                self_sk: Some(&me_sk),
                self_vk: None,
                cache: &room.cache,
                name_of: &name_of,
                room_label: &label,
            },
            0,
        )
    };
    assert_ne!(key("ab", "c"), key("a", "bc"));
}

#[test]
fn messages_in_one_second_are_ordered_by_purge_token_whatever_order_the_room_holds_them() {
    let mut room = Room::new();
    let first = room.send_raw(PEER, ME, T0, "x");
    let second = room.send_raw(PEER, ME, T0, "y");
    let third = room.send_raw(ME, PEER, T0, "z");
    let mut want = vec![
        token_hex(&first.purge_token()),
        token_hex(&second.purge_token()),
        token_hex(&third.purge_token()),
    ];
    want.sort();
    for order in [[0, 1, 2], [2, 1, 0], [1, 2, 0]] {
        let held = room.dms.messages.clone();
        room.dms.messages = order.iter().map(|i| held[*i].clone()).collect();
        let t = room.mine();
        let keys: Vec<String> = thread_with(&t, PEER)
            .messages
            .iter()
            .map(|m| m.key.clone())
            .collect();
        assert_eq!(keys, want, "held as {order:?}");
        room.dms.messages = held;
    }
}
