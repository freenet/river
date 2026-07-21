pub(crate) mod create_room_modal;
pub(crate) mod dm_rail_section;
pub(crate) mod edit_room_modal;
pub(crate) mod join_with_code_modal;
pub(crate) mod notification_modal;
pub(crate) mod receive_invitation_modal;
pub(crate) mod room_name_field;

use crate::components::app::chat_delegate::{
    retry_rooms_load, save_rooms_to_delegate, RoomsLoadState, ROOMS_LOAD_STATE,
};
use crate::components::app::document_title::{
    count_unread_in_room_data, mark_current_room_as_read,
};
use crate::components::app::sync_info::{RoomSyncStatus, SYNC_INFO};
use crate::components::app::{MobileView, CREATE_ROOM_MODAL, CURRENT_ROOM, MOBILE_VIEW, ROOMS};
use crate::components::members::{ConnectionStatusIndicator, ImportIdentityModal};
use crate::components::room_list::dm_rail_section::DmRailSection;
use crate::components::room_list::join_with_code_modal::JoinWithCodeModal;
use crate::room_data::CurrentRoom;
use crate::util::ecies::unseal_bytes_with_secrets;
use dioxus::logger::tracing::error;
use dioxus::prelude::*;
use dioxus_free_icons::{
    icons::fa_solid_icons::{
        FaArrowLeft, FaArrowsUpDown, FaChevronDown, FaChevronUp, FaComments, FaFileImport, FaLock,
        FaPlus, FaRightToBracket, FaTriangleExclamation,
    },
    Icon,
};
use ed25519_dalek::VerifyingKey;
use river_core::room_state::privacy::PrivacyMode;

// Access the build timestamp (ISO 8601 format) environment variable set by build.rs
const BUILD_TIMESTAMP_ISO: &str = env!("BUILD_TIMESTAMP_ISO", "Build timestamp not set");

// Stable Dioxus key for the reorder tail drop zone (keys must be unique and
// distinct from any room's `{room_key:?}`).
const TAIL_DROP_ZONE_KEY: &str = "room-reorder-tail-drop-zone";

// Stable Dioxus key for the single non-room state row (freenet/river#397). The
// three states are mutually exclusive, so they share one key and Dioxus reuses
// the element across transitions.
const ROOM_LIST_STATE_KEY: &str = "room-list-state";

/// What the rooms rail shows in place of the room list (freenet/river#397).
/// When the `ROOMS` map is empty during startup / migration, a bare `<ul>`
/// rendered nothing, so a user with rooms still loading saw a blank list and
/// assumed data loss. This picks one of three explicit non-room states; a
/// non-empty list always wins.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RoomListDisplay {
    /// Initial load hasn't resolved yet — subtle spinner + "Loading your rooms…".
    Loading,
    /// A delegate/room migration is recovering rooms — spinner + "Migrating…".
    Migrating,
    /// Load resolved and the user genuinely has no rooms — calm empty state.
    Empty,
    /// A load we KNOW involved rooms failed/stalled — error + Retry block, never
    /// a false "no rooms yet" (freenet/river#397 Codex review 4).
    LoadFailed,
    /// There is at least one room to render — show the list unchanged.
    List,
}

/// Pure decision for the rail's display state, split out so it is unit-testable
/// without the Dioxus runtime (freenet/river#397).
///
/// A non-empty room list ALWAYS renders the list — once there's something to
/// show, load/migration bookkeeping is irrelevant. Only when `room_count == 0`
/// do we disambiguate the non-room states: `Loading`/`Migrating` spinners, the
/// `LoadFailed` error+retry block, and the calm `Empty`. `Empty` is reached ONLY
/// from `Loaded`, so an unresolved / migrating / failed load never shows "no
/// rooms yet".
pub(crate) fn room_list_display_state(
    load_state: RoomsLoadState,
    room_count: usize,
) -> RoomListDisplay {
    if room_count > 0 {
        RoomListDisplay::List
    } else {
        match load_state {
            RoomsLoadState::Loading => RoomListDisplay::Loading,
            RoomsLoadState::Migrating => RoomListDisplay::Migrating,
            RoomsLoadState::Loaded => RoomListDisplay::Empty,
            RoomsLoadState::LoadFailed => RoomListDisplay::LoadFailed,
        }
    }
}

/// Convert UTC ISO timestamp to local time string
fn format_build_time_local() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        use js_sys::Date;
        let date = Date::new(&wasm_bindgen::JsValue::from_str(BUILD_TIMESTAMP_ISO));
        if date.to_string().as_string().is_some() {
            // Format as "YYYY-MM-DD HH:MM" in local time
            let year = date.get_full_year();
            let month = date.get_month() + 1; // JS months are 0-indexed
            let day = date.get_date();
            let hours = date.get_hours();
            let minutes = date.get_minutes();
            let offset_min = date.get_timezone_offset() as i32;
            let tz_str = if offset_min == 0 {
                "UTC".to_string()
            } else {
                // getTimezoneOffset returns minutes west of UTC, so negate
                let sign = if offset_min <= 0 { '+' } else { '-' };
                let abs = offset_min.unsigned_abs();
                format!("UTC{}{}", sign, abs / 60)
            };
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02} {}",
                year, month, day, hours, minutes, tz_str
            )
        } else {
            BUILD_TIMESTAMP_ISO.to_string()
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        BUILD_TIMESTAMP_ISO.to_string()
    }
}

#[component]
pub fn RoomList() -> Element {
    let mut import_modal_active = use_signal(|| false);
    let mut join_code_modal_active = use_signal(|| false);

    // Drag-and-drop reorder state (a local view preference). `dragged_room`
    // is the row currently being dragged; `drag_over_room` is the row the
    // cursor is hovering over, used to draw the insertion indicator;
    // `drag_over_end` is the same for the tail (move-to-end) drop zone.
    // All mutations to these are routed through `crate::util::defer()` per the
    // Dioxus signal-safety rules (this component reads them during render).
    let mut dragged_room = use_signal(|| None::<VerifyingKey>);
    let mut drag_over_room = use_signal(|| None::<VerifyingKey>);
    let mut drag_over_end = use_signal(|| false);

    // Touch-friendly reorder mode (freenet/river#348). HTML5 drag-and-drop is
    // pointer-only — browsers don't emit drag events for touch gestures — so a
    // toggle reveals per-row up/down controls that call the same input-agnostic
    // `move_room_up`/`move_room_down` persistence helpers. Read via `try_read`
    // and mutated through `crate::util::defer()` like the drag signals above.
    let mut reorder_mode = use_signal(|| false);

    // Memoize the room list to avoid reading signals during render.
    // Rooms render in the user's drag-chosen order first, then any
    // not-yet-positioned rooms in a deterministic order — see
    // `Rooms::ordered_room_keys` (raw `HashMap` order is unstable).
    let room_items = use_memo(move || {
        let Ok(rooms) = ROOMS.try_read() else {
            return Vec::new();
        };
        let current_room_key = CURRENT_ROOM.read().owner_key;

        rooms
            .ordered_room_keys()
            .into_iter()
            .filter_map(|room_key| {
                let room_data = rooms.map.get(&room_key)?;
                // A room is "awaiting sync" only while it has placeholder state
                // AND has not been given up on. Once a placeholder room reaches
                // a terminal Error (the bounded contract-absent case,
                // freenet/river#290, or a failed GET/PUT send) the spinner must
                // stop, so we surface an error marker (tooltip = the stored
                // error message) instead of a perpetual spinner.
                //
                // Scoped to placeholder-state rooms: a fully-synced room that
                // later hits some other transient `Error` should not show the
                // marker here.
                let sync_error_msg: Option<String> = if room_data.is_awaiting_initial_sync() {
                    match SYNC_INFO
                        .try_read()
                        .ok()
                        .and_then(|si| si.get_sync_status(&room_key).cloned())
                    {
                        Some(RoomSyncStatus::Error(msg)) => Some(msg),
                        _ => None,
                    }
                } else {
                    None
                };
                let awaiting_sync =
                    room_data.is_awaiting_initial_sync() && sync_error_msg.is_none();
                // Decrypt room name if room is private and we have the secret
                let sealed_name = &room_data
                    .room_state
                    .configuration
                    .configuration
                    .display
                    .name;
                let room_name = match unseal_bytes_with_secrets(sealed_name, &room_data.secrets) {
                    Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                    Err(_) => sealed_name.to_string_lossy(),
                };
                let is_current = current_room_key == Some(room_key);
                let is_private = room_data
                    .room_state
                    .configuration
                    .configuration
                    .privacy_mode
                    == PrivacyMode::Private;
                // Unread badge: same per-room count the document title uses,
                // surfaced in the rail so users who don't get browser
                // notifications (e.g. not on a localhost node) can still see
                // which rooms have new messages.
                let unread = count_unread_in_room_data(room_data);
                Some((
                    room_key,
                    room_name,
                    is_current,
                    awaiting_sync,
                    is_private,
                    unread,
                    sync_error_msg,
                ))
            })
            .collect::<Vec<_>>()
    });

    // Snapshot the drag-state signals once per render via `try_read` (fallible,
    // per the Dioxus signal-safety rules — a concurrent write borrow returns
    // Err here instead of panicking with "RefCell already borrowed" on
    // Firefox). Reused for the per-row highlight and the tail-zone visibility.
    let dragged_now: Option<VerifyingKey> = dragged_room.try_read().ok().and_then(|g| *g);
    let drag_over_now: Option<VerifyingKey> = drag_over_room.try_read().ok().and_then(|g| *g);
    let drag_over_end_now: bool = drag_over_end.try_read().map(|g| *g).unwrap_or(false);
    let reorder_now: bool = reorder_mode.try_read().map(|g| *g).unwrap_or(false);
    // Total rooms, used to disable the up control on the first row and the down
    // control on the last. Also gates the reorder toggle's visibility: with one
    // room there is nothing to reorder.
    let room_count: usize = room_items.read().len();

    // Rail display state (freenet/river#397). Read the load-state signal
    // reactively (fallible per the Dioxus signal-safety rules — a concurrent
    // write returns Err, in which case we treat the load as still `Loading` and
    // re-render on the next signal settle rather than panicking on Firefox).
    // Combined with `room_count` this decides whether the rail shows the list,
    // a loading spinner, a migrating spinner, or the calm empty state.
    let load_state: RoomsLoadState = ROOMS_LOAD_STATE
        .try_read()
        .map(|g| *g)
        .unwrap_or(RoomsLoadState::Loading);
    let display = room_list_display_state(load_state, room_count);

    rsx! {
        aside {
            // Stable hook for the connection-indicator regression tests
            // (freenet/river#274): the rooms rail is the always-rendered
            // left column that carries the persistent connection pill, so
            // tests anchor on this testid instead of the brittle visible
            // text "Rooms".
            "data-testid": "rooms-rail",
            class: "w-full md:w-64 flex-shrink-0 bg-panel border-r border-border flex flex-col overflow-y-auto",
            // Mobile back button (hidden on desktop)
            div { class: "md:hidden flex items-center px-3 py-2 border-b border-border flex-shrink-0",
                button {
                    class: "p-2 rounded-lg text-text-muted hover:text-accent hover:bg-surface transition-colors",
                    onclick: move |_| crate::util::defer(move || *MOBILE_VIEW.write() = MobileView::Chat),
                    Icon { icon: FaArrowLeft, width: 16, height: 16 }
                }
                span { class: "ml-2 text-sm font-semibold text-text", "Rooms" }
            }
            div { class: "p-4 hidden md:flex justify-center",
                img {
                    class: "w-24 h-24",
                    src: asset!("/assets/river_logo.svg"),
                    alt: "River Logo"
                }
            }

            // Rooms header with create button
            div { class: "px-4 py-2 flex items-center justify-between",
                h2 { class: "text-sm font-semibold text-text-muted uppercase tracking-wide flex items-center gap-2",
                    Icon { width: 16, height: 16, icon: FaComments }
                    span { "Rooms" }
                }
                div { class: "flex items-center gap-1",
                    // Reorder-mode toggle — the touch-friendly path to room
                    // reordering (freenet/river#348). Only meaningful with at
                    // least two rooms, so it's hidden below that to keep the
                    // header uncluttered on first load — but stays visible while
                    // reorder mode is on (even if rooms drop to one) so the user
                    // can always toggle back out rather than getting stuck.
                    if room_count > 1 || reorder_now {
                        button {
                            class: format!(
                                "p-1.5 rounded-md transition-colors {}",
                                if reorder_now {
                                    "text-accent bg-accent/10"
                                } else {
                                    "text-text-muted hover:text-accent hover:bg-surface"
                                },
                            ),
                            title: if reorder_now { "Done reordering" } else { "Reorder rooms" },
                            "aria-label": "Reorder rooms",
                            "aria-pressed": "{reorder_now}",
                            "data-testid": "reorder-rooms-toggle",
                            onclick: move |_| {
                                // Defer the toggle per the Dioxus signal-safety
                                // rules — this component reads `reorder_mode`
                                // during render.
                                crate::util::defer(move || {
                                    let next = !*reorder_mode.peek();
                                    reorder_mode.set(next);
                                });
                            },
                            Icon { width: 14, height: 14, icon: FaArrowsUpDown }
                        }
                    }
                    button {
                        "data-testid": "create-room-button",
                        class: "p-1.5 rounded-md text-text-muted hover:text-accent hover:bg-surface transition-colors",
                        title: "Create Room",
                        onclick: move |_| {
                            CREATE_ROOM_MODAL.write().show = true;
                        },
                        Icon { width: 14, height: 14, icon: FaPlus }
                    }
                }
            }

            // Room list
            ul {
                "data-testid": "room-list",
                class: "flex-1 px-2 py-1 space-y-0.5",

                // Non-room states (freenet/river#397). Shown only when there are
                // no rooms to render — the room-item map and tail drop zone below
                // produce nothing while empty, so this is purely additive. A
                // subtle, near-monochrome block: spinner (loading/migrating) or a
                // calm empty-state hint, vertically centred in the rail.
                match display {
                    RoomListDisplay::Loading => rsx! {
                        li {
                            key: "{ROOM_LIST_STATE_KEY}",
                            "data-testid": "room-list-loading",
                            class: "flex flex-col items-center justify-center gap-2 py-10 px-4 text-center",
                            div { class: "animate-spin w-4 h-4 border-2 border-text-muted border-t-transparent rounded-full" }
                            span { class: "text-sm text-text-muted", "Loading your rooms…" }
                        }
                    },
                    RoomListDisplay::Migrating => rsx! {
                        li {
                            key: "{ROOM_LIST_STATE_KEY}",
                            "data-testid": "room-list-migrating",
                            class: "flex flex-col items-center justify-center gap-2 py-10 px-4 text-center",
                            div { class: "animate-spin w-4 h-4 border-2 border-text-muted border-t-transparent rounded-full" }
                            span { class: "text-sm text-text-muted", "Migrating your rooms…" }
                            span { class: "text-xs text-text-muted opacity-70", "(one-time step after an update)" }
                        }
                    },
                    RoomListDisplay::Empty => rsx! {
                        li {
                            key: "{ROOM_LIST_STATE_KEY}",
                            "data-testid": "room-list-empty",
                            class: "flex flex-col items-center justify-center gap-1 py-10 px-4 text-center",
                            span { class: "text-sm text-text-muted", "No rooms yet" }
                            span { class: "text-xs text-text-muted opacity-70", "Create a room or join one with an invite code." }
                        }
                    },
                    RoomListDisplay::LoadFailed => rsx! {
                        li {
                            key: "{ROOM_LIST_STATE_KEY}",
                            "data-testid": "room-list-error",
                            class: "flex flex-col items-center justify-center gap-3 py-10 px-4 text-center",
                            span { class: "text-sm text-text-muted", "Couldn't load your rooms" }
                            span { class: "text-xs text-text-muted opacity-70", "Check your connection and try again." }
                            button {
                                "data-testid": "room-list-retry-button",
                                class: "flex items-center justify-center gap-2 px-3 py-2 rounded-lg text-sm text-text-muted bg-surface hover:bg-surface-hover transition-colors",
                                // Retry re-fires the current-delegate load. Safe to
                                // call directly — `retry_rooms_load` defers its
                                // signal write and spawns via setTimeout(0) (Dioxus
                                // signal-safety rules).
                                onclick: move |_| retry_rooms_load(),
                                span { "Retry" }
                            }
                        }
                    },
                    RoomListDisplay::List => rsx! {},
                }

                {room_items.read().iter().enumerate().map(|(idx, (room_key, room_name, is_current, awaiting_sync, is_private, unread, sync_error_msg))| {
                    let room_key = *room_key;
                    let room_name = room_name.clone();
                    let is_current = *is_current;
                    let awaiting_sync = *awaiting_sync;
                    let is_private = *is_private;
                    let unread = *unread;
                    let sync_error_msg = sync_error_msg.clone();
                    // Row position, for disabling the up control on the first
                    // row and the down control on the last (reorder mode).
                    let is_first = idx == 0;
                    let is_last = idx + 1 == room_count;
                    // Drag feedback: dim the row being dragged, and draw a top
                    // border on the row the cursor is over (drop lands the
                    // dragged room immediately before it — see `move_room`).
                    // The 2px top border is always reserved (transparent →
                    // accent) so highlighting it on drag-over causes no layout
                    // shift.
                    let is_dragged = dragged_now == Some(room_key);
                    let is_drag_over = drag_over_now == Some(room_key);
                    let room_testid =
                        format!("room-item-{}", bs58::encode(room_key.to_bytes()).into_string());
                    rsx! {
                        li {
                            key: "{room_key:?}",
                            // Stable per-room hook for automation (freenet/river#25).
                            // Entity-ID pattern: `room-item-{base58(owner_vk)}`.
                            "data-testid": "{room_testid}",
                            // Rooms are reorderable by drag-and-drop. The whole
                            // row is the drag handle; the inner button still
                            // handles click-to-open (a click is distinct from a
                            // drag gesture).
                            draggable: "true",
                            class: format!(
                                "rounded-lg cursor-grab active:cursor-grabbing select-none transition-opacity border-t-2 {} {}",
                                if is_dragged { "opacity-50" } else { "" },
                                if is_drag_over && !is_dragged { "border-accent" } else { "border-transparent" },
                            ),
                            ondragstart: move |_| {
                                // Defer every drag-state mutation per the
                                // signal-safety rules (this component reads
                                // these signals during render).
                                crate::util::defer(move || dragged_room.set(Some(room_key)));
                            },
                            ondragenter: move |_| {
                                // Fires on entering a new row (unlike ondragover,
                                // which fires continuously), so this won't churn
                                // re-renders on every mouse move.
                                crate::util::defer(move || {
                                    if *drag_over_room.peek() != Some(room_key) {
                                        drag_over_room.set(Some(room_key));
                                    }
                                });
                            },
                            ondragover: move |evt| {
                                // Required for the browser to treat this row as a
                                // valid drop target.
                                evt.prevent_default();
                            },
                            ondrop: move |evt| {
                                evt.prevent_default();
                                // Read the dragged key up front (peek: no
                                // subscription), then do all signal mutations
                                // and the persisted reorder inside one deferred,
                                // re-entrancy-safe closure.
                                let src = *dragged_room.peek();
                                crate::util::defer(move || {
                                    drag_over_room.set(None);
                                    dragged_room.set(None);
                                    if let Some(src) = src {
                                        if src != room_key {
                                            ROOMS.with_mut(|rooms| rooms.move_room(src, room_key));
                                            spawn(async move {
                                                if let Err(e) = save_rooms_to_delegate().await {
                                                    error!("Failed to save room order: {}", e);
                                                }
                                            });
                                        }
                                    }
                                });
                            },
                            ondragend: move |_| {
                                crate::util::defer(move || {
                                    dragged_room.set(None);
                                    drag_over_room.set(None);
                                    drag_over_end.set(false);
                                });
                            },
                            div { class: "flex items-center",
                            button {
                                class: format!(
                                    "flex-1 min-w-0 text-left px-3 py-2 rounded-lg text-sm transition-colors {}",
                                    if is_current {
                                        "bg-accent/10 text-accent font-medium"
                                    } else {
                                        "text-text hover:bg-surface"
                                    }
                                ),
                                onclick: move |_| {
                                    // Defer signal mutations to a clean execution context to
                                    // prevent RefCell re-entrant borrow panics.
                                    crate::util::defer(move || {
                                        *CURRENT_ROOM.write() = CurrentRoom { owner_key: Some(room_key) };
                                        mark_current_room_as_read();
                                        // Switch to chat view on mobile
                                        *MOBILE_VIEW.write() = MobileView::Chat;
                                        spawn(async move {
                                            if let Err(e) = save_rooms_to_delegate().await {
                                                error!("Failed to save current room selection: {}", e);
                                            }
                                        });
                                    });
                                },
                                div { class: "flex items-center gap-2",
                                    span { class: "block truncate flex-1", "{room_name}" }
                                    // Private-room lock — sits to the RIGHT of the
                                    // room name (after it in the flex row).
                                    if is_private {
                                        span {
                                            class: "flex-shrink-0 text-text-muted",
                                            title: "Private room (members-only, end-to-end encrypted)",
                                            "aria-label": "Private room",
                                            Icon { width: 12, height: 12, icon: FaLock }
                                        }
                                    }
                                    // Unread badge — hidden for the current
                                    // room (its messages are marked read on
                                    // open, so a badge there would only
                                    // flicker). Styling mirrors the DM rail
                                    // badge plus `flex-shrink-0` so a long
                                    // truncated room name can't squash it.
                                    if unread > 0 && !is_current {
                                        span {
                                            class: "ml-2 flex-shrink-0 inline-flex items-center justify-center px-2 py-0.5 rounded-full text-xs font-medium bg-accent text-white",
                                            "data-testid": "room-unread-badge",
                                            title: "{unread} unread",
                                            "aria-label": "{unread} unread messages",
                                            "{unread}"
                                        }
                                    }
                                    if awaiting_sync {
                                        div { class: "animate-spin w-3 h-3 border-2 border-text-muted border-t-transparent rounded-full flex-shrink-0" }
                                    } else if let Some(err_msg) = sync_error_msg {
                                        // Terminal sync failure for a placeholder room: the
                                        // bounded contract-absent case (freenet/river#290) or a
                                        // failed GET/PUT send. Show a warning marker (tooltip = the
                                        // stored error message) instead of a perpetual spinner.
                                        span {
                                            class: "flex-shrink-0 text-red-600 dark:text-red-400",
                                            title: "{err_msg}",
                                            "aria-label": "Room sync failed",
                                            Icon { width: 12, height: 12, icon: FaTriangleExclamation }
                                        }
                                    }
                                }
                            }
                            // Touch-friendly reorder controls (freenet/river#348):
                            // up/down move the room one slot, calling the same
                            // input-agnostic persistence helpers the drag path
                            // uses. Disabled at the list boundaries (first row
                            // can't go up; last can't go down). Only rendered in
                            // reorder mode so the rail stays clean by default.
                            if reorder_now {
                                div { class: "flex items-center flex-shrink-0 pr-1 gap-0.5",
                                    button {
                                        r#type: "button",
                                        class: format!(
                                            "p-2 rounded-md transition-colors {}",
                                            if is_first {
                                                "text-text-muted/30 cursor-not-allowed"
                                            } else {
                                                "text-text-muted hover:text-accent hover:bg-surface"
                                            },
                                        ),
                                        disabled: is_first,
                                        title: "Move up",
                                        "aria-label": "Move room up",
                                        "data-testid": "reorder-room-up",
                                        onclick: move |evt| {
                                            // Sibling of the room-open button, so a tap
                                            // here must not bubble into row selection.
                                            evt.stop_propagation();
                                            // `move_room_up` is a no-op at the top, but
                                            // the button is also `disabled` there.
                                            crate::util::defer(move || {
                                                ROOMS.with_mut(|rooms| rooms.move_room_up(room_key));
                                                spawn(async move {
                                                    if let Err(e) = save_rooms_to_delegate().await {
                                                        error!("Failed to save room order: {}", e);
                                                    }
                                                });
                                            });
                                        },
                                        Icon { width: 14, height: 14, icon: FaChevronUp }
                                    }
                                    button {
                                        r#type: "button",
                                        class: format!(
                                            "p-2 rounded-md transition-colors {}",
                                            if is_last {
                                                "text-text-muted/30 cursor-not-allowed"
                                            } else {
                                                "text-text-muted hover:text-accent hover:bg-surface"
                                            },
                                        ),
                                        disabled: is_last,
                                        title: "Move down",
                                        "aria-label": "Move room down",
                                        "data-testid": "reorder-room-down",
                                        onclick: move |evt| {
                                            evt.stop_propagation();
                                            crate::util::defer(move || {
                                                ROOMS.with_mut(|rooms| rooms.move_room_down(room_key));
                                                spawn(async move {
                                                    if let Err(e) = save_rooms_to_delegate().await {
                                                        error!("Failed to save room order: {}", e);
                                                    }
                                                });
                                            });
                                        },
                                        Icon { width: 14, height: 14, icon: FaChevronDown }
                                    }
                                }
                            }
                            }
                        }
                    }
                }).collect::<Vec<_>>().into_iter()}

                // Tail drop zone: only present mid-drag. Every row drop inserts
                // the dragged room BEFORE its target, so the final slot would be
                // unreachable without a dedicated end target. Shown only while a
                // drag is in progress so it never adds layout when idle.
                if dragged_now.is_some() {
                    li {
                        key: "{TAIL_DROP_ZONE_KEY}",
                        class: format!(
                            "h-8 rounded-lg border-2 border-dashed transition-colors flex items-center justify-center text-xs text-text-muted {}",
                            if drag_over_end_now { "border-accent bg-accent/10" } else { "border-border/40" },
                        ),
                        "aria-label": "Move to end",
                        ondragenter: move |_| {
                            crate::util::defer(move || {
                                if !*drag_over_end.peek() {
                                    drag_over_end.set(true);
                                }
                            });
                        },
                        ondragover: move |evt| {
                            evt.prevent_default();
                        },
                        ondragleave: move |_| {
                            crate::util::defer(move || drag_over_end.set(false));
                        },
                        ondrop: move |evt| {
                            evt.prevent_default();
                            let src = *dragged_room.peek();
                            crate::util::defer(move || {
                                drag_over_end.set(false);
                                drag_over_room.set(None);
                                dragged_room.set(None);
                                if let Some(src) = src {
                                    ROOMS.with_mut(|rooms| rooms.move_room_to_end(src));
                                    spawn(async move {
                                        if let Err(e) = save_rooms_to_delegate().await {
                                            error!("Failed to save room order: {}", e);
                                        }
                                    });
                                }
                            });
                        },
                        "Drop here to move to end"
                    }
                }
            }

            // Direct Messages section under the room list — surfaces DM
            // threads across ALL rooms so a user with DMs in Room B can
            // see them while focused on Room A (zorolin's feedback,
            // 2026-05-16). Hidden when empty so it doesn't clutter the
            // first-load experience.
            DmRailSection {}

            // Bottom actions
            div { class: "p-3 border-t border-border space-y-2",
                // Enter a portable invite code (freenet/river#381). Lets a user
                // join a room from a bare code — no host-baked link needed —
                // which is the only practical path on hosts like try.freenet.org.
                button {
                    "data-testid": "join-with-code-button",
                    class: "w-full flex items-center justify-center gap-2 px-3 py-2 rounded-lg text-sm text-text-muted bg-surface hover:bg-surface-hover transition-colors",
                    onclick: move |_| join_code_modal_active.set(true),
                    Icon { width: 14, height: 14, icon: FaRightToBracket }
                    span { "Enter Invite Code" }
                }
                button {
                    class: "w-full flex items-center justify-center gap-2 px-3 py-2 rounded-lg text-sm text-text-muted bg-surface hover:bg-surface-hover transition-colors",
                    onclick: move |_| import_modal_active.set(true),
                    Icon { width: 14, height: 14, icon: FaFileImport }
                    span { "Import ID" }
                }
            }

            // WebSocket connection status pill — kept in the always-rendered
            // left rail so it stays visible for users with no rooms yet
            // (Bug #5, Ivvor on Matrix 2026-05-17). Previously lived in the
            // right-hand member panel, which is hidden when no room is
            // selected.
            ConnectionStatusIndicator {}

            // Build info (local time)
            div { class: "px-3 py-2 text-xs text-text-muted text-center",
                {"Built: "} {format_build_time_local()}
            }
        }
        ImportIdentityModal {
            is_active: import_modal_active
        }
        JoinWithCodeModal {
            is_active: join_code_modal_active
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// freenet/river#397: the initial load hasn't resolved and there are no
    /// rooms yet → show the loading spinner, NOT a blank list or a premature
    /// "no rooms yet".
    #[test]
    fn loading_with_no_rooms_shows_loading() {
        assert_eq!(
            room_list_display_state(RoomsLoadState::Loading, 0),
            RoomListDisplay::Loading
        );
    }

    /// freenet/river#397: a migration is in progress with the map still empty
    /// (Ivvor's case) → show "Migrating…", never "no rooms yet".
    #[test]
    fn migrating_with_no_rooms_shows_migrating() {
        assert_eq!(
            room_list_display_state(RoomsLoadState::Migrating, 0),
            RoomListDisplay::Migrating
        );
    }

    /// freenet/river#397: the load resolved and the user genuinely has no rooms
    /// → the calm empty state (only reachable from `Loaded`).
    #[test]
    fn loaded_with_no_rooms_shows_empty() {
        assert_eq!(
            room_list_display_state(RoomsLoadState::Loaded, 0),
            RoomListDisplay::Empty
        );
    }

    /// freenet/river#397 Codex review 4: a failed load with no rooms → the
    /// LoadFailed error+retry block, NOT a false "no rooms yet".
    #[test]
    fn load_failed_with_no_rooms_shows_error() {
        assert_eq!(
            room_list_display_state(RoomsLoadState::LoadFailed, 0),
            RoomListDisplay::LoadFailed
        );
    }

    /// freenet/river#397: any rooms present → render the list, regardless of the
    /// load state (mid-migration, still-loading, resolved, OR failed). Once
    /// there's something to show, load bookkeeping is irrelevant — a partial load
    /// that fetched SOME rooms shows them rather than the error block.
    #[test]
    fn any_rooms_shows_list_regardless_of_load_state() {
        for state in [
            RoomsLoadState::Loading,
            RoomsLoadState::Migrating,
            RoomsLoadState::Loaded,
            RoomsLoadState::LoadFailed,
        ] {
            assert_eq!(
                room_list_display_state(state, 1),
                RoomListDisplay::List,
                "{state:?} with rooms must render the list"
            );
            assert_eq!(
                room_list_display_state(state, 7),
                RoomListDisplay::List,
                "{state:?} with rooms must render the list"
            );
        }
    }

    /// freenet/river#397 (#1 safety invariant): a genuinely-new user, once the
    /// load resolves (`Loaded`) with zero rooms, lands on the calm empty state —
    /// never a perpetual spinner. (The resolution itself — Loading→Loaded via the
    /// completion signal / backstop — is pinned by the chat_delegate tests.)
    #[test]
    fn new_user_resolves_to_empty_not_stuck_loading() {
        assert_eq!(
            room_list_display_state(RoomsLoadState::Loaded, 0),
            RoomListDisplay::Empty,
            "a resolved new user with no rooms must show Empty, never Loading"
        );
    }
}
