//! Browser hooks (`window.__riverTest`) the Playwright specs use to deliver
//! INBOUND messages, remove messages, drive the no-room screen's load states,
//! complete an own DM while another thread is open, and put reactions on the
//! newest message.
//!
//! The composer is not a substitute: an own send is an explicit request to go
//! to the newest message, so a message sent through the UI
//! proves nothing about how an arriving one behaves. These write straight into
//! `ROOMS`, as an arriving network update does.
//!
//! The room hooks skip `apply_delta`'s verification on purpose, and every hook
//! admits test identities on self's say-so, so none may be reachable anywhere
//! the result could be pushed to the network. The module compiles only for
//! wasm32 with two features, both required:
//!
//! * `example-data`, which is off for the published webapp (`UI_FEATURES` is
//!   empty for `build-webapp`) and for every non-example build;
//! * `no-sync`, because `cargo make dev-example` turns `example-data` on
//!   WITHOUT it. A developer pointing that build at a live node would
//!   otherwise have the synchronizer push the test members and their messages
//!   into a real room, or contract-invalid state where they do not verify.

use crate::components::app::chat_delegate::{RoomsLoadState, ROOMS_LOAD_STATE};
use crate::components::app::{CURRENT_ROOM, ROOMS};
use crate::room_data::{CurrentRoom, RoomData};
use dioxus::prelude::*;
use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_scaffold::ComposableState;
use river_core::room_state::{
    configuration::AuthorizedConfigurationV1,
    direct_messages::{compose_direct_message, DirectMessagesDelta},
    member::{AuthorizedMember, Member, MemberId},
    member_info::{AuthorizedMemberInfo, MemberInfo},
    message::{AuthorizedMessageV1, MessageV1, RoomMessageBody},
    privacy::{PrivacyMode, SealedBytes},
    ChatRoomParametersV1,
};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsValue;

thread_local! {
    static FORGE_NEXT_SEND: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The load worker `setRoomsLoadStateKeepingRooms("migrating")` holds open
    /// (until the next call, or `releaseHeldLoadWorker`).
    static HELD_LOAD_WORKER: std::cell::RefCell<
        Option<crate::components::app::chat_delegate::LoadWorkerGuard>,
    > = const { std::cell::RefCell::new(None) };
}

/// `signature` with one bit flipped if `forgeNextSendSignature` was called
/// since the last send, else unchanged. Called by the composer's send path.
pub fn forge_if_requested(signature: ed25519_dalek::Signature) -> ed25519_dalek::Signature {
    if !FORGE_NEXT_SEND.with(|f| f.replace(false)) {
        return signature;
    }
    let mut bytes = signature.to_bytes();
    bytes[0] ^= 1;
    ed25519_dalek::Signature::from_bytes(&bytes)
}

pub fn install_test_hooks() {
    let Some(window) = web_sys::window() else {
        return;
    };
    // Idempotent: `App` may re-render, and re-installing would leak closures.
    if js_sys::Reflect::has(&window, &JsValue::from_str("__riverTest")).unwrap_or(false) {
        return;
    }

    let hooks = js_sys::Object::new();
    let set_hook = |name: &str, hook: JsValue| {
        let _ = js_sys::Reflect::set(&hooks, &JsValue::from_str(name), &hook);
    };

    set_hook(
        "appendMessage",
        Closure::<dyn FnMut(String)>::new(move |text: String| {
            crate::util::defer(move || deliver([(text, Delivery::Append)]));
        })
        .into_js_value(),
    );

    // A DM history from test peer `peer` to self, in ONE mutation: `count`
    // messages labelled "<history_label> NN", a minute apart and all in the
    // past, so a DM sent or delivered afterwards sorts below them. Long enough
    // at 30 to make the thread scroll on every test viewport.
    set_hook(
        "appendDmsForPeer",
        Closure::<dyn FnMut(u32, u32)>::new(move |peer: u32, count: u32| {
            let peer = dm_peer(peer);
            crate::util::defer(move || append_dm_history(peer, count));
        })
        .into_js_value(),
    );

    // `count` DMs from test peer `peer`, labelled like `appendDmsForPeer`'s,
    // across exactly two local days, yesterday and today, whatever the time
    // (`two_day_stamp`). The thread shows a day break between.
    set_hook(
        "appendDmsAcrossDaysForPeer",
        Closure::<dyn FnMut(u32, u32)>::new(move |peer: u32, count: u32| {
            let peer = dm_peer(peer);
            crate::util::defer(move || {
                deliver_dms(
                    peer,
                    (0..count)
                        .map(|i| format!("{} {i:02}", peer.history_label))
                        .collect(),
                    DmStamp::TwoDays,
                )
            });
        })
        .into_js_value(),
    );

    // One inbound DM from test peer `peer`, stamped after every DM in the
    // thread so it renders as the newest.
    set_hook(
        "deliverDmForPeer",
        Closure::<dyn FnMut(u32, String)>::new(move |peer: u32, text: String| {
            let peer = dm_peer(peer);
            crate::util::defer(move || deliver_dms(peer, vec![text], DmStamp::Newest));
        })
        .into_js_value(),
    );

    // Admit a test peer to the current room with no DMs, so a spec can open
    // an empty thread with them from Member Info.
    set_hook(
        "admitDmPeer",
        Closure::<dyn FnMut(u32)>::new(move |peer: u32| {
            let peer = dm_peer(peer);
            crate::util::defer(move || {
                with_current_room_mut(|room, room_key| {
                    let room_key = *room_key;
                    let peer_sk = SigningKey::from_bytes(&peer.seed);
                    admit_with_nickname(room, &room_key, &peer_sk, peer.nickname);
                })
            });
        })
        .into_js_value(),
    );

    // An own DM to test peer `peer` in the current room, through the invite
    // picker's send path, so it can complete while another thread is open.
    // Resolves to the outcome's name, "Sent" on success.
    set_hook(
        "sendDmToPeer",
        Closure::<dyn FnMut(u32, String) -> js_sys::Promise>::new(
            move |peer: u32, text: String| {
                let peer_id =
                    MemberId::from(&SigningKey::from_bytes(&dm_peer(peer).seed).verifying_key());
                let (tx, rx) = futures::channel::oneshot::channel();
                crate::util::defer(move || {
                    let _ = tx.send(CURRENT_ROOM.peek().owner_key);
                });
                wasm_bindgen_futures::future_to_promise(async move {
                    let room = rx
                        .await
                        .ok()
                        .flatten()
                        .ok_or_else(|| JsValue::from_str("sendDmToPeer: no current room"))?;
                    let body = river_core::room_state::dm_body::DirectMessageBody::Text { text };
                    let outcome =
                        crate::components::direct_messages::send_structured_dm(room, peer_id, body)
                            .await;
                    Ok(JsValue::from_str(&format!("{outcome:?}")))
                })
            },
        )
        .into_js_value(),
    );

    set_hook(
        "insertMessageBeforeLast",
        Closure::<dyn FnMut(String)>::new(move |text: String| {
            crate::util::defer(move || deliver([(text, Delivery::BeforeLast)]));
        })
        .into_js_value(),
    );

    // A burst in ONE state mutation — one delta application, one re-render —
    // as a network delta carrying many messages produces. The windowing specs
    // use it to grow an anchored window well past one backfill step without
    // paying per-delivery render round-trips (#505).
    set_hook(
        "appendMessages",
        Closure::<dyn FnMut(u32)>::new(move |count: u32| {
            crate::util::defer(move || {
                deliver((0..count).map(|i| (format!("batched arrival {i:02}"), Delivery::Append)))
            });
        })
        .into_js_value(),
    );

    set_hook(
        "addReactionsToNewest",
        Closure::<dyn FnMut(js_sys::Array)>::new(move |emojis: js_sys::Array| {
            let emojis: Vec<String> = emojis.iter().filter_map(|e| e.as_string()).collect();
            crate::util::defer(move || add_reactions_to_newest(emojis));
        })
        .into_js_value(),
    );

    // Replace the current room's avatar salt with 32 copies of `fill`, or
    // remove it with `null`, so a spec can see both the neutral disc and two
    // different salts colouring one member differently.
    set_hook(
        "setAvatarSalt",
        Closure::<dyn FnMut(JsValue)>::new(move |fill: JsValue| {
            let fill = fill.as_f64().map(|f| f as u8);
            crate::util::defer(move || {
                let Some(room_key) = CURRENT_ROOM.peek().owner_key else {
                    return;
                };
                let key = room_key.to_bytes();
                crate::avatar_salt::AVATAR_SALTS.with_mut(|salts| match fill {
                    Some(b) => {
                        salts.insert(key, [b; 32]);
                    }
                    None => {
                        salts.remove(&key);
                    }
                });
            });
        })
        .into_js_value(),
    );

    // Append `count` join events in ONE mutation, from the alternating
    // appended identities ("Test Sender A" / "Test Sender B"), so two fold into
    // one "A and B joined" notice.
    set_hook(
        "appendJoinEvents",
        Closure::<dyn FnMut(u32)>::new(move |count: u32| {
            crate::util::defer(move || {
                with_current_room_mut(|room, room_key| {
                    for _ in 0..count {
                        push_test_body(
                            room,
                            room_key,
                            RoomMessageBody::join_event(),
                            Delivery::Append,
                        );
                    }
                    prune_to_cap(room);
                });
            });
        })
        .into_js_value(),
    );

    // Remove every message in the current room whose text contains the given
    // substring, in ONE mutation — the shape of a ban purge or a retention
    // drain, which take messages out of the list rather than adding a delete
    // action. The scroll specs use it for deletions above and at the reader.
    set_hook(
        "removeMessages",
        Closure::<dyn FnMut(String)>::new(move |needle: String| {
            crate::util::defer(move || remove_messages(&needle));
        })
        .into_js_value(),
    );

    // Put the current room in the state of a private room whose secret this
    // device does not hold. The conversation then offers no composer and
    // says why. The rendered history does not change, since every example
    // message body is public.
    //
    // Only for a room self owns: the configuration is re-signed with the
    // owner's key, because a configuration that fails its signature check
    // reads as a room still awaiting its initial sync, which has no composer.
    set_hook(
        "makeRoomPrivateWithoutSecret",
        Closure::<dyn FnMut()>::new(move || {
            crate::util::defer(|| {
                with_current_room_mut(|room, room_key| {
                    let Some(owner_sk) = room
                        .signing_key()
                        .filter(|sk| sk.verifying_key() == *room_key)
                        .cloned()
                    else {
                        web_sys::console::error_1(
                            &"__riverTest.makeRoomPrivateWithoutSecret: self must own the room"
                                .into(),
                        );
                        return;
                    };
                    let mut config = room.room_state.configuration.configuration.clone();
                    config.privacy_mode = PrivacyMode::Private;
                    room.room_state.configuration =
                        AuthorizedConfigurationV1::new(config, &owner_sk);
                    room.secrets.clear();
                    room.current_secret_version = None;
                });
            });
        })
        .into_js_value(),
    );

    // Corrupt the signature of the next own message the composer sends, so
    // the room does not admit it and the send fails AFTER the composer has
    // handed it off: the failed-send case of the A06 specs.
    set_hook(
        "forgeNextSendSignature",
        Closure::<dyn FnMut()>::new(move || FORGE_NEXT_SEND.with(|f| f.set(true))).into_js_value(),
    );

    // Drive the no-room screen's load states (freenet/river#509), which the
    // example build, always seeded with rooms, never leaves. Clears ROOMS too:
    // `room_list_display_state` renders the list whenever there is anything to show.
    set_hook(
        "setRoomsLoadState",
        Closure::<dyn FnMut(String)>::new(move |state: String| {
            let parsed = match state.as_str() {
                "loading" => RoomsLoadState::Loading,
                "migrating" => RoomsLoadState::Migrating,
                "failed" => RoomsLoadState::LoadFailed,
                "loaded" => RoomsLoadState::Loaded,
                other => wasm_bindgen::throw_str(&format!("unknown rooms load state {other:?}")),
            };
            // A worker `setRoomsLoadStateKeepingRooms("migrating")` holds is
            // deliberately NOT released here: settling it over the emptied
            // ROOMS would arm the idle resolution, which rewrites this state to
            // `Loaded` 1.5 s later. Do not mix the two hooks on one page.
            crate::util::defer(move || {
                ROOMS.with_mut(|rooms| {
                    rooms.map.clear();
                    rooms.room_order.clear();
                    rooms.current_room_key = None;
                });
                *CURRENT_ROOM.write() = CurrentRoom { owner_key: None };
                *ROOMS_LOAD_STATE.write() = parsed;
            });
        })
        .into_js_value(),
    );

    // Set the load state WITHOUT clearing ROOMS, for the rail's note under a
    // partly restored list (freenet/river#802), which needs rooms showing.
    set_hook(
        "setRoomsLoadStateKeepingRooms",
        Closure::<dyn FnMut(String)>::new(move |state: String| {
            let parsed = match state.as_str() {
                "loading" => RoomsLoadState::Loading,
                "migrating" => RoomsLoadState::Migrating,
                "failed" => RoomsLoadState::LoadFailed,
                "loaded" => RoomsLoadState::Loaded,
                other => wasm_bindgen::throw_str(&format!("unknown rooms load state {other:?}")),
            };
            // The rail shows `Migrating` under a list only while a load worker
            // runs (a stale `Migrating` must not keep the note up), so
            // "migrating" holds a real worker guard until the next call.
            HELD_LOAD_WORKER.with(|held| {
                *held.borrow_mut() = (parsed == RoomsLoadState::Migrating)
                    .then(crate::components::app::chat_delegate::LoadWorkerGuard::new);
            });
            // Written only on a change, as production does
            // (`set_rooms_load_state`), so a repeated "migrating" re-renders
            // through the worker counter alone.
            crate::util::defer(move || {
                if *ROOMS_LOAD_STATE.peek() != parsed {
                    *ROOMS_LOAD_STATE.write() = parsed;
                }
            });
        })
        .into_js_value(),
    );

    // The held load worker settles while the load state stays as it is: the
    // stale-`Migrating` case, whose note must go (freenet/river#802 review).
    set_hook(
        "releaseHeldLoadWorker",
        Closure::<dyn FnMut()>::new(move || {
            // Dropped inside `defer`, like a real worker settling off a task.
            let held = HELD_LOAD_WORKER.with(|held| held.borrow_mut().take());
            crate::util::defer(move || drop(held));
        })
        .into_js_value(),
    );

    // A journaled walk that may still bring rooms back starts (`true`) or ends
    // (`false`), through the same journal-session version counter a real walk
    // bumps (freenet/river#802).
    set_hook(
        "setWalkRestoringRooms",
        Closure::<dyn FnMut(bool)>::new(move |on: bool| {
            crate::components::app::freenet_api::migration_journal::test_set_walk_restoring(on);
        })
        .into_js_value(),
    );

    // Switch rooms the way a notification click does: CURRENT_ROOM changes
    // while whatever modal is open stays open. The room list is behind the
    // modal's backdrop, so the UI itself cannot do this from a test.
    set_hook(
        "switchRoom",
        Closure::<dyn FnMut(String)>::new(move |name: String| {
            // Everything inside `defer`: reading ROOMS needs the Dioxus runtime, which
            // a raw JS call does not have (and `panic = "abort"` would kill the page).
            crate::util::defer(move || {
                let key = ROOMS.peek().map.iter().find_map(|(key, room)| {
                    let sealed = &room.room_state.configuration.configuration.display.name;
                    let shown =
                        crate::util::ecies::unseal_bytes_with_secrets(sealed, &room.secrets)
                            .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
                            .unwrap_or_else(|_| sealed.to_string_lossy());
                    (shown == name).then_some(*key)
                });
                match key {
                    Some(key) => {
                        *CURRENT_ROOM.write() = CurrentRoom {
                            owner_key: Some(key),
                        };
                    }
                    None => web_sys::console::error_1(
                        &format!("__riverTest.switchRoom: no room named {name:?}").into(),
                    ),
                }
            });
        })
        .into_js_value(),
    );

    // How long a mouse click on the room description waits before it
    // toggles. A spec sets it long, so the wait cannot end while the steps
    // that must happen inside it run, however slow the runner.
    set_hook(
        "setDescriptionClickWaitMs",
        Closure::<dyn FnMut(u32)>::new(move |ms: u32| {
            crate::components::conversation::set_description_click_wait_ms(u64::from(ms));
        })
        .into_js_value(),
    );

    let _ = js_sys::Reflect::set(&window, &JsValue::from_str("__riverTest"), &hooks);
}

/// Where a delivered message lands in the room's message list.
#[derive(Clone, Copy)]
enum Delivery {
    /// At the end, as an ordinary arrival does.
    Append,
    /// One position from the end — the mid-list insert that grows the history
    /// WITHOUT remounting its last row, which is the case the old
    /// `onmounted`-on-the-last-message scroll trigger could not see.
    BeforeLast,
}

// Drives the append-author alternation, across single and batched deliveries.
thread_local! {
    static APPEND_SEQ: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Fixed identities, so a delivered message never merges into the group above
/// it by accident: grouping needs the same author within 5 minutes, and
/// `BeforeLast` in particular has to leave the last group's key (its first
/// message's id) untouched or the row remounts and the test proves nothing.
///
/// APPENDED arrivals ALTERNATE between two identities per message, so a burst
/// of N arrivals is N display items rather than folding into one group — a
/// six-arrival follow loop that renders as a single item exercises the
/// windowing's grow/trim interleave zero times (#505).
fn test_author(delivery: Delivery) -> (SigningKey, &'static str) {
    match delivery {
        Delivery::Append => {
            let seq = APPEND_SEQ.with(|s| s.replace(s.get() + 1));
            if seq % 2 == 0 {
                (SigningKey::from_bytes(&[0x5A; 32]), "Test Sender A")
            } else {
                (SigningKey::from_bytes(&[0x6A; 32]), "Test Sender B")
            }
        }
        Delivery::BeforeLast => (SigningKey::from_bytes(&[0x7B; 32]), "Test Sender Insert"),
    }
}

/// Admit `member_vk` to `room` as a member invited by self, so its messages
/// survive `MessagesV1::apply_delta`, which keeps only messages signed by a
/// member or the owner. The invitation chain is test identity -> self ->
/// owner. Skipped where self cannot vouch: an observer is not a member, and
/// such a room has no composer, so no local `apply_delta` runs there anyway.
fn admit_test_member(room: &mut RoomData, room_key: &VerifyingKey, member_vk: VerifyingKey) {
    let owner_id = MemberId::from(room_key);
    let members = &room.room_state.members.members;
    if member_vk == *room_key || members.iter().any(|m| m.member.member_vk == member_vk) {
        return;
    }
    let Some(self_sk) = room.signing_key().cloned() else {
        return;
    };
    let self_vk = self_sk.verifying_key();
    if self_vk != *room_key && !members.iter().any(|m| m.member.member_vk == self_vk) {
        return;
    }
    room.room_state.members.members.push(AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: MemberId::from(&self_vk),
            member_vk,
        },
        &self_sk,
    ));
}

/// Admit `sk`'s identity (see `admit_test_member`) and register its nickname
/// the first time it speaks, so it renders like any other member rather than
/// as "Unknown".
fn admit_with_nickname(
    room: &mut RoomData,
    room_key: &VerifyingKey,
    sk: &SigningKey,
    nickname: &str,
) {
    let id = MemberId::from(&sk.verifying_key());
    admit_test_member(room, room_key, sk.verifying_key());
    if !room
        .room_state
        .member_info
        .member_info
        .iter()
        .any(|entry| entry.member_info.member_id == id)
    {
        room.room_state
            .member_info
            .member_info
            .push(AuthorizedMemberInfo::new_with_member_key(
                MemberInfo {
                    member_id: id,
                    version: 0,
                    preferred_nickname: SealedBytes::public(nickname.as_bytes().to_vec()),
                    deputies: Vec::new(),
                },
                sk,
            ));
    }
}

/// Insert `text` into `room` where `delivery` says, as that delivery's test
/// identity.
fn push_test_message(
    room: &mut RoomData,
    room_key: &VerifyingKey,
    text: String,
    delivery: Delivery,
) {
    push_test_body(room, room_key, RoomMessageBody::public(text), delivery);
}

/// Insert `content` into `room` where `delivery` says, as that delivery's
/// test identity.
fn push_test_body(
    room: &mut RoomData,
    room_key: &VerifyingKey,
    content: RoomMessageBody,
    delivery: Delivery,
) {
    let (sk, nickname) = test_author(delivery);
    let author = MemberId::from(&sk.verifying_key());
    admit_with_nickname(room, room_key, &sk, nickname);

    let time = crate::util::get_current_system_time();
    let message = AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: MemberId::from(room_key),
            author,
            content,
            time,
        },
        &sk,
    );

    let messages = &mut room.room_state.recent_messages.messages;
    let at = match delivery {
        Delivery::Append => messages.len(),
        Delivery::BeforeLast => messages.len().saturating_sub(1),
    };
    messages.insert(at, message);
}

/// What a real arrival does once the room is at `max_recent_messages`: drain
/// the oldest. Mirrors the room's `post_apply_cleanup` message cap
/// (common/src/room_state.rs) — without this, the hooks grow the
/// message list unboundedly and the at-cap index-shift path (#505)
/// is unreachable from the browser suite.
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

/// Run `f` on the current room in ONE `ROOMS` mutation. It writes `ROOMS`, so
/// call it only from inside a `defer` closure.
fn with_current_room_mut(f: impl FnOnce(&mut RoomData, &VerifyingKey)) {
    let Some(room_key) = CURRENT_ROOM.peek().owner_key else {
        return;
    };
    ROOMS.with_mut(|rooms| {
        if let Some(room) = rooms.map.get_mut(&room_key) {
            f(room, &room_key);
        }
    });
}

/// Drop the current room's messages whose public text contains `needle`.
fn remove_messages(needle: &str) {
    with_current_room_mut(|room, _| {
        room.room_state.recent_messages.messages.retain(|m| {
            !m.message
                .content
                .as_public_string()
                .is_some_and(|text| text.contains(needle))
        });
    });
}

/// One reaction per emoji on the current room's newest message, in ONE mutation,
/// all held by Test Sender Insert. A send drops the example data's reactions, so a
/// spec that needs a row's chips to fill the line sets them here.
fn add_reactions_to_newest(emojis: Vec<String>) {
    with_current_room_mut(|room, room_key| {
        let Some(newest) = room
            .room_state
            .recent_messages
            .messages
            .last()
            .map(|m| m.id())
        else {
            web_sys::console::error_1(&"__riverTest.addReactionsToNewest: no messages".into());
            return;
        };
        let (sk, nickname) = test_author(Delivery::BeforeLast);
        admit_with_nickname(room, room_key, &sk, nickname);
        let reactor = MemberId::from(&sk.verifying_key());
        // One member holding several reactions breaks the one-per-member rule
        // on purpose: only the chips' geometry matters here.
        let reactions = room
            .room_state
            .recent_messages
            .actions_state
            .reactions
            .entry(newest)
            .or_default();
        for emoji in emojis {
            reactions.insert(emoji, vec![reactor]);
        }
    });
}

/// Deliver every message to the current room in ONE `ROOMS` mutation, so one
/// re-render, as a network delta does.
fn deliver(messages: impl IntoIterator<Item = (String, Delivery)>) {
    with_current_room_mut(|room, room_key| {
        for (text, delivery) in messages {
            push_test_message(room, room_key, text, delivery);
        }
        prune_to_cap(room);
    });
}

/// A fixed test identity on the other side of a DM thread.
struct DmPeer {
    seed: [u8; 32],
    /// What the specs open the thread by, so no nickname may contain another.
    nickname: &'static str,
    /// Prefix of the DMs `append_dm_history` seeds, "<prefix> NN".
    history_label: &'static str,
}

/// The DM hooks' test peers, by index: a spec that needs a second thread in the
/// same room uses peer 1.
const DM_PEERS: [DmPeer; 2] = [
    DmPeer {
        seed: [0x9D; 32],
        nickname: "DM Test Peer",
        history_label: "dm history",
    },
    DmPeer {
        seed: [0xAE; 32],
        nickname: "Other DM Peer",
        history_label: "other dm history",
    },
];

/// The test peer at `index`; throws to the caller for any other index.
fn dm_peer(index: u32) -> &'static DmPeer {
    DM_PEERS.get(index as usize).unwrap_or_else(|| {
        wasm_bindgen::throw_str(&format!(
            "__riverTest DM hooks: no test peer {index} (there are {})",
            DM_PEERS.len()
        ))
    })
}

/// `count` DMs from `peer` labelled "<history_label> NN", a minute apart and
/// all in the past, in ONE mutation (see `appendDmsForPeer`).
fn append_dm_history(peer: &DmPeer, count: u32) {
    deliver_dms(
        peer,
        (0..count)
            .map(|i| format!("{} {i:02}", peer.history_label))
            .collect(),
        DmStamp::History,
    )
}

/// How a DM delivery is timestamped.
#[derive(Clone, Copy)]
enum DmStamp {
    /// A minute apart, the last one a minute ago.
    History,
    /// The second half between local midnight and now (a minute apart when
    /// there is room, closer just after midnight), the first half the same
    /// times a day earlier: today and yesterday at any time of day.
    TwoDays,
    /// Now, or one second after the thread's newest DM if that is later, so
    /// the delivery sorts last even within one second of the previous one
    /// (the thread orders same-second DMs by signature).
    Newest,
}

/// The `i`th of `count` `DmStamp::TwoDays` timestamps: the second half's
/// slots fall between local midnight and `now`, and the first half reuses
/// the same slots a day earlier, so the thread spans exactly yesterday and
/// today whatever the time (a spec stamped "now less a few minutes" failed
/// just after midnight).
fn two_day_stamp(i: u64, count: u64, now: u64) -> u64 {
    let midnight = {
        let d = js_sys::Date::new_0();
        d.set_hours(0);
        d.set_minutes(0);
        d.set_seconds(0);
        d.set_milliseconds(0);
        (d.get_time() / 1000.0) as u64
    };
    let half = count / 2;
    let today = count - half;
    let slot = if i < half { i } else { i - half };
    let spacing = (now.saturating_sub(midnight) / (today + 1)).min(60);
    let at = now.saturating_sub((today - slot) * spacing);
    if i < half {
        // The same wall-clock time on the previous calendar day: the day
        // after a DST change, yesterday was not 86,400 s (nor midnight plus
        // today's offset) long.
        let d = js_sys::Date::new(&JsValue::from_f64(at as f64 * 1000.0));
        d.set_date(d.get_date() - 1);
        (d.get_time() / 1000.0) as u64
    } else {
        at
    }
}

/// Deliver `texts` from `peer` to self in the current room, in ONE `ROOMS`
/// mutation, through the DM field's `apply_delta`: it checks each signature
/// and both memberships, and keeps the stored order and caps.
fn deliver_dms(peer: &DmPeer, texts: Vec<String>, stamp: DmStamp) {
    with_current_room_mut(|room, room_key| {
        let room_key = *room_key;
        let Some(self_vk) = room.signing_key().map(|sk| sk.verifying_key()) else {
            web_sys::console::error_1(&"__riverTest DM hooks: self holds no key here".into());
            return;
        };
        let peer_sk = SigningKey::from_bytes(&peer.seed);
        admit_with_nickname(room, &room_key, &peer_sk, peer.nickname);

        let self_id = MemberId::from(&self_vk);
        let peer_id = MemberId::from(&peer_sk.verifying_key());
        let now = crate::util::get_current_system_time()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let count = texts.len() as u64;
        let (first, step) = match stamp {
            DmStamp::History | DmStamp::TwoDays => (now.saturating_sub(count * 60), 60),
            DmStamp::Newest => (
                room.room_state
                    .direct_messages
                    .messages
                    .iter()
                    .filter(|m| {
                        let (s, r) = (m.message.sender, m.message.recipient);
                        (s == self_id && r == peer_id) || (s == peer_id && r == self_id)
                    })
                    .map(|m| m.message.timestamp + 1)
                    .fold(now, u64::max),
                1,
            ),
        };
        let new_messages: Vec<_> = texts
            .into_iter()
            .zip(0..)
            .filter_map(|(text, i)| {
                let ts = match stamp {
                    DmStamp::TwoDays => two_day_stamp(i, count, now),
                    _ => first + i * step,
                };
                compose_direct_message(&peer_sk, &self_vk, &room_key, ts, now, text.as_bytes())
                    .inspect_err(|e| {
                        web_sys::console::error_1(&format!("__riverTest DM compose: {e}").into())
                    })
                    .ok()
            })
            .collect();
        let expected = new_messages.len();
        let delta = Some(DirectMessagesDelta {
            new_messages,
            advanced_purges: vec![],
        });
        let parent = room.room_state.clone();
        let params = ChatRoomParametersV1 { owner: room_key };
        let held_before = room.room_state.direct_messages.messages.len();
        if let Err(e) = room
            .room_state
            .direct_messages
            .apply_delta(&parent, &params, &delta)
        {
            web_sys::console::error_1(&format!("__riverTest DM apply: {e}").into());
        } else if room.room_state.direct_messages.messages.len() != held_before + expected {
            web_sys::console::error_1(&"__riverTest DM apply dropped a delivery".into());
        }
    });
}
