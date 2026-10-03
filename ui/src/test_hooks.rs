//! Browser hooks (`window.__riverTest`) the Playwright specs use to deliver
//! INBOUND messages, to drive the no-room screen's load states, and to render
//! a room's history empty without touching its messages.
//!
//! The composer is not a substitute: a message sent through the UI is the
//! reader's own, and goes through `apply_delta`'s verification, which drops
//! messages these hooks minted (their authors are not room members). These
//! write straight into `ROOMS`, as an arriving network update does.
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
use wasm_bindgen::convert::{FromWasmAbi, ReturnWasmAbi};
use wasm_bindgen::JsValue;

/// The room whose history `setHistoryEmpty` holds empty: the render the
/// `message_groups` memo produces when a contended read leaves it `None`
/// (#555) while the room's messages are all still there. Nothing is deleted,
/// so clearing it renders the identical rows and anchor keys again. Keyed by
/// room, and cleared by a room switch (`end_history_renders_empty`), so it
/// never outlives the room it was set in.
static HISTORY_RENDERS_EMPTY: GlobalSignal<Option<VerifyingKey>> = Global::new(|| None);

/// Whether `setHistoryEmpty` holds `room`'s history empty. Read by the
/// `message_groups` memo after its anchor, so the memo subscribes to it. A
/// contended read is not "empty": it nudges, as every fallible memo read must
/// (.claude/rules/dioxus-signal-safety.md), and the rows render.
pub fn history_renders_empty(room: Option<VerifyingKey>) -> bool {
    match HISTORY_RENDERS_EMPTY.try_read() {
        Ok(emptied) => emptied.is_some() && *emptied == room,
        Err(_) => {
            crate::util::signal_guard::schedule_nudge();
            false
        }
    }
}

/// A room switch ends `setHistoryEmpty`. Called from the render that switches,
/// so the write is deferred.
pub fn end_history_renders_empty() {
    if HISTORY_RENDERS_EMPTY.peek().is_some() {
        crate::util::defer(|| *HISTORY_RENDERS_EMPTY.write() = None);
    }
}

/// Exercise the real history controller at a chosen DOM boundary. These
/// callbacks mutate no application state directly: controller signal work
/// still goes through `defer`. Registration happens once, after its install.
pub fn install_history_scroll_probe(take_in: impl Fn() + 'static, restore: impl Fn() + 'static) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Ok(hooks) = js_sys::Reflect::get(&window, &JsValue::from_str("__riverTest")) else {
        return;
    };
    if !hooks.is_object()
        || js_sys::Reflect::has(&hooks, &JsValue::from_str("takeInPendingHistoryScroll"))
            .unwrap_or(false)
    {
        return;
    }
    let take_in = Closure::<dyn FnMut()>::new(take_in).into_js_value();
    let restore = Closure::<dyn FnMut()>::new(restore).into_js_value();
    let _ = js_sys::Reflect::set(
        &hooks,
        &JsValue::from_str("takeInPendingHistoryScroll"),
        &take_in,
    );
    let _ = js_sys::Reflect::set(
        &hooks,
        &JsValue::from_str("restoreHistoryPosition"),
        &restore,
    );
}

pub fn install_test_hooks() {
    let Some(window) = web_sys::window() else {
        return;
    };
    // Idempotent: `App` may re-render, and re-installing would leak closures.
    if js_sys::Reflect::has(&window, &JsValue::from_str("__riverTest")).unwrap_or(false) {
        return;
    }

    fn expose<A: FromWasmAbi + 'static, R: ReturnWasmAbi + 'static>(
        hooks: &js_sys::Object,
        name: &str,
        hook: impl FnMut(A) -> R + 'static,
    ) {
        let hook = Closure::<dyn FnMut(A) -> R>::new(hook).into_js_value();
        let _ = js_sys::Reflect::set(hooks, &JsValue::from_str(name), &hook);
    }

    let hooks = js_sys::Object::new();

    expose(&hooks, "appendMessage", move |text: String| {
        crate::util::defer(move || deliver([(RoomMessageBody::public(text), Delivery::Append)]));
    });

    expose(&hooks, "insertMessageBeforeLast", move |text: String| {
        crate::util::defer(move || {
            deliver([(RoomMessageBody::public(text), Delivery::BeforeLast)])
        });
    });

    // A burst in ONE state mutation — one delta application, one re-render —
    // as a network delta carrying many messages produces. The windowing specs
    // use it to grow an anchored window well past one backfill step without
    // paying per-delivery render round-trips (#505 blocker 2).
    expose(&hooks, "appendMessages", move |count: u32| {
        crate::util::defer(move || {
            deliver((0..count).map(|i| {
                (
                    RoomMessageBody::public(format!("batched arrival {i:02}")),
                    Delivery::Append,
                )
            }))
        });
    });

    // A join event at the end, which renders as an event-summary row: no
    // example room seeds one, and the anchor-row spec needs every row kind.
    expose(&hooks, "appendJoinEvent", move |_: JsValue| {
        crate::util::defer(move || deliver([(RoomMessageBody::join_event(), Delivery::Append)]));
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

    // Remove messages by their row's DOM id (`msg-{id}`), all in ONE `ROOMS`
    // mutation, as a delta that drops several messages renders. Only the
    // rendering is simulated: nothing here checks that a removal is authorized.
    // Resolves with the ids that matched no message, so a fixture that removes
    // nothing fails instead of passing quietly.
    expose(&hooks, "removeMessages", move |dom_ids: JsValue| {
        let wanted: Vec<String> = js_sys::Array::from(&dom_ids)
            .iter()
            .filter_map(|id| id.as_string())
            .collect();
        let mut resolve = None;
        let promise = js_sys::Promise::new(&mut |res, _rej| resolve = Some(res));
        let resolve = resolve.expect("Promise::new runs its executor synchronously");
        // Reading ROOMS needs the Dioxus runtime, which a raw JS call lacks.
        crate::util::defer(move || {
            let unmatched: js_sys::Array = remove(wanted)
                .into_iter()
                .map(|id| JsValue::from_str(&id))
                .collect();
            let _ = resolve.call1(&JsValue::NULL, &unmatched);
        });
        promise
    });

    // Render the current room's history empty (`true`) or with its rows again
    // (`false`) through the real `message_groups` memo and render arm. The
    // promise resolves once the deferred state change has run; it says nothing
    // about whether the render has reached the DOM.
    expose(&hooks, "setHistoryEmpty", move |on: bool| {
        let mut resolve = None;
        let promise = js_sys::Promise::new(&mut |res, _rej| resolve = Some(res));
        let resolve = resolve.expect("Promise::new runs its executor synchronously");
        // Reading CURRENT_ROOM needs the Dioxus runtime, which a raw JS call lacks.
        crate::util::defer(move || {
            *HISTORY_RENDERS_EMPTY.write() = if on {
                CURRENT_ROOM.peek().owner_key
            } else {
                None
            };
            let _ = resolve.call0(&JsValue::NULL);
        });
        promise
    });

    let _ = js_sys::Reflect::set(&window, &JsValue::from_str("__riverTest"), &hooks);
}

/// Remove every message whose row id is in `dom_ids`, in one `ROOMS` mutation.
/// The current room is searched first. Ids it does not hold are removed from
/// another visited room, so a snapshot saved on leaving can be invalidated
/// while that room is not on screen. Returns the ids that matched nothing.
fn remove(dom_ids: Vec<String>) -> Vec<String> {
    fn drop_matching(room: &mut RoomData, unmatched: &mut std::collections::BTreeSet<String>) {
        room.room_state
            .recent_messages
            .messages
            .retain(|message| !unmatched.remove(&format!("msg-{:?}", message.id().0)));
    }

    let mut unmatched: std::collections::BTreeSet<String> = dom_ids.into_iter().collect();
    let current = CURRENT_ROOM.peek().owner_key;
    ROOMS.with_mut(|rooms| {
        if let Some(key) = current {
            if let Some(room) = rooms.map.get_mut(&key) {
                drop_matching(room, &mut unmatched);
            }
        }
        if unmatched.is_empty() {
            return;
        }
        for (key, room) in rooms.map.iter_mut() {
            if Some(*key) == current || unmatched.is_empty() {
                continue;
            }
            drop_matching(room, &mut unmatched);
        }
    });
    unmatched.into_iter().collect()
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

/// Insert `content` into `room` where `delivery` says, as that delivery's test
/// identity, registering the identity's nickname the first time it speaks so
/// the bubble renders like any other member's rather than as "Unknown".
fn push_test_message(
    room: &mut RoomData,
    room_key: &VerifyingKey,
    content: RoomMessageBody,
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
            content,
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
/// re-render, as a network delta does. A no-op without a loaded current room.
/// Needs the Dioxus runtime: call it from inside `defer`.
fn deliver(messages: impl IntoIterator<Item = (RoomMessageBody, Delivery)>) {
    let Some(room_key) = CURRENT_ROOM.peek().owner_key else {
        return;
    };
    ROOMS.with_mut(|rooms| {
        let Some(room) = rooms.map.get_mut(&room_key) else {
            return;
        };
        for (content, delivery) in messages {
            push_test_message(room, &room_key, content, delivery);
        }
        prune_to_cap(room);
    });
}
