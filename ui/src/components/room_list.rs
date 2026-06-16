pub(crate) mod create_room_modal;
pub(crate) mod dm_rail_section;
pub(crate) mod edit_room_modal;
pub(crate) mod receive_invitation_modal;
pub(crate) mod room_name_field;

use crate::components::app::chat_delegate::save_rooms_to_delegate;
use crate::components::app::document_title::{
    count_unread_in_room_data, mark_current_room_as_read,
};
use crate::components::app::sync_info::{RoomSyncStatus, SYNC_INFO};
use crate::components::app::{MobileView, CREATE_ROOM_MODAL, CURRENT_ROOM, MOBILE_VIEW, ROOMS};
use crate::components::members::{ConnectionStatusIndicator, ImportIdentityModal};
use crate::components::room_list::dm_rail_section::DmRailSection;
use crate::room_data::CurrentRoom;
use crate::util::ecies::unseal_bytes_with_secrets;
use dioxus::logger::tracing::error;
use dioxus::prelude::*;
use dioxus_free_icons::{
    icons::fa_solid_icons::{
        FaArrowLeft, FaComments, FaFileImport, FaLock, FaPlus, FaTriangleExclamation,
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

    // Drag-and-drop reorder state (a local view preference). `dragged_room`
    // is the row currently being dragged; `drag_over_room` is the row the
    // cursor is hovering over, used to draw the insertion indicator;
    // `drag_over_end` is the same for the tail (move-to-end) drop zone.
    // All mutations to these are routed through `crate::util::defer()` per the
    // Dioxus signal-safety rules (this component reads them during render).
    let mut dragged_room = use_signal(|| None::<VerifyingKey>);
    let mut drag_over_room = use_signal(|| None::<VerifyingKey>);
    let mut drag_over_end = use_signal(|| false);

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
                button {
                    class: "p-1.5 rounded-md text-text-muted hover:text-accent hover:bg-surface transition-colors",
                    title: "Create Room",
                    onclick: move |_| {
                        CREATE_ROOM_MODAL.write().show = true;
                    },
                    Icon { width: 14, height: 14, icon: FaPlus }
                }
            }

            // Room list
            ul { class: "flex-1 px-2 py-1 space-y-0.5",
                {room_items.read().iter().map(|(room_key, room_name, is_current, awaiting_sync, is_private, unread, sync_error_msg)| {
                    let room_key = *room_key;
                    let room_name = room_name.clone();
                    let is_current = *is_current;
                    let awaiting_sync = *awaiting_sync;
                    let is_private = *is_private;
                    let unread = *unread;
                    let sync_error_msg = sync_error_msg.clone();
                    // Drag feedback: dim the row being dragged, and draw a top
                    // border on the row the cursor is over (drop lands the
                    // dragged room immediately before it — see `move_room`).
                    // The 2px top border is always reserved (transparent →
                    // accent) so highlighting it on drag-over causes no layout
                    // shift.
                    let is_dragged = dragged_now == Some(room_key);
                    let is_drag_over = drag_over_now == Some(room_key);
                    rsx! {
                        li {
                            key: "{room_key:?}",
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
                            button {
                                class: format!(
                                    "w-full text-left px-3 py-2 rounded-lg text-sm transition-colors {}",
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
    }
}
