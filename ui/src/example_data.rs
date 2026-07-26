use crate::util::random_full_name;
use crate::{
    constants::ROOM_CONTRACT_WASM,
    room_data::{RoomData, Rooms},
    util::to_cbor_vec,
};
use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_scaffold::util::fast_hash;
use freenet_scaffold::ComposableState;
use freenet_stdlib::prelude::{ContractCode, ContractInstanceId, ContractKey, Parameters};
use lipsum::lipsum;
use rand::rngs::OsRng;
use river_core::room_state::ChatRoomParametersV1;
use river_core::{
    room_state::{
        configuration::*,
        member::*,
        member_info::*,
        message::*,
        privacy::{RoomDisplayMetadata, SealedBytes},
    },
    ChatRoomStateV1,
};
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How many alternating-author filler messages the deep-history fixture seeds.
///
/// The point is to comfortably exceed `INITIAL_WINDOW_ITEMS` (60) in DISPLAY
/// ITEMS so the windowed-tail rendering (freenet/river#498) is actually active
/// under Playwright — the standard fixture's ~15-20 items leave the window, the
/// backfill sentinel and the restore path all unreachable, which is how #501
/// shipped with a green suite. Strictly ALTERNATING authors, because
/// consecutive same-author messages within five minutes fold into one display
/// item: alternation makes messages == items, so the count is a guarantee
/// rather than a hope.
///
/// Sized to more than THREE windows (188 + the 12 standard messages = 200), so
/// the backfill-paging spec takes several growth steps to exhaust the room and
/// the #505 blocker-2 scenario (arrivals widening the rendered window past one
/// whole growth step) is reachable with a single batched delivery. The room's
/// `max_recent_messages` is raised to [`DEEP_ROOM_MAX_RECENT_MESSAGES`] so
/// those deliveries do not prune the fillers out from under the test.
const DEEP_HISTORY_FILLER_MESSAGES: usize = 188;

/// `max_recent_messages` for the deep fixture room: 200 seeded + headroom for
/// the specs' delivered arrivals without hitting the at-cap prune path (which
/// has its own dedicated fixture room below).
const DEEP_ROOM_MAX_RECENT_MESSAGES: usize = 300;

/// Fillers for the AT-CAP fixture room: 148 + the 13 standard messages lands
/// exactly on that room's `max_recent_messages`
/// ([`AT_CAP_MAX_RECENT_MESSAGES`]), so every delivered arrival drains the
/// oldest message — the steady state of every busy production room, and the
/// index-shift regression surface of #505 blocker 1.
///
/// These fillers come in same-author PAIRS (AABB…), NOT alternating: paired
/// consecutive messages fold into MULTI-message display groups, which is the
/// dominant production message shape and the one the #505 re-review blocker
/// lives in — draining the first message of a multi-message head group
/// RE-KEYS the group (a group's key is its first message's id). A strictly
/// alternating fixture is blind to that by construction. 148 paired fillers =
/// 74 display items, still comfortably past the 60-item window;
/// `at_cap_room_is_exactly_at_its_message_cap` pins all of it.
const AT_CAP_FILLER_MESSAGES: usize = 148;

/// `max_recent_messages` for the at-cap room. Raised from the default 100 so
/// the room can hold enough PAIRED fillers to exceed the render window in
/// display items (pairs halve the item count) while sitting exactly at cap.
///
/// Tracks the seeded total exactly: [`AT_CAP_FILLER_MESSAGES`] plus the
/// standard block. It went 160 -> 161 when #489's guaranteed impostor message
/// was added to that block — `at_cap_room_is_exactly_at_its_message_cap` is
/// what catches the drift, so bump this rather than shrinking the fillers
/// (they must stay an EVEN count to keep the same-author pairing).
const AT_CAP_MAX_RECENT_MESSAGES: usize = 161;

/// How deep a fixture room's message history is seeded.
#[derive(Debug, Clone, Copy, PartialEq)]
enum HistoryDepth {
    /// The standard ~12-message fixture. Fits inside the render window, so
    /// these rooms exercise the pre-window code path exactly as before.
    Standard,
    /// [`DEEP_HISTORY_FILLER_MESSAGES`] alternating-author fillers ahead of the
    /// standard fixture, so the room renders a windowed tail with a live
    /// backfill sentinel, with prune headroom for delivered arrivals.
    Deep,
    /// [`AT_CAP_FILLER_MESSAGES`] fillers, landing the room EXACTLY on its
    /// `max_recent_messages` cap so arrivals prune.
    AtCap,
}

/// Whether the page was loaded with `?deep-history-room=1`, asking for the
/// extra >window fixture room. Query-gated so the default fixture (and every
/// existing spec's timing) is untouched; only the windowing specs opt in.
#[cfg(target_arch = "wasm32")]
fn deep_history_room_requested() -> bool {
    web_sys::window()
        .and_then(|w| w.location().search().ok())
        .is_some_and(|search| search.contains("deep-history-room"))
}

#[cfg(not(target_arch = "wasm32"))]
fn deep_history_room_requested() -> bool {
    false
}

pub fn create_example_rooms() -> Rooms {
    let mut map = HashMap::new();

    // Room where you're just an observer (not a member). Description includes links
    // so the room header link-rendering path is exercised in example/playwright builds.
    let room1 = create_room(
        &"Public Discussion Room".to_string(),
        SelfIs::Observer,
        Some("Welcome | [Website](https://freenet.org/) · [Docs](https://docs.freenet.org/)"),
        HistoryDepth::Standard,
    );
    map.insert(room1.owner_vk, room1.room_data);

    // Room where you're a member
    let room2 = create_room(
        &"Team Chat Room".to_string(),
        SelfIs::Member,
        None,
        HistoryDepth::Standard,
    );
    map.insert(room2.owner_vk, room2.room_data);

    // Room where you're the owner
    let room3 = create_room(
        &"Your Private Room".to_string(),
        SelfIs::Owner,
        None,
        HistoryDepth::Standard,
    );
    map.insert(room3.owner_vk, room3.room_data);

    // Rooms deeper than the render window, for the windowing specs (#501):
    // one with prune headroom, one exactly at its message cap so delivered
    // arrivals exercise the at-cap index-shift path (#505 blocker 1).
    if deep_history_room_requested() {
        let room4 = create_room(
            &"Deep History Room".to_string(),
            SelfIs::Member,
            None,
            HistoryDepth::Deep,
        );
        map.insert(room4.owner_vk, room4.room_data);

        let room5 = create_room(
            &"Capped History Room".to_string(),
            SelfIs::Member,
            None,
            HistoryDepth::AtCap,
        );
        map.insert(room5.owner_vk, room5.room_data);
    }

    Rooms {
        map,
        current_room_key: None,
        removed_rooms: std::collections::HashSet::new(),
        notification_modes: Default::default(),
        migrated_rooms: Vec::new(),
        room_order: Vec::new(),
    }
}

struct CreatedRoom {
    owner_vk: VerifyingKey,
    room_data: RoomData,
}

#[derive(Debug, PartialEq)]
enum SelfIs {
    Observer,
    Member,
    Owner,
}

// Function to create a room with an owner and members, self_is determines whether
// the user of the UI is the owner, a member, or an observer (not an owner or member)
fn create_room(
    room_name: &String,
    self_is: SelfIs,
    description: Option<&str>,
    history_depth: HistoryDepth,
) -> CreatedRoom {
    let mut csprng = OsRng;

    // Create self - the user actually using the app
    let self_sk = SigningKey::generate(&mut csprng);
    let self_vk = self_sk.verifying_key();
    let self_id = self_vk.into();

    // Create owner of the room
    let owner_sk = if self_is == SelfIs::Owner {
        &self_sk
    } else {
        &SigningKey::generate(&mut csprng)
    };
    let owner_vk = owner_sk.verifying_key();
    let owner_id = MemberId::from(&owner_vk);

    let mut room_state = ChatRoomStateV1::default();

    // Set configuration
    let mut config = Configuration::default();
    config.display = RoomDisplayMetadata {
        name: SealedBytes::public(room_name.clone().into_bytes()),
        description: description.map(|d| SealedBytes::public(d.as_bytes().to_vec())),
    };
    config.owner_member_id = owner_id;
    match history_depth {
        HistoryDepth::Standard => {}
        // Headroom over the 200 seeded messages so the specs can deliver
        // arrival batches without the at-cap prune shifting the fixture out
        // from under them.
        HistoryDepth::Deep => config.max_recent_messages = DEEP_ROOM_MAX_RECENT_MESSAGES,
        // Landing EXACTLY on the cap is this room's entire purpose; the cap
        // is raised so paired fillers still exceed the render window in
        // display items.
        HistoryDepth::AtCap => config.max_recent_messages = AT_CAP_MAX_RECENT_MESSAGES,
    }
    room_state.configuration = AuthorizedConfigurationV1::new(config, owner_sk);

    // Initialize member lists
    let mut members = MembersV1::default();
    let mut member_info = MemberInfoV1::default();

    // The "other member" identity is generated up front so the owner can
    // deputize them below: this makes the deputy 🛡 shield (and the
    // member-info modal's deputy legend chip, freenet/river#451) visible in
    // example / dev mode and exercisable by Playwright.
    let other_member_sk = SigningKey::generate(&mut csprng);
    let other_member_vk = other_member_sk.verifying_key();
    let other_member_id = MemberId::from(&other_member_vk);

    // Always add owner to member_info. The owner deputizes the other member
    // (a global moderator), so that member shows the 🛡 shield in every view.
    member_info
        .member_info
        .push(AuthorizedMemberInfo::new_with_member_key(
            MemberInfo {
                member_id: owner_id,
                version: 0,
                // The trailing 🛡👑 is a deliberate BADGE-SPOOF attempt baked
                // into example data: the owner's stored nickname claims the
                // deputy shield and the owner crown. `display_nickname` strips
                // both at render time, so the member list must still show
                // exactly ONE shield (the real deputy's) and the conversation
                // must show the owner's name with no glyphs. If the strip ever
                // regresses, `member-info-deputy-tag.spec.ts`'s
                // `toHaveCount(1)` and `conversation-deputy-badge.spec.ts`
                // both fail.
                preferred_nickname: SealedBytes::public(
                    (random_full_name() + " (Owner) \u{1F6E1}\u{1F451}").into_bytes(),
                ),
                deputies: vec![other_member_id],
            },
            owner_sk,
        ));

    // If self is a member (but not owner), add to members list and cache AuthorizedMember
    let mut self_authorized_member = None;
    if self_is == SelfIs::Member {
        let am = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: self_vk.clone(),
            },
            owner_sk,
        );
        self_authorized_member = Some(am.clone());
        members.members.push(am);

        member_info
            .member_info
            .push(AuthorizedMemberInfo::new_with_member_key(
                MemberInfo {
                    member_id: self_id,
                    version: 0,
                    preferred_nickname: SealedBytes::public(
                        (random_full_name() + " (You)").into_bytes(),
                    ),
                    deputies: Vec::new(),
                },
                &self_sk,
            ));
    }

    // Add another member to ensure the room has at least one member. (Their
    // identity was generated above so the owner could deputize them.)
    // In rooms where self is owner, other member should be invited by self
    // In other rooms, other member should be invited by owner
    let inviter_id = if self_is == SelfIs::Owner {
        self_id
    } else {
        owner_id
    };
    let inviter_sk = if self_is == SelfIs::Owner {
        &self_sk
    } else {
        owner_sk
    };

    members.members.push(AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: inviter_id,
            member_vk: other_member_vk,
        },
        inviter_sk,
    ));

    // The deputy's nickname, captured rather than inlined: the impostor added
    // below is DERIVED from it (see `confusable_variant`), because these names
    // are re-rolled on every run.
    let deputy_nickname = random_full_name() + " (Member)";

    member_info
        .member_info
        .push(AuthorizedMemberInfo::new_with_member_key(
            MemberInfo {
                member_id: other_member_id,
                version: 0,
                preferred_nickname: SealedBytes::public(deputy_nickname.clone().into_bytes()),
                deputies: Vec::new(),
            },
            &other_member_sk,
        ));

    // The IMPOSTOR (freenet/river#489), so the ⚠ impersonation warning is
    // visible in `cargo make dev-example` and reachable from Playwright. Before
    // this, the badge had never been seen in a browser: nothing in example data
    // produced two members whose names fold together.
    //
    // They are a plain member, NOT a deputy. Adding a second deputy would break
    // `member-info-deputy-tag.spec.ts`, which asserts exactly ONE member row
    // carries a 🛡 — and it would be the wrong shape anyway, since the point is
    // a member with no authority wearing a moderator's name.
    let impostor_sk = SigningKey::generate(&mut csprng);
    let impostor_vk = impostor_sk.verifying_key();
    let impostor_id = MemberId::from(&impostor_vk);
    members.members.push(AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: inviter_id,
            member_vk: impostor_vk,
        },
        inviter_sk,
    ));
    member_info
        .member_info
        .push(AuthorizedMemberInfo::new_with_member_key(
            MemberInfo {
                member_id: impostor_id,
                version: 0,
                preferred_nickname: SealedBytes::public(
                    confusable_variant(&deputy_nickname).into_bytes(),
                ),
                deputies: Vec::new(),
            },
            &impostor_sk,
        ));

    // Add members to the room
    room_state.members = members.clone();
    room_state.member_info = member_info.clone();

    // Create a map of member IDs to their signing keys for message creation
    let mut member_keys = HashMap::new();
    if self_is == SelfIs::Member {
        member_keys.insert(self_id, self_sk.clone());
    }
    member_keys.insert(other_member_id, other_member_sk);
    member_keys.insert(impostor_id, impostor_sk);

    // Add example messages
    add_example_messages(
        &mut room_state,
        &owner_id,
        owner_sk,
        &member_keys,
        other_member_id,
        impostor_id,
        history_depth,
    );

    let verification_result = room_state.verify(
        &room_state,
        &ChatRoomParametersV1 {
            owner: owner_vk.clone(),
        },
    );
    if !verification_result.is_ok() {
        panic!(
            "Failed to verify room state: {:?}",
            verification_result.err()
        );
    }

    // Generate contract key for the room
    let parameters = ChatRoomParametersV1 { owner: owner_vk };
    let params_bytes = to_cbor_vec(&parameters);
    let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
    // Use the full ContractKey constructor that includes the code hash
    let contract_key =
        ContractKey::from_params_and_code(Parameters::from(params_bytes), &contract_code);

    CreatedRoom {
        owner_vk,
        room_data: RoomData {
            owner_vk: owner_vk.clone(),
            room_state,
            self_sk: self_sk.clone(),
            contract_key,
            last_read_message_id: None,
            secrets: std::collections::HashMap::new(),
            current_secret_version: None,
            last_secret_rotation: None,
            key_migrated_to_delegate: true, // Example data doesn't need migration
            self_authorized_member,
            invite_chain: vec![],
            self_member_info: None,
            self_nickname: None,
            previous_contract_key: None,
            invitation_secrets: std::collections::HashMap::new(),
        },
    }
}

fn add_example_messages(
    room_state: &mut ChatRoomStateV1,
    owner_id: &MemberId,
    owner_key: &SigningKey,
    member_keys: &HashMap<MemberId, SigningKey>,
    // The owner-appointed deputy. Given one guaranteed message below so the
    // conversation's 🛡 badge is deterministically present for Playwright —
    // the random author picks alone leave it to chance.
    deputy_id: MemberId,
    // The member whose nickname is confusable with the deputy's (#489). Given a
    // guaranteed message for the same reason: the ⚠ on an author line must not
    // depend on the random author picks above.
    impostor_id: MemberId,
    history_depth: HistoryDepth,
) {
    // Use a timestamp 24 hours ago as base time for messages
    let now = crate::util::get_current_system_time()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let base_time = now - (24 * 60 * 60 * 1000); // 24 hours ago in milliseconds

    let mut messages = MessagesV1::default();
    let mut current_time_ms = base_time;

    // Verify owner exists in member_info but NOT in members list
    if !room_state
        .member_info
        .member_info
        .iter()
        .any(|m| m.member_info.member_id == *owner_id)
    {
        panic!("Owner ID not found in member_info: {}", owner_id);
    }
    if room_state
        .members
        .members
        .iter()
        .any(|m| m.member.id() == *owner_id)
    {
        panic!(
            "Owner ID found in members list when it should not be: {}",
            owner_id
        );
    }

    // Verify that all member_keys are valid and members exist
    for (member_id, signing_key) in member_keys.iter() {
        if MemberId::from(&signing_key.verifying_key()) != *member_id {
            panic!("Member ID does not match signing key");
        }

        // Verify they exist in members list (unless they're the owner)
        if *member_id != *owner_id
            && !room_state
                .members
                .members
                .iter()
                .any(|m| m.member.id() == *member_id)
        {
            panic!("Member ID not found in members list: {}", member_id);
        }
    }

    // Create a vec of possible authors (owner + members)
    let authors: Vec<(MemberId, &SigningKey)> = member_keys
        .iter()
        .map(|(id, key)| (*id, key))
        .chain(std::iter::once((*owner_id, owner_key)))
        .collect();

    // Deep-history fixtures: filler messages AHEAD of the standard fixture so
    // the room's display-item count comfortably exceeds the render window.
    // Deep-room authors strictly ALTERNATE — consecutive same-author messages
    // within five minutes fold into one display item, so alternation makes
    // "N messages" mean "N display items". The AT-CAP room's authors come in
    // PAIRS (AABB…) instead: two-message display groups are the dominant
    // production shape, and a drained first message RE-KEYING its group is
    // the #505 re-review blocker an alternating fixture cannot see.
    // Deterministic content and fixed 60s gaps: nothing here varies run to
    // run.
    let filler_count = match history_depth {
        HistoryDepth::Standard => 0,
        HistoryDepth::Deep => DEEP_HISTORY_FILLER_MESSAGES,
        HistoryDepth::AtCap => AT_CAP_FILLER_MESSAGES,
    };
    for i in 0..filler_count {
        let author_slot = match history_depth {
            HistoryDepth::AtCap => (i / 2) % 2,
            _ => i % 2,
        };
        let (author_id, signing_key) = authors[author_slot.min(authors.len() - 1)];
        let msg = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: *owner_id,
                author: author_id,
                time: get_time_from_millis(current_time_ms),
                content: RoomMessageBody::public(format!(
                    "history filler {i:02}: the window only renders a tail"
                )),
            },
            signing_key,
        );
        messages.messages.push(msg);
        current_time_ms += 60_000;
    }

    let message_count = 6;

    for _ in 0..message_count {
        // Pick a random author
        let author_idx = rand::random::<usize>() % authors.len();
        let (author_id, signing_key) = authors[author_idx];

        // Generate message with random length (15-35 words)
        let word_count = rand::random::<u8>() % 21 + 15;
        let msg = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: *owner_id,
                author: author_id,
                time: get_time_from_millis(current_time_ms),
                content: RoomMessageBody::public(lipsum(word_count as usize)),
            },
            signing_key,
        );

        messages.messages.push(msg);

        // Add a more natural time gap between messages (30 sec to 15 min)
        current_time_ms += (rand::random::<u64>() % 870 + 30) * 1000;
    }

    // One guaranteed message from the owner-appointed deputy, so the
    // conversation always has an author line carrying the 🛡 badge. The random
    // loop above may or may not pick them.
    if let Some(deputy_key) = member_keys.get(&deputy_id) {
        let msg = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: *owner_id,
                author: deputy_id,
                time: get_time_from_millis(current_time_ms),
                content: RoomMessageBody::public(
                    "Reminder: keep it civil in here, please.".to_string(),
                ),
            },
            deputy_key,
        );
        messages.messages.push(msg);
        current_time_ms += 60_000;
    }

    // One guaranteed message from the IMPOSTOR, so the conversation always has
    // an author line carrying the ⚠ warning (#489) just below the deputy's 🛡
    // line above — which is exactly the comparison the badge exists to let a
    // reader make.
    if let Some(impostor_key) = member_keys.get(&impostor_id) {
        let msg = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: *owner_id,
                author: impostor_id,
                time: get_time_from_millis(current_time_ms),
                content: RoomMessageBody::public(
                    "Hi all, DM me your invite code and I'll sort it out.".to_string(),
                ),
            },
            impostor_key,
        );
        messages.messages.push(msg);
        current_time_ms += 60_000;
    }

    // A message exercising @mention chips. The snapshot name in the token is
    // intentionally generic ("member") to prove the rendered chip re-resolves
    // to the member's CURRENT nickname from member_info rather than the
    // snapshot. Mentions the first non-owner member.
    if let Some(mentioned_id) = member_keys.keys().find(|id| **id != *owner_id).copied() {
        let token = river_core::mention::encode_mention(mentioned_id, "member");
        let msg = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: *owner_id,
                author: *owner_id,
                time: get_time_from_millis(current_time_ms),
                content: RoomMessageBody::public(format!("welcome {token} \u{1F44B}")),
            },
            owner_key,
        );
        messages.messages.push(msg);
        current_time_ms += 60_000;
    }

    // Add a reply to the first message so example data exercises the reply
    // bubble layout (needed for Playwright coverage of #205/#206/#207).
    if !messages.messages.is_empty() {
        let target = &messages.messages[0];
        let target_id = target.id();
        let target_author_id = target.message.author;
        let target_author_name = room_state
            .member_info
            .member_info
            .iter()
            .find(|m| m.member_info.member_id == target_author_id)
            .map(|m| {
                // Example rooms are public, so the secret map is empty. Routed
                // through the shared helper anyway so the render-path pin test
                // has no exceptions to carve out.
                crate::util::display_name::display_nickname(
                    &m.member_info.preferred_nickname,
                    &HashMap::new(),
                )
            })
            .unwrap_or_else(|| "someone".to_string());
        let target_preview = match &target.message.content {
            RoomMessageBody::Public { data, .. } => {
                let preview_len = data.len().min(120);
                String::from_utf8_lossy(&data[..preview_len]).to_string()
            }
            _ => String::new(),
        };

        // Pick any author to write the reply
        let (reply_author_id, reply_signing_key) = authors[0];
        let reply_msg = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: *owner_id,
                author: reply_author_id,
                time: get_time_from_millis(current_time_ms),
                content: RoomMessageBody::reply(
                    "Thanks for bringing this up — quick thought below.".to_string(),
                    target_id,
                    target_author_name,
                    target_preview,
                ),
            },
            reply_signing_key,
        );
        messages.messages.push(reply_msg);
    }

    // A reply whose quoted message CANNOT be resolved, so the example build
    // renders the "Original message unavailable" placeholder and Playwright can
    // assert on it. In production this is what a reply to a BANNED member's
    // message looks like — banning purges the target, leaving only the
    // replier-authored snapshot, which the UI deliberately refuses to render
    // (see `resolve_reply_strip`). A fabricated target id reproduces that
    // end state without having to simulate a ban, and is deterministic.
    {
        // Authored by the OWNER (i.e. self) so the placeholder's `is_self`
        // styling branch is the one Playwright exercises, deterministically —
        // `authors[0]` comes from a HashMap iteration and is not stable.
        current_time_ms += 30_000;
        let orphan_reply = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: *owner_id,
                author: *owner_id,
                time: get_time_from_millis(current_time_ms),
                content: RoomMessageBody::reply(
                    "That was out of line.".to_string(),
                    MessageId(fast_hash(b"river example: purged message")),
                    // Snapshot fields a banned member's reply would carry. The
                    // UI must render NEITHER of them.
                    "BannedUser".to_string(),
                    "snapshot text that must never be rendered".to_string(),
                ),
            },
            owner_key,
        );
        messages.messages.push(orphan_reply);
    }

    // Add a message containing an unbreakable long URL so the bubble width
    // regression for #212 is exercised by Playwright.
    {
        let (long_url_author_id, long_url_signing_key) = authors[0];
        current_time_ms += 30_000;
        let long_url_msg = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: *owner_id,
                author: long_url_author_id,
                time: get_time_from_millis(current_time_ms),
                content: RoomMessageBody::public(
                    "https://example.com/longlongurlpathlonglongurlpathlonglongurlpathlonglongurlpathlonglongurlpathlonglongurlpathlonglongurlpathlonglongurlpathlonglongurlpathlonglongurlpath".to_string(),
                ),
            },
            long_url_signing_key,
        );
        messages.messages.push(long_url_msg);
    }

    // A SELF (owner-authored) reply whose quoted preview is the long URL above.
    // The reply strip renders its preview with `white-space: nowrap`, so an
    // unbreakable long URL in the preview drives the bubble to its full
    // `max-w-prose` width. On a self (right-aligned) message that used to
    // overflow the viewport on narrow mobile screens, clipping the bubble off
    // both edges. Kept in example data so the mobile-overflow Playwright
    // regression test has a self bubble that WOULD overflow without the
    // per-message width clamp.
    if let Some(long_url_msg) = messages.messages.last().cloned() {
        let target_id = long_url_msg.id();
        let target_author_id = long_url_msg.message.author;
        let target_author_name = room_state
            .member_info
            .member_info
            .iter()
            .find(|m| m.member_info.member_id == target_author_id)
            .map(|m| {
                // Example rooms are public, so the secret map is empty. Routed
                // through the shared helper anyway so the render-path pin test
                // has no exceptions to carve out.
                crate::util::display_name::display_nickname(
                    &m.member_info.preferred_nickname,
                    &HashMap::new(),
                )
            })
            .unwrap_or_else(|| "someone".to_string());
        let target_preview = match &long_url_msg.message.content {
            RoomMessageBody::Public { data, .. } => {
                let preview_len = data.len().min(120);
                String::from_utf8_lossy(&data[..preview_len]).to_string()
            }
            _ => String::new(),
        };
        current_time_ms += 30_000;
        let self_reply_to_long_url = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: *owner_id,
                author: *owner_id,
                time: get_time_from_millis(current_time_ms),
                content: RoomMessageBody::reply(
                    "yep, seen that one".to_string(),
                    target_id,
                    target_author_name,
                    target_preview,
                ),
            },
            owner_key,
        );
        messages.messages.push(self_reply_to_long_url);
    }

    // Add reactions to messages from OTHER members (not owner)
    // Rule: One reaction per user per message
    // In "Your Private Room" the owner IS self, so this shows self reacting to others
    let non_owner_messages: Vec<_> = messages
        .messages
        .iter()
        .filter(|m| m.message.author != *owner_id)
        .collect();

    // Get a non-owner member ID for multi-user reaction demo
    let other_member_id = member_keys.keys().find(|id| *id != owner_id).cloned();

    if non_owner_messages.len() >= 1 {
        // First non-owner message: owner reacts with thumbs up, other member with heart
        let msg_id = non_owner_messages[0].id();
        let mut reactions = HashMap::new();
        reactions.insert("👍".to_string(), vec![*owner_id]);
        if let Some(other_id) = other_member_id {
            reactions.insert("❤️".to_string(), vec![other_id]);
        }
        messages.actions_state.reactions.insert(msg_id, reactions);
    }

    if non_owner_messages.len() >= 2 {
        // Second non-owner message: owner reacts with celebration
        let msg_id = non_owner_messages[1].id();
        let mut reactions = HashMap::new();
        reactions.insert("🎉".to_string(), vec![*owner_id]);
        messages.actions_state.reactions.insert(msg_id, reactions);
    }

    room_state.recent_messages = messages;
}

/// A nickname visually confusable with `real` — the impostor half of the #489
/// ⚠ fixture.
///
/// **Derived, not hardcoded, and that is the whole point.** Example nicknames
/// come from [`random_full_name`] and are re-rolled on every run, so a literal
/// `"lan Clarke"` would collide with the deputy only on the runs where the
/// generator happened to pick `Ian Clarke` — a fixture that reaches its own
/// premise about 1 time in 1,000.
///
/// Every branch is a TIER-1 collision by construction, for ANY input:
///
/// * `I` -> `l` and `l` -> `I` swap one member of the fold's *bar class*
///   (`I`, `l`, `1`, `|`, `!` all fold to the same sentinel), so the VISUAL
///   skeleton is unchanged. This is the live 2026-07-25 attack, and the branch
///   the demo wants: the two names are near-indistinguishable on screen.
/// * A letter-for-digit swap from [`crate::util::confusable`]'s ASCII table
///   (`o`->`0`, `e`->`3`, `s`->`5`, `a`->`4`, `t`->`7`), which that table folds
///   straight back, so the skeleton is unchanged.
/// * Uppercasing changes only case, so the CASE-INSENSITIVE skeleton is
///   unchanged.
///
/// **The branch ORDER is a demo decision, not an arbitrary one.** Measured
/// exhaustively over the real pools (46 x 23 = 1,058 handles): `I`->`l` covers
/// 6.5%, `l`->`I` covers 42.1%, and the remaining 51.4% used to fall through to
/// uppercasing. That meant more than half of `cargo make dev-example` runs
/// showed `BOB SMITH (MEMBER)` flagged as impersonating `Bob Smith (Member)` —
/// a correct tier-1 match (the case-insensitive fold is exactly that), but one
/// a developer reasonably reads as a FALSE POSITIVE. A fixture that teaches the
/// reader to distrust the badge inverts its own purpose, and is the same
/// "trains people to ignore the warning" harm the tier-1-only decision exists
/// to avoid. The digit swap is checked first for that reason: all 544 of those
/// handles contain at least one of `o/e/s/a/t`, so it covers the remainder
/// completely and `B0b Smith (Member)` is what the demo shows.
///
/// Uppercasing stays as a last resort for inputs outside the generator's pools.
///
/// `the_example_impostor_always_collides_with_the_deputy` pins the property
/// against the real generator rather than against one hand-picked name.
fn confusable_variant(real: &str) -> String {
    // Letters the ASCII confusable table folds a digit back onto. Kept in the
    // fold's own order so the correspondence is checkable by eye against
    // `fold_ascii_confusable`.
    const DIGIT_FOR: [(char, char); 5] =
        [('o', '0'), ('e', '3'), ('s', '5'), ('a', '4'), ('t', '7')];

    // `I` and `l` are ASCII, so a `find` hit is always a char boundary and
    // `i + 1` is the next one.
    if let Some(i) = real.find('I') {
        return format!("{}l{}", &real[..i], &real[i + 1..]);
    }
    if let Some(i) = real.find('l') {
        return format!("{}I{}", &real[..i], &real[i + 1..]);
    }
    // Both cases: the visual fold lowercases, so `O` and `o` both end up at
    // `o`, which is where the table sends `0`.
    for (i, c) in real.char_indices() {
        if let Some((_, digit)) = DIGIT_FOR
            .iter()
            .find(|(letter, _)| c.eq_ignore_ascii_case(letter))
        {
            return format!("{}{}{}", &real[..i], digit, &real[i + c.len_utf8()..]);
        }
    }
    real.to_uppercase()
}

fn get_time_from_millis(ms: u64) -> SystemTime {
    // Use WASM-compatible time function
    crate::util::get_current_system_time()
        - Duration::from_millis(
            crate::util::get_current_system_time()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::from_secs(0))
                .as_millis() as u64
                - ms,
        )
}

// Test function to create the example data
#[cfg(test)]
mod tests {
    use super::*;

    /// The deep-history fixture must actually put the windowed render path in
    /// play: more display items than `INITIAL_WINDOW_ITEMS` (60), guaranteed
    /// by strict author alternation (messages == display items), and a state
    /// that still verifies. If this shrinks below the window, every #501
    /// Playwright test silently degrades to the small-room path — the exact
    /// coverage gap that let #501 ship.
    #[test]
    fn deep_history_room_exceeds_the_render_window() {
        let room = create_room(
            &"Deep History Room".to_string(),
            SelfIs::Member,
            None,
            HistoryDepth::Deep,
        );

        let msgs = &room.room_data.room_state.recent_messages.messages;
        assert!(
            msgs.len() >= DEEP_HISTORY_FILLER_MESSAGES,
            "expected at least the filler messages, got {}",
            msgs.len()
        );
        // The fillers are the oldest messages; adjacent authors must differ so
        // no two fold into one display item.
        for pair in msgs[..DEEP_HISTORY_FILLER_MESSAGES].windows(2) {
            assert_ne!(
                pair[0].message.author, pair[1].message.author,
                "adjacent fillers share an author and would merge into one \
                 display item"
            );
        }
        assert!(
            DEEP_HISTORY_FILLER_MESSAGES > 60,
            "the fixture must exceed INITIAL_WINDOW_ITEMS or the windowing \
             specs test nothing"
        );
        // Prune headroom: the specs deliver arrival batches into this room;
        // if the total creeps up to the cap, those deliveries silently start
        // pruning fillers and every count-based premise drifts.
        let max = room
            .room_data
            .room_state
            .configuration
            .configuration
            .max_recent_messages;
        assert_eq!(max, DEEP_ROOM_MAX_RECENT_MESSAGES);
        assert!(
            msgs.len() + 80 <= max,
            "the deep room needs at least ~80 messages of prune headroom for \
             delivered arrivals; seeded {} of max {max}",
            msgs.len()
        );
        // create_room verifies the state internally (it panics on failure),
        // so reaching this point means the deep room is contract-valid.
    }

    /// The at-cap fixture must land EXACTLY on its message cap: one message
    /// under and arrivals do not prune (blocker-1's index shift never
    /// happens); one over and `create_room`'s verify sees a state
    /// `apply_delta` would never produce. Its fillers must come in
    /// same-author PAIRS — multi-message display groups are what makes an
    /// at-cap drain RE-KEY the head group (#505 re-review blocker) — while
    /// still exceeding the render window in display ITEMS (pairs halve the
    /// item count).
    #[test]
    fn at_cap_room_is_exactly_at_its_message_cap() {
        let room = create_room(
            &"Capped History Room".to_string(),
            SelfIs::Member,
            None,
            HistoryDepth::AtCap,
        );
        let state = &room.room_data.room_state;
        assert_eq!(
            state.recent_messages.messages.len(),
            state.configuration.configuration.max_recent_messages,
            "the capped room must sit exactly at max_recent_messages so every \
             delivered arrival drains the oldest message (#505 blocker 1)"
        );
        // Paired fillers: within a pair the author repeats (the messages fold
        // into ONE multi-message display group); across pairs it changes (the
        // groups stay distinct).
        let fillers = &state.recent_messages.messages[..AT_CAP_FILLER_MESSAGES];
        for pair in fillers.chunks(2) {
            assert_eq!(
                pair[0].message.author, pair[1].message.author,
                "a filler pair must share its author, or no display group has \
                 more than one message and the re-key path is untestable"
            );
        }
        for boundary in fillers.chunks(2).collect::<Vec<_>>().windows(2) {
            assert_ne!(
                boundary[0][0].message.author, boundary[1][0].message.author,
                "adjacent pairs must differ in author, or they merge into one \
                 display item and the item count collapses"
            );
        }
        // Still deeper than the render window in display ITEMS, so the
        // windowed premise assertions hold for this room too. Counted from
        // the seeded DATA (runs of same-author adjacent messages collapse,
        // as `group_messages` does — modulo its 5-minute threshold, which
        // these fixed 60s-apart fillers never cross) rather than from the
        // constant, so a fixture change that alters the authoring pattern —
        // not just the count — fails here.
        let display_items = fillers
            .windows(2)
            .filter(|w| w[0].message.author != w[1].message.author)
            .count()
            + 1;
        assert!(
            display_items > 60,
            "paired fillers must still exceed INITIAL_WINDOW_ITEMS display \
             items or the windowing specs quietly degrade to the small-room \
             path; got {display_items}"
        );
    }

    /// The impostor's name must ACTUALLY collide with the deputy's, for every
    /// name the generator can produce — not just for the one a developer had in
    /// mind when they wrote the fixture.
    ///
    /// This is the fixture-reaches-its-premise check. A fixture that silently
    /// fails to set up the state it claims is this repo's signature failure
    /// mode: the Playwright spec would still pass its "no ⚠ on the deputy" half
    /// while the ⚠ it is supposed to find never rendered at all.
    #[test]
    fn the_example_impostor_always_collides_with_the_deputy() {
        use crate::util::confusable::folds_together;

        // One name per branch of `confusable_variant`, so a future edit that
        // breaks a branch fails here rather than in a browser.
        for (real, why) in [
            ("Ian Clarke (Member)", "capital I -> lowercase l"),
            ("Alice Golden (Member)", "no capital I; lowercase l -> I"),
            ("Bob Smith (Member)", "neither I nor l; letter -> digit"),
            ("Zyx Wvu", "no I, no l, no o/e/s/a/t; uppercased"),
        ] {
            let variant = confusable_variant(real);
            assert_ne!(
                variant, real,
                "{why}: the impostor must differ from {real:?}"
            );
            assert!(
                folds_together(&variant, real),
                "{why}: {variant:?} must fold onto {real:?} or the ⚠ never fires"
            );
        }

        // The demo-QUALITY property, pinned rather than left to a comment: the
        // uppercase fallback must be unreachable for every handle the generator
        // can actually produce. It used to catch 51.4% of them, and
        // `BOB SMITH (MEMBER)` beside `Bob Smith (Member)` reads as a false
        // positive to anyone looking at the dev build — which teaches exactly
        // the distrust of the badge this feature cannot afford.
        for _ in 0..500 {
            let real = random_full_name() + " (Member)";
            assert_ne!(
                confusable_variant(&real),
                real.to_uppercase(),
                "{real:?} fell through to the uppercase fallback; the demo now \
                 shows a shouted name, which reads as a false positive"
            );
        }

        // And against the REAL generator, which is what the fixture uses.
        for _ in 0..500 {
            let real = random_full_name() + " (Member)";
            let variant = confusable_variant(&real);
            assert_ne!(variant, real);
            assert!(
                folds_together(&variant, &real),
                "{variant:?} does not fold onto {real:?}"
            );
        }
    }

    /// The end-to-end fixture precondition: in EVERY example room the viewer's
    /// own checker is non-empty, the deputy is badged, and the impostor is the
    /// member it flags.
    ///
    /// Asserted through the same entry points the UI renders from, so it cannot
    /// pass while the browser shows nothing.
    #[test]
    fn every_example_room_shows_the_impersonation_warning() {
        use crate::components::members::{
            deputy_badges_for_viewer, impersonation_checker_for_viewer,
            impersonation_warning_for_display, privilege_in_view,
        };
        use crate::util::display_name::display_nickname;

        for (owner_vk, room_data) in create_example_rooms().map.iter() {
            let owner_id = MemberId::from(owner_vk);
            let self_member_id = MemberId::from(&room_data.self_sk.verifying_key());
            let state = &room_data.room_state;

            let deputy_badges = deputy_badges_for_viewer(
                &state.members,
                &state.member_info,
                &room_data.secrets,
                owner_id,
                self_member_id,
            );
            assert!(
                !deputy_badges.is_empty(),
                "the owner-appointed deputy must be badged, or there is no \
                 protected name for the impostor to collide with"
            );

            let checker = impersonation_checker_for_viewer(
                &state.member_info,
                &room_data.secrets,
                owner_id,
                &deputy_badges,
            );
            assert!(
                !checker.is_empty(),
                "the protected set is empty, so no ⚠ can ever render in this room"
            );

            let flagged: Vec<MemberId> = state
                .members
                .members
                .iter()
                .map(|m| m.member.id())
                .filter(|&id| {
                    let Some(mi) = state.member_info.canonical(id) else {
                        return false;
                    };
                    let name =
                        display_nickname(&mi.member_info.preferred_nickname, &room_data.secrets);
                    impersonation_warning_for_display(
                        &checker,
                        id,
                        &name,
                        privilege_in_view(id, owner_id, &deputy_badges),
                    )
                    .is_some()
                })
                .collect();

            assert_eq!(
                flagged.len(),
                1,
                "exactly one member (the impostor) must be flagged; the deputy \
                 is exempt from their OWN name and nobody else should collide"
            );
            assert!(
                !deputy_badges.contains_key(&flagged[0]),
                "the flagged member must be the plain-member impostor, not a \
                 badged deputy"
            );
        }
    }

    #[test]
    fn test_create_example_rooms() {
        let rooms = create_example_rooms();
        assert_eq!(rooms.map.len(), 3);

        for (owner_vk, room_data) in rooms.map.iter() {
            // Verify the room state
            let verification_result = room_data.room_state.verify(
                &room_data.room_state,
                &ChatRoomParametersV1 {
                    owner: owner_vk.clone(),
                },
            );
            assert!(
                verification_result.is_ok(),
                "Room state failed to verify: {:?}",
                verification_result.err()
            );

            // Verify room has at least basic configuration
            assert!(
                room_data
                    .room_state
                    .configuration
                    .configuration
                    .display
                    .name
                    .declared_len()
                    > 0
            );

            // Verify members list exists
            assert!(!room_data.room_state.members.members.is_empty());

            // Verify member info exists
            assert!(!room_data.room_state.member_info.member_info.is_empty());
        }
    }
}

/// Browser hooks the Playwright specs use to deliver INBOUND messages.
///
/// The composer is not a substitute: `handle_send_message` raises
/// `force_scroll`, which deliberately bypasses the pin that the scroll specs
/// exist to test, so a message sent through the UI proves nothing about how an
/// arriving one behaves. These write straight into `ROOMS`, as an arriving
/// network update does.
///
/// They skip `apply_delta`'s verification on purpose, so they must never be
/// reachable anywhere the result could be pushed to the network. Two gates,
/// both required:
///
/// * `example-data`, which is off for the published webapp (`UI_FEATURES` is
///   empty for `build-webapp`) and for every non-example build;
/// * `no-sync`, because `cargo make dev-example` turns `example-data` on
///   WITHOUT it. A message minted here is signed by a key that is not a room
///   member, so a developer pointing that build at a live node would otherwise
///   have the synchronizer try to push contract-invalid state.
#[cfg(all(target_arch = "wasm32", feature = "no-sync"))]
pub fn install_test_hooks() {
    use wasm_bindgen::prelude::*;

    let Some(window) = web_sys::window() else {
        return;
    };
    // Idempotent: `App` may re-render, and re-installing would leak closures.
    if js_sys::Reflect::has(&window, &JsValue::from_str("__riverTest")).unwrap_or(false) {
        return;
    }

    let hooks = js_sys::Object::new();

    let append = Closure::wrap(Box::new(move |text: String| {
        crate::util::defer(move || deliver_message(text, Delivery::Append));
    }) as Box<dyn FnMut(String)>);
    let _ = js_sys::Reflect::set(&hooks, &JsValue::from_str("appendMessage"), append.as_ref());
    append.forget();

    let insert = Closure::wrap(Box::new(move |text: String| {
        crate::util::defer(move || deliver_message(text, Delivery::BeforeLast));
    }) as Box<dyn FnMut(String)>);
    let _ = js_sys::Reflect::set(
        &hooks,
        &JsValue::from_str("insertMessageBeforeLast"),
        insert.as_ref(),
    );
    insert.forget();

    // A burst in ONE state mutation — one delta application, one re-render —
    // as a network delta carrying many messages produces. The windowing specs
    // use it to grow an anchored window well past one backfill step without
    // paying per-delivery render round-trips (#505 blocker 2).
    let append_many = Closure::wrap(Box::new(move |count: u32| {
        crate::util::defer(move || deliver_batch(count as usize));
    }) as Box<dyn FnMut(u32)>);
    let _ = js_sys::Reflect::set(
        &hooks,
        &JsValue::from_str("appendMessages"),
        append_many.as_ref(),
    );
    append_many.forget();

    let _ = js_sys::Reflect::set(&window, &JsValue::from_str("__riverTest"), &hooks);
}

#[cfg(all(not(target_arch = "wasm32"), feature = "no-sync"))]
pub fn install_test_hooks() {}

/// Where a delivered message lands in the room's message list.
#[cfg(all(target_arch = "wasm32", feature = "no-sync"))]
#[derive(Clone, Copy, PartialEq)]
enum Delivery {
    /// At the end, as an ordinary arrival does.
    Append,
    /// One position from the end — the mid-list insert that grows the history
    /// WITHOUT remounting its last row, which is the case the old
    /// `onmounted`-on-the-last-bubble scroll trigger could not see.
    BeforeLast,
}

/// Fixed identities, so a delivered message never merges into the group above
/// it by accident: grouping needs the same author within 5 minutes, and
/// `BeforeLast` in particular has to leave the last group's key (its first
/// message's id) untouched or the row remounts and the test proves nothing.
///
/// APPENDED arrivals ALTERNATE between two identities per message, so a burst
/// of N arrivals is N display items rather than folding into one group — a
/// six-arrival follow loop that renders as a single item exercises the
/// windowing's grow/trim interleave zero times (#505 review).
#[cfg(all(target_arch = "wasm32", feature = "no-sync"))]
fn test_author(delivery: Delivery, seq: u64) -> (SigningKey, &'static str) {
    match delivery {
        Delivery::Append if seq % 2 == 0 => (SigningKey::from_bytes(&[0x5A; 32]), "Test Sender A"),
        Delivery::Append => (SigningKey::from_bytes(&[0x6A; 32]), "Test Sender B"),
        Delivery::BeforeLast => (SigningKey::from_bytes(&[0x7B; 32]), "Test Sender Insert"),
    }
}

// Monotone counter driving the append-author alternation. Shared by single
// and batched deliveries so alternation continues across calls. WASM is
// single-threaded; the thread_local is just the idiomatic mutable static.
#[cfg(all(target_arch = "wasm32", feature = "no-sync"))]
thread_local! {
    static APPEND_SEQ: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Append `message` to `room` as the given test identity, registering the
/// identity's nickname the first time it speaks so the bubble renders like
/// any other member's rather than as "Unknown".
#[cfg(all(target_arch = "wasm32", feature = "no-sync"))]
fn push_test_message(
    room: &mut RoomData,
    room_key: &VerifyingKey,
    text: String,
    sk: &SigningKey,
    nickname: &str,
    at_end: bool,
) {
    let author = MemberId::from(&sk.verifying_key());
    if !room
        .room_state
        .member_info
        .member_info
        .iter()
        .any(|entry| entry.member_info.member_id == author)
    {
        room.room_state
            .member_info
            .member_info
            .push(AuthorizedMemberInfo::new_with_member_key(
                MemberInfo {
                    member_id: author,
                    version: 0,
                    preferred_nickname: SealedBytes::public(nickname.as_bytes().to_vec()),
                    deputies: Vec::new(),
                },
                sk,
            ));
    }

    let message = AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: MemberId::from(room_key),
            author,
            content: RoomMessageBody::public(text),
            time: crate::util::get_current_system_time(),
        },
        sk,
    );

    let messages = &mut room.room_state.recent_messages.messages;
    let at = if at_end {
        messages.len()
    } else {
        messages.len().saturating_sub(1)
    };
    messages.insert(at, message);
}

/// What a real arrival does once the room is at `max_recent_messages`: drain
/// the oldest. Mirrors `MessagesV1::apply_delta`
/// (common/src/room_state/message.rs) — without this, the hooks grow the
/// message list unboundedly and the at-cap index-shift path (#505 blocker 1)
/// is unreachable from the browser suite.
#[cfg(all(target_arch = "wasm32", feature = "no-sync"))]
fn prune_to_cap(room: &mut RoomData) {
    let max = room
        .room_state
        .configuration
        .configuration
        .max_recent_messages;
    let messages = &mut room.room_state.recent_messages.messages;
    if messages.len() > max {
        let excess = messages.len() - max;
        messages.drain(0..excess);
    }
}

#[cfg(all(target_arch = "wasm32", feature = "no-sync"))]
fn deliver_message(text: String, delivery: Delivery) {
    use crate::components::app::{CURRENT_ROOM, ROOMS};
    use dioxus::prelude::*;

    let Some(room_key) = CURRENT_ROOM.peek().owner_key else {
        return;
    };
    let seq = match delivery {
        Delivery::Append => APPEND_SEQ.with(|s| {
            let v = s.get();
            s.set(v + 1);
            v
        }),
        Delivery::BeforeLast => 0,
    };
    let (sk, nickname) = test_author(delivery, seq);

    ROOMS.with_mut(|rooms| {
        let Some(room) = rooms.map.get_mut(&room_key) else {
            return;
        };
        push_test_message(
            room,
            &room_key,
            text,
            &sk,
            nickname,
            delivery == Delivery::Append,
        );
        prune_to_cap(room);
    });
}

/// Deliver `count` appended arrivals in ONE `ROOMS` mutation (one re-render),
/// alternating authors like single deliveries do.
#[cfg(all(target_arch = "wasm32", feature = "no-sync"))]
fn deliver_batch(count: usize) {
    use crate::components::app::{CURRENT_ROOM, ROOMS};
    use dioxus::prelude::*;

    let Some(room_key) = CURRENT_ROOM.peek().owner_key else {
        return;
    };
    ROOMS.with_mut(|rooms| {
        let Some(room) = rooms.map.get_mut(&room_key) else {
            return;
        };
        for i in 0..count {
            let seq = APPEND_SEQ.with(|s| {
                let v = s.get();
                s.set(v + 1);
                v
            });
            let (sk, nickname) = test_author(Delivery::Append, seq);
            push_test_message(
                room,
                &room_key,
                format!("batched arrival {i:02}"),
                &sk,
                nickname,
                true,
            );
        }
        prune_to_cap(room);
    });
}
