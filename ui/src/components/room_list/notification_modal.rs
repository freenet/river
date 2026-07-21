//! Compact per-room notification-preference modal, opened from the bell icon in
//! the conversation header (`NOTIFICATION_MODAL`). Lets the user pick when a
//! browser notification fires for a room: every message, mentions & replies
//! only, or muted.
//!
//! This is the canonical entry point for the setting — the room-details
//! (edit-room) modal deliberately does NOT duplicate it, so there is one home.
//!
//! Storage: the choice is a local user setting persisted in the `rooms_meta`
//! delegate blob (see [`crate::room_data::NotificationMode`] /
//! [`crate::room_data::RoomsMeta`]), the same model as `room_order`. It
//! soft-syncs across the user's devices (local-wins merge in `reconcile_meta`),
//! so the help text says "your devices", not "this device only".

use crate::components::app::chat_delegate::save_rooms_to_delegate;
use crate::components::app::{NOTIFICATION_MODAL, ROOMS};
use crate::room_data::NotificationMode;
use dioxus::logger::tracing::error;
use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::{FaAt, FaBell, FaBellSlash, FaCheck, FaXmark};
use dioxus_free_icons::Icon;
use ed25519_dalek::VerifyingKey;

/// The three notification modes, with the display strings for the modal rows.
/// Kept in one place so the row list and the icon/label lookups can't drift.
const MODES: [(NotificationMode, &str, &str); 3] = [
    (
        NotificationMode::All,
        "All messages",
        "Notify for every message in this room.",
    ),
    (
        NotificationMode::MentionsAndReplies,
        "Mentions & replies only",
        "Notify only when someone @mentions you or replies to your message.",
    ),
    (
        NotificationMode::Muted,
        "Muted",
        "Never notify for this room.",
    ),
];

/// Read the current notification mode for `room_vk` from `ROOMS`. An absent
/// entry means [`NotificationMode::All`] (the default).
fn current_mode(room_vk: &VerifyingKey) -> NotificationMode {
    ROOMS
        .try_read()
        .ok()
        .and_then(|rooms| rooms.notification_modes.get(room_vk).copied())
        .unwrap_or_default()
}

/// Persist a newly-chosen mode: mutate the in-memory map, then write the
/// `rooms_meta` blob to the delegate. Deferred to satisfy the Dioxus
/// signal-safety rule (no signal mutation directly inside an event handler).
fn set_mode(room_vk: VerifyingKey, mode: NotificationMode) {
    crate::util::defer(move || {
        ROOMS.with_mut(|rooms| {
            rooms.notification_modes.insert(room_vk, mode);
        });
        spawn(async move {
            if let Err(e) = save_rooms_to_delegate().await {
                error!("Failed to save notification mode: {}", e);
            }
        });
        // Close the modal after applying, so a pick is one click.
        NOTIFICATION_MODAL.write().room = None;
    });
}

/// Always-mounted modal; renders nothing unless `NOTIFICATION_MODAL.room` is set.
#[component]
pub fn NotificationModal() -> Element {
    let Some(room_vk) = NOTIFICATION_MODAL.read().room else {
        return rsx! {};
    };
    let selected = current_mode(&room_vk);

    rsx! {
        // Backdrop (same pattern as EditRoomModal).
        div {
            class: "fixed inset-0 z-50 flex items-center justify-center",
            div {
                class: "absolute inset-0 bg-black/50",
                onclick: move |_| {
                    NOTIFICATION_MODAL.write().room = None;
                }
            }
            // Content — compact (max-w-sm), just the mode picker.
            div {
                "data-testid": "notification-modal",
                class: "relative z-10 w-full max-w-sm mx-4 bg-panel rounded-xl shadow-xl border border-border",
                div { class: "p-5",
                    div { class: "flex items-center justify-between mb-4",
                        h1 { class: "text-lg font-semibold text-text", "Notifications" }
                        button {
                            "data-testid": "notification-modal-close",
                            class: "p-1.5 rounded-lg text-text-muted hover:text-text hover:bg-surface transition-colors",
                            "aria-label": "Close",
                            onclick: move |_| {
                                NOTIFICATION_MODAL.write().room = None;
                            },
                            Icon { icon: FaXmark, width: 16, height: 16 }
                        }
                    }
                    div { class: "flex flex-col gap-1",
                        for (mode , label , description) in MODES {
                            {
                                let is_selected = mode == selected;
                                rsx! {
                                    button {
                                        key: "{label}",
                                        "data-testid": "notification-mode-option",
                                        class: if is_selected {
                                            "flex items-start gap-3 w-full text-left px-3 py-2.5 rounded-lg bg-surface border border-accent transition-colors"
                                        } else {
                                            "flex items-start gap-3 w-full text-left px-3 py-2.5 rounded-lg border border-transparent hover:bg-surface transition-colors"
                                        },
                                        onclick: move |_| {
                                            // No-op if already selected — just close.
                                            if is_selected {
                                                crate::util::defer(move || {
                                                    NOTIFICATION_MODAL.write().room = None;
                                                });
                                            } else {
                                                set_mode(room_vk, mode);
                                            }
                                        },
                                        span {
                                            class: if is_selected { "text-accent flex-shrink-0 mt-0.5" } else { "text-text-muted flex-shrink-0 mt-0.5" },
                                            {mode_icon(mode)}
                                        }
                                        div { class: "min-w-0 flex-1",
                                            div { class: "text-sm font-medium text-text", "{label}" }
                                            div { class: "text-xs text-text-muted", "{description}" }
                                        }
                                        if is_selected {
                                            span { class: "text-accent flex-shrink-0 mt-0.5",
                                                Icon { icon: FaCheck, width: 14, height: 14 }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    p { class: "text-xs text-text-muted mt-4",
                        "Applies to this room and syncs across your devices."
                    }
                }
            }
        }
    }
}

/// The leading icon for a mode row.
fn mode_icon(mode: NotificationMode) -> Element {
    match mode {
        NotificationMode::All => rsx! { Icon { icon: FaBell, width: 16, height: 16 } },
        NotificationMode::MentionsAndReplies => rsx! { Icon { icon: FaAt, width: 16, height: 16 } },
        NotificationMode::Muted => rsx! { Icon { icon: FaBellSlash, width: 16, height: 16 } },
    }
}
