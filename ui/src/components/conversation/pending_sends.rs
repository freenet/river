//! The sender's own messages that are not yet signed and committed, shown at
//! the end of the history the moment they are sent (`outbox.rs`).
//!
//! Rendered after the history list and before `bottom-sentinel`, so a request
//! to go to the end includes them. They carry no `data-item-key`, so the
//! scroll code's row measurement and the read rule never count them as
//! messages: a pending row is local UI state, not part of the room.

use super::outbox::OUTBOX;
use super::outbox_commit;
use crate::components::app::sync_info::SYNC_INFO;
use crate::components::app::ROOMS;
use crate::util::{format_utc_as_full_datetime, format_utc_as_local_time};
use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;

#[component]
pub(super) fn PendingSends(room: VerifyingKey, author: VerifyingKey) -> Element {
    // Rendered from the outbox, which only a deferred closure writes; still
    // read fallibly, and anchored so a contended read cannot leave this
    // unsubscribed (freenet/river#555).
    crate::util::signal_guard::anchor();
    let rows = match OUTBOX.try_read() {
        Ok(outbox) => outbox.pending_rows(&room, &author),
        Err(_) => {
            crate::util::signal_guard::schedule_nudge();
            return rsx! {};
        }
    };
    if rows.is_empty() {
        return rsx! {};
    }
    rsx! {
        div { class: "msg-history", "data-testid": "pending-messages",
            for row in rows {
                {
                    let ms = row
                        .time
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0);
                    let failed = row.failed.is_some();
                    let id = row.id;
                    rsx! {
                        div {
                            key: "{id}",
                            class: "msg-group",
                            "data-testid": "pending-message",
                            "data-pending-state": if failed { "failed" } else { "sending" },
                            div { class: "msg-row", "data-message-self": "true",
                                div { class: "msg-gutter" }
                                div { class: "msg-content relative min-w-0",
                                    div { class: "flex flex-wrap items-baseline gap-x-2 mb-0.5 leading-5",
                                        span { class: "text-[15px] font-bold text-text-secondary [overflow-wrap:anywhere]",
                                            "{row.author_name}"
                                        }
                                        span {
                                            class: "text-xs text-text-muted",
                                            title: format_utc_as_full_datetime(ms),
                                            {format_utc_as_local_time(ms)}
                                        }
                                        if let Some(reason) = row.failed.clone() {
                                            span {
                                                "data-testid": "pending-message-status",
                                                class: "text-xs text-danger",
                                                title: "{reason}",
                                                "Not sent"
                                            }
                                            button {
                                                r#type: "button",
                                                "data-testid": "pending-message-retry",
                                                class: "text-xs font-semibold text-link hover:underline cursor-pointer",
                                                onclick: move |_| outbox_commit::retry(room, id),
                                                "Retry"
                                            }
                                            button {
                                                r#type: "button",
                                                "data-testid": "pending-message-discard",
                                                class: "text-xs text-text-muted hover:underline cursor-pointer",
                                                onclick: move |_| outbox_commit::discard(room, id),
                                                "Discard"
                                            }
                                        } else {
                                            span {
                                                "data-testid": "pending-message-status",
                                                role: "status",
                                                class: "text-xs text-text-muted",
                                                "Sending"
                                            }
                                        }
                                    }
                                    div {
                                        "data-testid": "pending-message-text",
                                        class: if failed {
                                            "whitespace-pre-wrap [overflow-wrap:anywhere]"
                                        } else {
                                            "whitespace-pre-wrap [overflow-wrap:anywhere] text-text-muted"
                                        },
                                        "{row.text}"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The sender's own messages that are in the room but have not been sent: the
/// sync pass skipped them because the room is not subscribed yet, and it stays
/// that way until the subscription is confirmed (or retried). Without this the
/// message reads as posted while nothing has left the device.
///
/// Rendered beside [`PendingSends`] with the same row styling, and gone as soon
/// as the change is sent (`SyncInfo::state_updated` clears the hold).
#[component]
pub(super) fn HeldSends(room: VerifyingKey, author: VerifyingKey) -> Element {
    // Reads two signals fallibly; anchored so a contended read cannot leave
    // this unsubscribed (freenet/river#555).
    crate::util::signal_guard::anchor();
    let (Ok(sync_info), Ok(rooms)) = (SYNC_INFO.try_read(), ROOMS.try_read()) else {
        crate::util::signal_guard::schedule_nudge();
        return rsx! {};
    };
    let count = rooms.map.get(&room).map_or(0, |room_data| {
        sync_info.held_message_count(
            &room,
            &room_data.room_state,
            river_core::room_state::member::MemberId::from(author),
        )
    });
    if count == 0 {
        return rsx! {};
    }
    let text = if count == 1 {
        "Your message has not been sent yet. It will go out as soon as the room connection is confirmed."
    } else {
        "Your messages have not been sent yet. They will go out as soon as the room connection is confirmed."
    };
    rsx! {
        div { class: "msg-history", "data-testid": "held-messages",
            div {
                class: "msg-group",
                "data-testid": "held-message-notice",
                "data-pending-state": "held",
                "data-held-count": "{count}",
                div { class: "msg-row", "data-message-self": "true",
                    div { class: "msg-gutter" }
                    div { class: "msg-content relative min-w-0",
                        span {
                            "data-testid": "held-message-status",
                            role: "status",
                            class: "text-xs text-text-muted",
                            "{text}"
                        }
                    }
                }
            }
        }
    }
}
