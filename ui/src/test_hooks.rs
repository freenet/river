//! Browser hooks (`window.__riverTest`) the Playwright specs use to deliver
//! INBOUND messages and to drive the no-room screen's load states.
//!
//! The composer is not a substitute: `handle_send_message` raises
//! `force_scroll`, which deliberately bypasses the pin that the scroll specs
//! exist to test, so a message sent through the UI proves nothing about how an
//! arriving one behaves. These write straight into `ROOMS`, as an arriving
//! network update does.
//!
//! They skip `apply_delta`'s verification on purpose, so they must never be
//! reachable anywhere the result could be pushed to the network. The module
//! compiles only for wasm32 with two features, both required:
//!
//! * `example-data`, which is off for the published webapp (`UI_FEATURES` is
//!   empty for `build-webapp`) and for every non-example build;
//! * `no-sync`, because `cargo make dev-example` turns `example-data` on
//!   WITHOUT it. A message minted here is signed by a key that is not a room
//!   member, so a developer pointing that build at a live node would otherwise
//!   have the synchronizer try to push contract-invalid state.

use crate::components::app::chat_delegate::{RoomsLoadState, ROOMS_LOAD_STATE};
use crate::components::app::{CURRENT_ROOM, ROOMS};
use crate::room_data::{CurrentRoom, RoomData};
use dioxus::prelude::*;
use ed25519_dalek::{SigningKey, VerifyingKey};
use river_core::room_state::{
    member::MemberId,
    member_info::{AuthorizedMemberInfo, MemberInfo},
    message::{AuthorizedMessageV1, MessageV1, RoomMessageBody},
    privacy::SealedBytes,
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

    let hooks = js_sys::Object::new();

    expose(&hooks, "appendMessage", move |text: String| {
        crate::util::defer(move || deliver([(text, Delivery::Append)]));
    });

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
    }
}

/// Insert `text` into `room` where `delivery` says, as that delivery's test
/// identity, registering the identity's nickname the first time it speaks so
/// the bubble renders like any other member's rather than as "Unknown".
fn push_test_message(
    room: &mut RoomData,
    room_key: &VerifyingKey,
    text: String,
    delivery: Delivery,
) {
    let (sk, nickname) = test_author(delivery);
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
                &sk,
            ));
    }

    let message = AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: MemberId::from(room_key),
            author,
            content: RoomMessageBody::public(text),
            time: crate::util::get_current_system_time(),
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
