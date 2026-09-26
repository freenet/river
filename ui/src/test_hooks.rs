#[cfg(target_arch = "wasm32")]
use crate::room_data::RoomData;
#[cfg(target_arch = "wasm32")]
use ed25519_dalek::{SigningKey, VerifyingKey};
#[cfg(target_arch = "wasm32")]
use river_core::room_state::{
    member::MemberId,
    member_info::{AuthorizedMemberInfo, MemberInfo},
    message::{AuthorizedMessageV1, MessageV1, RoomMessageBody},
    privacy::SealedBytes,
};

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

    // Leaks one closure per hook; the idempotency guard above bounds it.
    fn expose<T: ?Sized + wasm_bindgen::closure::WasmClosure>(
        hooks: &js_sys::Object,
        name: &str,
        hook: Closure<T>,
    ) {
        let _ = js_sys::Reflect::set(hooks, &JsValue::from_str(name), hook.as_ref());
        hook.forget();
    }

    let hooks = js_sys::Object::new();

    expose(
        &hooks,
        "appendMessage",
        Closure::wrap(Box::new(move |text: String| {
            crate::util::defer(move || deliver_message(text, Delivery::Append));
        }) as Box<dyn FnMut(String)>),
    );

    expose(
        &hooks,
        "insertMessageBeforeLast",
        Closure::wrap(Box::new(move |text: String| {
            crate::util::defer(move || deliver_message(text, Delivery::BeforeLast));
        }) as Box<dyn FnMut(String)>),
    );

    // A burst in ONE state mutation — one delta application, one re-render —
    // as a network delta carrying many messages produces. The windowing specs
    // use it to grow an anchored window well past one backfill step without
    // paying per-delivery render round-trips (#505 blocker 2).
    expose(
        &hooks,
        "appendMessages",
        Closure::wrap(Box::new(move |count: u32| {
            crate::util::defer(move || deliver_batch(count as usize));
        }) as Box<dyn FnMut(u32)>),
    );

    // Drive the no-room screen's load states (freenet/river#509). The example
    // build always seeds rooms and never leaves `ROOMS_LOAD_STATE` anywhere
    // but its default, so Loading / Migrating / LoadFailed are otherwise
    // unreachable from a browser test — which is exactly why #397's states
    // shipped with Rust unit tests only, and why nobody noticed they were
    // invisible on mobile.
    //
    // Clears ROOMS as well as setting the state: `room_list_display_state`
    // renders the list whenever there is anything to show, so a state change
    // alone would change nothing.
    expose(
        &hooks,
        "setRoomsLoadState",
        Closure::wrap(Box::new(move |state: String| {
            use crate::components::app::chat_delegate::{RoomsLoadState, ROOMS_LOAD_STATE};
            let parsed = match state.as_str() {
                "loading" => RoomsLoadState::Loading,
                "migrating" => RoomsLoadState::Migrating,
                "failed" => RoomsLoadState::LoadFailed,
                "loaded" => RoomsLoadState::Loaded,
                other => {
                    crate::util::debug_log(&format!("[test] unknown rooms load state {other:?}"));
                    return;
                }
            };
            crate::util::defer(move || {
                crate::components::app::ROOMS.with_mut(|rooms| {
                    rooms.map.clear();
                    rooms.room_order.clear();
                    rooms.current_room_key = None;
                });
                *crate::components::app::CURRENT_ROOM.write() =
                    crate::room_data::CurrentRoom { owner_key: None };
                *ROOMS_LOAD_STATE.write() = parsed;
            });
        }) as Box<dyn FnMut(String)>),
    );

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
