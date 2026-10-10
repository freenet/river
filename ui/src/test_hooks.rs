//! Browser hooks (`window.__riverTest`) the Playwright specs use to deliver
//! INBOUND messages, remove messages, and drive the no-room screen's load
//! states.
//!
//! The composer is not a substitute: an own send is an explicit request to go
//! to the newest message, so a message sent through the UI
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
//! The DM hooks (`appendDms`, `deliverDm`) do the same for a DM thread between
//! self and one fixed test identity, which they admit like the others. Unlike
//! the room hooks they run the DM field's own `apply_delta`, so every DM is
//! sealed to self, sender-signed, and kept in the contract's order and caps.
//!
//! `appendDmsForPeer` and `deliverDmForPeer` do the same for a second fixed
//! test identity (peer 1), so a spec can switch between two threads; peer 0 is
//! the identity `appendDms` and `deliverDm` use.
//!
//! `failNextDmRoomRead` makes the open DM thread's next `ROOMS` read fail, as
//! a contended one does, and `dmRoomReadFailuresTaken` reports when it has.
//!
//! `holdNextDmPlacement` holds the next DM thread placement (the opening or
//! own-send jump) after its task starts and before it runs, so a spec can
//! unmount the thread underneath it; `releaseHeldDmPlacement` runs it.
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

    fn expose_getter(hooks: &js_sys::Object, name: &str, hook: impl FnMut() -> u32 + 'static) {
        let hook = Closure::<dyn FnMut() -> u32>::new(hook).into_js_value();
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

    // A DM history from the test peer to self, in ONE mutation: `count`
    // messages labelled `dm history NN`, a minute apart and all in the past,
    // so a DM sent or delivered afterwards sorts below them. Long enough at 30
    // to make the thread scroll on every test viewport.
    expose(&hooks, "appendDms", move |count: u32| {
        crate::util::defer(move || append_dm_history(&DM_PEERS[0], count));
    });

    // One inbound DM from the test peer, stamped after every DM in the thread
    // so it renders as the newest.
    expose(&hooks, "deliverDm", move |text: String| {
        crate::util::defer(move || deliver_dms(&DM_PEERS[0], vec![text], DmStamp::Newest));
    });

    // `appendDms` and `deliverDm` for a chosen test peer: 0 is the one those
    // use, 1 a second identity with its own thread.
    expose2(&hooks, "appendDmsForPeer", move |peer: u32, count: u32| {
        let peer = dm_peer(peer);
        crate::util::defer(move || append_dm_history(peer, count));
    });
    expose2(
        &hooks,
        "deliverDmForPeer",
        move |peer: u32, text: String| {
            let peer = dm_peer(peer);
            crate::util::defer(move || deliver_dms(peer, vec![text], DmStamp::Newest));
        },
    );

    // Admit a test peer to the current room with no DMs, so a spec can open
    // an empty thread with them from Member Info.
    expose(&hooks, "admitDmPeer", move |peer: u32| {
        let peer = dm_peer(peer);
        crate::util::defer(move || {
            with_current_room_mut(|room, room_key| {
                let room_key = *room_key;
                let peer_sk = SigningKey::from_bytes(&peer.seed);
                admit_with_nickname(room, &room_key, &peer_sk, peer.nickname);
            })
        });
    });

    // Hold the NEXT DM thread placement (the opening or own-send jump) once
    // its task has started, before it touches the DOM, until
    // `releaseHeldDmPlacement`. One-shot: later placements run as usual.
    expose(&hooks, "holdNextDmPlacement", move |_: JsValue| {
        DM_PLACEMENT_HOLD_ARMED.with(|armed| armed.set(true));
    });

    // How many placements are held right now (0 or 1).
    expose_getter(&hooks, "heldDmPlacementCount", move || {
        HELD_DM_PLACEMENT.with(|held| u32::from(held.borrow().is_some()))
    });

    // Run the held placement, in a fresh task as it would have run. Throws if
    // nothing is held, so a release that does nothing cannot pass silently.
    expose(&hooks, "releaseHeldDmPlacement", move |_: JsValue| {
        // Out of the RefCell before it runs, so it cannot re-enter the borrow.
        let Some(action) = HELD_DM_PLACEMENT.with(|held| held.borrow_mut().take()) else {
            wasm_bindgen::throw_str("__riverTest.releaseHeldDmPlacement: no placement is held");
        };
        crate::util::safe_spawn_local(async move {
            RELEASED_DM_PLACEMENTS_RUN.with(|n| n.set(n.get() + 1));
            action();
        });
    });

    // How many released placements have run, so a test can wait for its
    // release to land rather than guess.
    expose_getter(&hooks, "releasedDmPlacementsRun", move || {
        RELEASED_DM_PLACEMENTS_RUN.with(|n| n.get())
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
            with_current_room_mut(|room, room_key| {
                let Some(owner_sk) = room
                    .signing_key()
                    .filter(|sk| sk.verifying_key() == *room_key)
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

    // Make the open DM thread's next `ROOMS` read take its failed-read branch,
    // as a contended read does, and nudge so that read happens. If no thread
    // is open, the next one to open takes it.
    expose(&hooks, "failNextDmRoomRead", move |_: JsValue| {
        DM_ROOM_READ_FAILURE.with(|f| f.set(true));
        crate::util::signal_guard::schedule_nudge();
    });

    // How many forced DM room-read failures have been taken, so a test can wait
    // for its failure to land rather than guess.
    expose_getter(&hooks, "dmRoomReadFailuresTaken", move || {
        DM_ROOM_READS_FAILED.with(|n| n.get())
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

thread_local! {
    /// A forced DM room-read failure is pending (`failNextDmRoomRead`).
    static DM_ROOM_READ_FAILURE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Forced DM room-read failures taken so far (`dmRoomReadFailuresTaken`).
    static DM_ROOM_READS_FAILED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// The next DM placement is to be held (`holdNextDmPlacement`).
    static DM_PLACEMENT_HOLD_ARMED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The held DM placement, as the thread queued it.
    static HELD_DM_PLACEMENT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
    /// Released DM placements that have run (`releasedDmPlacementsRun`).
    static RELEASED_DM_PLACEMENTS_RUN: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Run a DM thread placement now, or hold it if `holdNextDmPlacement` armed a
/// hold. The thread calls this from inside the placement's task.
pub(crate) fn run_or_hold_dm_placement(action: Box<dyn FnOnce()>) {
    if !DM_PLACEMENT_HOLD_ARMED.with(|armed| armed.replace(false)) {
        action();
        return;
    }
    let displaced = HELD_DM_PLACEMENT.with(|held| held.borrow_mut().replace(action));
    if displaced.is_some() {
        web_sys::console::error_1(
            &"__riverTest.holdNextDmPlacement: a held placement was never released".into(),
        );
    }
}

/// Take a pending forced failure for the DM thread's `ROOMS` read, counting it.
pub(crate) fn take_dm_room_read_failure() -> bool {
    let forced = DM_ROOM_READ_FAILURE.with(|f| f.replace(false));
    if forced {
        DM_ROOM_READS_FAILED.with(|n| n.set(n.get() + 1));
    }
    forced
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
    let (sk, nickname) = test_author(delivery);
    let author = MemberId::from(&sk.verifying_key());
    admit_with_nickname(room, room_key, &sk, nickname);

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

/// The DM hooks' test peers, by index. Peer 0 is the one `appendDms` and
/// `deliverDm` use (`dm-thread-scroll.spec.ts`); peer 1 gives a spec a second
/// thread in the same room (`dm-thread-lifecycle.spec.ts`).
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
/// all in the past, in ONE mutation (see `appendDms`).
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
    /// Now, or one second after the thread's newest DM if that is later, so
    /// the delivery sorts last even within one second of the previous one
    /// (the thread orders same-second DMs by signature).
    Newest,
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
            DmStamp::History => (now.saturating_sub(count * 60), 60),
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
                let ts = first + i * step;
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
