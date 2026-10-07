//! Browser hooks (`window.__riverTest`) the Playwright specs use to deliver
//! INBOUND messages, remove messages, and drive the no-room screen's load
//! states.
//!
//! The composer is not a substitute: an own send is an explicit request to go
//! to the newest message (10c decision 3), so a message sent through the UI
//! proves nothing about how an arriving one behaves. These write straight into
//! `ROOMS`, as an arriving network update does.
//!
//! A real arrival's author is a member, because the contract only keeps
//! messages signed by a member or the owner, so the hooks admit their test
//! identities as members too (see `admit_test_member`). Without that, the
//! first message the reader sends re-applies that rule locally and drops every
//! message the hooks delivered.
//!
//! None of these hooks scroll. Where the view ends up is the app's doing.
//!
//! They skip `apply_delta`'s verification on purpose, and admit test
//! identities on self's say-so, so they must never be reachable anywhere the
//! result could be pushed to the network. The module compiles only for wasm32
//! with two features, both required:
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
use river_core::room_state::{
    configuration::AuthorizedConfigurationV1,
    member::{AuthorizedMember, Member, MemberId},
    member_info::{AuthorizedMemberInfo, MemberInfo},
    message::{AuthorizedMessageV1, MessageV1, RoomMessageBody},
    privacy::{PrivacyMode, SealedBytes},
};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::convert::FromWasmAbi;
use wasm_bindgen::JsValue;

pub fn install_test_hooks() {
    let Some(window) = web_sys::window() else {
        return;
    };
    // Idempotent: `App` may re-render, and re-installing would leak closures.
    if js_sys::Reflect::has(&window, &JsValue::from_str("__riverTest")).unwrap_or(false) {
        return;
    }

    fn expose<A: FromWasmAbi + 'static>(
        hooks: &js_sys::Object,
        name: &str,
        hook: impl FnMut(A) + 'static,
    ) {
        let hook = Closure::<dyn FnMut(A)>::new(hook).into_js_value();
        let _ = js_sys::Reflect::set(hooks, &JsValue::from_str(name), &hook);
    }

    fn expose2<A: FromWasmAbi + 'static, B: FromWasmAbi + 'static>(
        hooks: &js_sys::Object,
        name: &str,
        hook: impl FnMut(A, B) + 'static,
    ) {
        let hook = Closure::<dyn FnMut(A, B)>::new(hook).into_js_value();
        let _ = js_sys::Reflect::set(hooks, &JsValue::from_str(name), &hook);
    }

    let hooks = js_sys::Object::new();

    expose(&hooks, "appendMessage", move |text: String| {
        crate::util::defer(move || deliver([(text, Delivery::Append)]));
    });

    // An arrival from a member whose clock runs `seconds` ahead of ours. Up to
    // the 60s skew tolerance in conversation.rs it keeps its own timestamp, so
    // a message sent here afterwards sorts ABOVE it (the A06 clock-skew case).
    expose2(
        &hooks,
        "appendMessageAhead",
        move |text: String, seconds: u32| {
            crate::util::defer(move || deliver([(text, Delivery::Ahead(seconds))]));
        },
    );

    expose(&hooks, "insertMessageBeforeLast", move |text: String| {
        crate::util::defer(move || deliver([(text, Delivery::BeforeLast)]));
    });

    // A burst in ONE state mutation — one delta application, one re-render —
    // as a network delta carrying many messages produces. The windowing specs
    // use it to grow an anchored window well past one backfill step without
    // paying per-delivery render round-trips (#505 blocker 2).
    expose(&hooks, "appendMessages", move |count: u32| {
        crate::util::defer(move || {
            deliver((0..count).map(|i| (format!("batched arrival {i:02}"), Delivery::Append)))
        });
    });

    // Remove every message in the current room whose text contains the given
    // substring, in ONE mutation — the shape of a ban purge or a retention
    // drain, which take messages out of the list rather than adding a delete
    // action. The scroll specs use it for deletions above and at the reader.
    expose(&hooks, "removeMessages", move |needle: String| {
        crate::util::defer(move || remove_messages(&needle));
    });

    // Put the current room in the state of a private room whose secret this
    // device does not hold. A send then falls back to a public body, which
    // the send's own local `apply_delta` rejects ("Cannot send public messages
    // in private room"), so the send fails AFTER the composer has handed it
    // off: the failed-send case of the A06 specs. The rendered history does
    // not change, since every example message body is public.
    //
    // Only for a room self owns: the configuration is re-signed with the
    // owner's key, because a configuration that fails its signature check
    // reads as a room still awaiting its initial sync, which has no composer.
    expose(&hooks, "makeRoomPrivateWithoutSecret", move |_: JsValue| {
        crate::util::defer(|| {
            let Some(room_key) = CURRENT_ROOM.peek().owner_key else {
                return;
            };
            ROOMS.with_mut(|rooms| {
                let Some(room) = rooms.map.get_mut(&room_key) else {
                    return;
                };
                let Some(owner_sk) = room
                    .signing_key()
                    .filter(|sk| sk.verifying_key() == room_key)
                    .cloned()
                else {
                    web_sys::console::error_1(
                        &"__riverTest.makeRoomPrivateWithoutSecret: self must own the room".into(),
                    );
                    return;
                };
                let mut config = room.room_state.configuration.configuration.clone();
                config.privacy_mode = PrivacyMode::Private;
                room.room_state.configuration = AuthorizedConfigurationV1::new(config, &owner_sk);
                room.secrets.clear();
                room.current_secret_version = None;
            });
        });
    });

    // Drive the no-room screen's load states (freenet/river#509), which the
    // example build, always seeded with rooms, never leaves. Clears ROOMS too:
    // `room_list_display_state` renders the list whenever there is anything to show.
    expose(&hooks, "setRoomsLoadState", move |state: String| {
        let parsed = match state.as_str() {
            "loading" => RoomsLoadState::Loading,
            "migrating" => RoomsLoadState::Migrating,
            "failed" => RoomsLoadState::LoadFailed,
            "loaded" => RoomsLoadState::Loaded,
            other => wasm_bindgen::throw_str(&format!("unknown rooms load state {other:?}")),
        };
        crate::util::defer(move || {
            ROOMS.with_mut(|rooms| {
                rooms.map.clear();
                rooms.room_order.clear();
                rooms.current_room_key = None;
            });
            *CURRENT_ROOM.write() = CurrentRoom { owner_key: None };
            *ROOMS_LOAD_STATE.write() = parsed;
        });
    });

    // Switch rooms the way a notification click does: CURRENT_ROOM changes
    // while whatever modal is open stays open. The room list is behind the
    // modal's backdrop, so the UI itself cannot do this from a test.
    expose(&hooks, "switchRoom", move |name: String| {
        // Everything inside `defer`: reading ROOMS needs the Dioxus runtime, which
        // a raw JS call does not have (and `panic = "abort"` would kill the page).
        crate::util::defer(move || {
            let key = ROOMS.peek().map.iter().find_map(|(key, room)| {
                let sealed = &room.room_state.configuration.configuration.display.name;
                let shown = crate::util::ecies::unseal_bytes_with_secrets(sealed, &room.secrets)
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
    });

    let _ = js_sys::Reflect::set(&window, &JsValue::from_str("__riverTest"), &hooks);
}

/// Where a delivered message lands in the room's message list.
#[derive(Clone, Copy)]
enum Delivery {
    /// At the end, as an ordinary arrival does.
    Append,
    /// One position from the end — the mid-list insert that grows the history
    /// WITHOUT remounting its last row, which is the case the old
    /// `onmounted`-on-the-last-bubble scroll trigger could not see.
    BeforeLast,
    /// At the end, stamped this many seconds ahead of our clock.
    Ahead(u32),
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
/// windowing's grow/trim interleave zero times (#505 review).
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
        // Its own identity, so it never groups with the arrival before it.
        Delivery::Ahead(_) => (SigningKey::from_bytes(&[0x8C; 32]), "Test Sender Ahead"),
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

/// Insert `text` into `room` where `delivery` says, as that delivery's test
/// identity, admitting it as a member (see `admit_test_member`) and
/// registering its nickname the first time it speaks so the bubble renders
/// like any other member's rather than as "Unknown".
fn push_test_message(
    room: &mut RoomData,
    room_key: &VerifyingKey,
    text: String,
    delivery: Delivery,
) {
    let (sk, nickname) = test_author(delivery);
    let author = MemberId::from(&sk.verifying_key());
    admit_test_member(room, room_key, sk.verifying_key());
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
                &sk,
            ));
    }

    let now = crate::util::get_current_system_time();
    let time = match delivery {
        Delivery::Ahead(seconds) => now + std::time::Duration::from_secs(seconds.into()),
        Delivery::Append | Delivery::BeforeLast => now,
    };
    let message = AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: MemberId::from(room_key),
            author,
            content: RoomMessageBody::public(text),
            time,
        },
        &sk,
    );

    let messages = &mut room.room_state.recent_messages.messages;
    let at = match delivery {
        Delivery::Append | Delivery::Ahead(_) => messages.len(),
        Delivery::BeforeLast => messages.len().saturating_sub(1),
    };
    messages.insert(at, message);
}

/// What a real arrival does once the room is at `max_recent_messages`: drain
/// the oldest. Mirrors `MessagesV1::apply_delta`
/// (common/src/room_state/message.rs) — without this, the hooks grow the
/// message list unboundedly and the at-cap index-shift path (#505 blocker 1)
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

/// Drop the current room's messages whose public text contains `needle`.
fn remove_messages(needle: &str) {
    let Some(room_key) = CURRENT_ROOM.peek().owner_key else {
        return;
    };
    ROOMS.with_mut(|rooms| {
        if let Some(room) = rooms.map.get_mut(&room_key) {
            room.room_state.recent_messages.messages.retain(|m| {
                !m.message
                    .content
                    .as_public_string()
                    .is_some_and(|text| text.contains(needle))
            });
        }
    });
}

/// Deliver every message to the current room in ONE `ROOMS` mutation, so one
/// re-render, as a network delta does.
fn deliver(messages: impl IntoIterator<Item = (String, Delivery)>) {
    let Some(room_key) = CURRENT_ROOM.peek().owner_key else {
        return;
    };
    ROOMS.with_mut(|rooms| {
        let Some(room) = rooms.map.get_mut(&room_key) else {
            return;
        };
        for (text, delivery) in messages {
            push_test_message(room, &room_key, text, delivery);
        }
        prune_to_cap(room);
    });
}
