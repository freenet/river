//! The read-only pane for a room opened by `?room=` link (freenet/river#513).
//! State and rules: [`crate::public_room`].

use crate::components::app::chat_delegate::{RoomsLoadState, ROOMS_LOAD_STATE};
use crate::components::app::freenet_api::freenet_synchronizer::SynchronizerStatus;
use crate::components::app::{CURRENT_ROOM, ROOMS, SYNC_STATUS};
use crate::components::room_list::edit_room_modal::CopyButton;
use crate::public_join::{self, offers_join, JoinPhase, JOIN};
use crate::public_room::{
    close, composer_note, fetch_action, public_room_link, request_fetch, request_subscribe, retry,
    FetchAction, ViewedRoom, ViewedRoomStatus, VIEWED_ROOM,
};
use crate::room_data::CurrentRoom;
use crate::util::display_name::display_nickname;
use dioxus::prelude::*;
use river_core::room_state::member::MemberId;
use river_core::room_state::ChatRoomStateV1;
use std::collections::HashMap;
use std::time::UNIX_EPOCH;

#[component]
pub fn ViewedRoomPane() -> Element {
    // Fetch once connected and the user's own rooms have loaded; subscribe once
    // the room is known to be public, and again after each reconnect.
    let last_connected = use_hook(|| std::rc::Rc::new(std::cell::Cell::new(false)));
    let asked = use_hook(|| std::rc::Rc::new(std::cell::Cell::new(false)));
    use_effect(move || {
        crate::util::signal_guard::anchor();
        let connected = match SYNC_STATUS.try_read() {
            Ok(s) => matches!(&*s, SynchronizerStatus::Connected),
            Err(_) => {
                crate::util::signal_guard::schedule_nudge();
                return;
            }
        };
        let rooms_loaded = match ROOMS_LOAD_STATE.try_read() {
            Ok(s) => *s == RoomsLoadState::Loaded,
            Err(_) => {
                crate::util::signal_guard::schedule_nudge();
                return;
            }
        };
        let just_connected = connected && !last_connected.replace(connected);
        if !connected {
            last_connected.set(false);
        }
        let Ok(viewed) = VIEWED_ROOM.try_read() else {
            crate::util::signal_guard::schedule_nudge();
            return;
        };
        let Some(ViewedRoom {
            owner_vk: Some(owner_vk),
            status,
        }) = viewed.as_ref()
        else {
            return;
        };
        let owner_vk = *owner_vk;
        let ours = match ROOMS.try_read() {
            Ok(rooms) => rooms.map.contains_key(&owner_vk),
            Err(_) => {
                crate::util::signal_guard::schedule_nudge();
                true
            }
        };
        match fetch_action(
            connected,
            just_connected,
            asked.get(),
            rooms_loaded,
            ours,
            status,
        ) {
            Some(FetchAction::Get) => {
                asked.set(true);
                request_fetch(owner_vk);
            }
            Some(FetchAction::Subscribe) => request_subscribe(owner_vk),
            None => {}
        }
    });

    // A room the user already belongs to opens as their own room, not as a
    // visitor's view of it. ROOMS may hydrate after the link was read.
    use_effect(move || {
        crate::util::signal_guard::anchor();
        let owner = match VIEWED_ROOM.try_read() {
            Ok(v) => v.as_ref().and_then(|r| r.owner_vk),
            Err(_) => {
                crate::util::signal_guard::schedule_nudge();
                return;
            }
        };
        let Some(owner) = owner else { return };
        let Ok(rooms) = ROOMS.try_read() else {
            crate::util::signal_guard::schedule_nudge();
            return;
        };
        if rooms.map.contains_key(&owner) {
            crate::util::defer(move || {
                *CURRENT_ROOM.write() = CurrentRoom {
                    owner_key: Some(owner),
                };
                close();
            });
        }
    });

    let viewed = match VIEWED_ROOM.try_read() {
        Ok(v) => v.clone(),
        Err(_) => {
            crate::util::signal_guard::schedule_nudge();
            return rsx! { div { class: "flex-1" } };
        }
    };
    let Some(viewed) = viewed else {
        return rsx! { div { class: "flex-1" } };
    };

    rsx! {
        div {
            class: "flex-1 flex flex-col min-w-0 bg-bg",
            "data-testid": "viewed-room",
            match viewed.status {
                ViewedRoomStatus::Public(state) => rsx! {
                    PublicRoomView { owner_vk: viewed.owner_vk, state: *state }
                },
                other => rsx! { Notice { status: other, owner_vk: viewed.owner_vk } },
            }
        }
    }
}

fn close_button() -> Element {
    rsx! {
        button {
            r#type: "button",
            "data-testid": "viewed-room-close",
            class: "flex-shrink-0 px-3 py-2 bg-surface hover:bg-surface-hover border border-border rounded-lg text-text-muted hover:text-text text-sm transition-colors",
            onclick: move |_| close(),
            "Close"
        }
    }
}

#[component]
fn Notice(status: ViewedRoomStatus, owner_vk: Option<ed25519_dalek::VerifyingKey>) -> Element {
    let (testid, title, body) = match &status {
        ViewedRoomStatus::Loading => (
            "viewed-room-loading",
            "Looking for this room".to_string(),
            "Asking the network for the room. This can take a few seconds.".to_string(),
        ),
        ViewedRoomStatus::NotFound => (
            "viewed-room-not-found",
            "Room not found".to_string(),
            "No peer has this room right now, or it could not be read.".to_string(),
        ),
        ViewedRoomStatus::InviteOnly => (
            "viewed-room-invite-only",
            "This room is invite-only".to_string(),
            "Its messages and members are private. To join, ask a member for an invitation."
                .to_string(),
        ),
        ViewedRoomStatus::TimedOut => (
            "viewed-room-timed-out",
            "No answer yet".to_string(),
            "Nothing replied about this room. Check your connection and try again.".to_string(),
        ),
        ViewedRoomStatus::Invalid(e) => (
            "viewed-room-invalid",
            "Invalid room link".to_string(),
            e.message(),
        ),
        ViewedRoomStatus::Public(_) => ("viewed-room-loading", String::new(), String::new()),
    };
    let can_retry = matches!(
        status,
        ViewedRoomStatus::NotFound | ViewedRoomStatus::TimedOut
    ) && owner_vk.is_some();
    rsx! {
        div { class: "flex-1 flex items-center justify-center p-6",
            div {
                class: "max-w-md text-center",
                "data-testid": "{testid}",
                h2 { class: "text-lg font-semibold text-text mb-2", "{title}" }
                p { class: "text-sm text-text-muted mb-4", "{body}" }
                div { class: "flex justify-center gap-2",
                    if can_retry {
                        button {
                            r#type: "button",
                            "data-testid": "viewed-room-retry",
                            class: "px-3 py-2 bg-surface hover:bg-surface-hover border border-border rounded-lg text-text text-sm transition-colors",
                            onclick: move |_| {
                                if let Some(owner_vk) = owner_vk {
                                    crate::util::defer(move || retry(owner_vk));
                                }
                            },
                            "Try again"
                        }
                    }
                    {close_button()}
                }
            }
        }
    }
}

/// One line of the read-only history.
struct Line {
    author: String,
    when: String,
    html: String,
}

/// The shown messages, oldest first: public text only, as the room's actions
/// (edits, deletions) leave it. Nicknames pass through the same sanitiser the
/// member's own view uses.
fn lines(state: &ChatRoomStateV1) -> Vec<Line> {
    let no_secrets = HashMap::new();
    state
        .recent_messages
        .display_messages()
        .filter_map(|m| {
            let text = state.recent_messages.effective_text(m)?;
            let author = state
                .member_info
                .member_info
                .iter()
                .find(|i| i.member_info.member_id == m.message.author)
                .map(|i| display_nickname(&i.member_info.preferred_nickname, &no_secrets))
                .unwrap_or_else(|| short_id(m.message.author));
            let ms = m
                .message
                .time
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            Some(Line {
                author,
                when: crate::util::format_utc_as_full_datetime(ms),
                html: crate::components::conversation::message_to_html(&text),
            })
        })
        .collect()
}

fn short_id(id: MemberId) -> String {
    let s = format!("{id:?}");
    s.chars().take(8).collect()
}

#[component]
fn PublicRoomView(
    owner_vk: Option<ed25519_dalek::VerifyingKey>,
    state: ChatRoomStateV1,
) -> Element {
    let config = &state.configuration.configuration;
    let name = config.display.name.to_string_lossy();
    let description = config
        .display
        .description
        .as_ref()
        .map(|d| d.to_string_lossy())
        .filter(|d| !d.is_empty());
    let link = owner_vk.map(|vk| public_room_link(&vk));
    let note = composer_note(&state);
    let rows = lines(&state);
    let can_join = offers_join(&state);
    let queued = match owner_vk {
        Some(o) => JOIN
            .try_read()
            .ok()
            .map(|j| j.queued_for(&o))
            .unwrap_or_default(),
        None => Vec::new(),
    };

    rsx! {
        div { class: "flex items-start gap-3 px-4 py-3 border-b border-border",
            div { class: "flex-1 min-w-0",
                h1 {
                    class: "text-base font-semibold text-text truncate",
                    "data-testid": "viewed-room-name",
                    "{name}"
                }
                p { class: "text-xs text-text-muted", "Public room, read-only" }
                if let Some(description) = description {
                    p { class: "text-sm text-text-secondary mt-1 break-words", "{description}" }
                }
            }
            if let Some(link) = link {
                CopyButton {
                    value: link,
                    testid: "viewed-room-copy-link",
                    label: "Copy public link",
                }
            }
            {close_button()}
        }
        div {
            class: "flex-1 overflow-y-auto px-4 py-3",
            "data-testid": "viewed-room-messages",
            if rows.is_empty() {
                p { class: "text-sm text-text-muted", "No messages yet." }
            }
            for (i, line) in rows.into_iter().enumerate() {
                div { key: "{i}", class: "mb-3", "data-testid": "viewed-room-message",
                    div { class: "text-xs text-text-muted",
                        span { class: "font-semibold text-text-secondary", "{line.author}" }
                        " "
                        span { "{line.when}" }
                    }
                    div {
                        class: "text-sm text-text break-words",
                        dangerous_inner_html: "{line.html}",
                    }
                }
            }
            for (i, text) in queued.into_iter().enumerate() {
                div {
                    key: "pending-{i}",
                    class: "mb-3 opacity-60",
                    "data-testid": "viewed-room-pending-message",
                    div { class: "text-xs text-text-muted", "You, waiting to join" }
                    div { class: "text-sm text-text whitespace-pre-wrap break-words", "{text}" }
                }
            }
        }
        if can_join {
            if let Some(owner) = owner_vk {
                JoinComposer {
                    key: "{crate::public_room::owner_key_b58(&owner)}",
                    owner_vk: owner,
                    note: note.to_string(),
                    readmit: false,
                }
            }
        } else {
            div { class: "px-4 py-3 border-t border-border",
                textarea {
                    class: "w-full px-3 py-2 bg-surface border border-border rounded-lg text-sm text-text-muted opacity-60 cursor-not-allowed",
                    "data-testid": "viewed-room-composer",
                    disabled: true,
                    rows: "1",
                    placeholder: "Joining is required to post",
                }
                p {
                    class: "mt-2 text-xs text-text-muted",
                    "data-testid": "viewed-room-composer-note",
                    "{note}"
                }
            }
        }
    }
}

/// The composer of a public room that admits by proof of work: Join, or type
/// and Send. Sending before admission queues the message, shown as pending
/// above; it posts once admission completes.
#[component]
pub(crate) fn JoinComposer(
    owner_vk: ed25519_dalek::VerifyingKey,
    note: String,
    #[props(default = false)] readmit: bool,
) -> Element {
    let mut draft = use_signal(String::new);

    // Text handed back after a failure, cancel or close returns to the draft in
    // front of what was typed since. The text is taken from the join itself,
    // so a second run of this effect finds nothing to take.
    use_effect(move || {
        crate::util::signal_guard::anchor();
        let has = match JOIN.try_read() {
            Ok(j) => j.has_returned_for(&owner_vk),
            Err(_) => {
                crate::util::signal_guard::schedule_nudge();
                return;
            }
        };
        if has {
            if let Some(back) = public_join::take_returned(&owner_vk) {
                let current = draft.peek().clone();
                let restored = public_join::restore_draft(&current, Some(&back));
                public_join::note_draft(owner_vk, &restored);
                draft.set(restored);
            }
        }
    });

    let (phase, queued_here, notice, raised) = match JOIN.try_read() {
        Ok(j) => (
            j.phase_for(&owner_vk),
            j.queued_for(&owner_vk),
            j.notice_for(&owner_vk),
            j.raised_for(&owner_vk),
        ),
        Err(_) => {
            crate::util::signal_guard::schedule_nudge();
            (JoinPhase::Idle, Vec::new(), None, false)
        }
    };
    let busy = matches!(phase, JoinPhase::Solving { .. } | JoinPhase::Submitting);
    let can_send = !draft.read().trim().is_empty() && phase != JoinPhase::Submitting;

    let mut send = move || {
        let text = draft.peek().clone();
        if text.trim().is_empty() {
            return;
        }
        public_join::note_draft(owner_vk, "");
        draft.set(String::new());
        crate::util::defer(move || public_join::send(owner_vk, text));
    };

    let (status_testid, status) = match &phase {
        JoinPhase::Idle => ("viewed-room-join-idle", note.clone()),
        // The room raised what it asks of newcomers: say so, as an explanation.
        JoinPhase::Solving { .. } if raised => (
            "viewed-room-join-progress",
            "Room security was raised; confirming you're human. You can keep writing.".to_string(),
        ),
        JoinPhase::Solving { tried, expected } => (
            "viewed-room-join-progress",
            if *expected > 0.0 {
                format!(
                    "Joining: proving you are not a bot ({}%). You can keep writing. Keep this tab open: a phone may pause a hidden tab.",
                    ((*tried as f64 / expected) * 100.0).min(99.0) as u32
                )
            } else {
                "Joining: proving you are not a bot. You can keep writing. Keep this tab open: a phone may pause a hidden tab.".to_string()
            },
        ),
        JoinPhase::Ready => (
            "viewed-room-join-ready",
            "Ready. Your first message makes you a member.".to_string(),
        ),
        JoinPhase::Submitting => ("viewed-room-join-submitting", "Posting...".to_string()),
        JoinPhase::Failed(f) => ("viewed-room-join-error", f.message()),
    };
    let failed = matches!(phase, JoinPhase::Failed(_));
    let retryable = matches!(&phase, JoinPhase::Failed(f) if f.retryable());
    let can_start = matches!(phase, JoinPhase::Idle) || retryable;

    // A re-admitted member's messages wait here: they are not in the room yet.
    let pending = if readmit { queued_here } else { Vec::new() };

    rsx! {
        div { class: "px-4 py-3 border-t border-border",
            for (i, text) in pending.into_iter().enumerate() {
                div {
                    key: "readmit-{i}",
                    class: "mb-2 opacity-60 text-sm whitespace-pre-wrap break-words",
                    "data-testid": "readmit-pending-message",
                    "{text}"
                }
            }
            div { class: "flex items-end gap-2",
                textarea {
                    class: "flex-1 px-3 py-2 bg-surface border border-border rounded-lg text-sm text-text",
                    "data-testid": "viewed-room-composer",
                    rows: "1",
                    placeholder: "Write a message",
                    value: "{draft}",
                    oninput: move |evt| {
                        let value = evt.value();
                        public_join::note_draft(owner_vk, &value);
                        draft.set(value);
                    },
                    onkeydown: move |evt| {
                        if evt.key() == Key::Enter && !evt.modifiers().shift() {
                            evt.prevent_default();
                            send();
                        }
                    },
                }
                if can_start {
                    button {
                        r#type: "button",
                        "data-testid": "viewed-room-join",
                        class: "flex-shrink-0 px-3 py-2 bg-surface hover:bg-surface-hover border border-border rounded-lg text-text text-sm transition-colors",
                        onclick: move |_| {
                            crate::util::defer(move || {
                                if readmit {
                                    public_join::start_readmit(owner_vk)
                                } else {
                                    public_join::start(owner_vk)
                                }
                            });
                        },
                        if failed { "Try again" } else if readmit { "Rejoin" } else { "Join" }
                    }
                }
                button {
                    r#type: "button",
                    "data-testid": "viewed-room-send",
                    class: "flex-shrink-0 px-3 py-2 bg-accent text-white rounded-lg text-sm disabled:opacity-50",
                    disabled: !can_send,
                    onclick: move |_| send(),
                    "Send"
                }
            }
            p {
                class: "mt-2 text-xs text-text-muted flex items-center gap-2",
                "data-testid": "{status_testid}",
                "{status}"
                if let Some(notice) = notice {
                    span { "data-testid": "viewed-room-join-notice", "{notice}" }
                }
                if busy && matches!(phase, JoinPhase::Solving { .. }) {
                    button {
                        r#type: "button",
                        "data-testid": "viewed-room-join-cancel",
                        class: "underline text-text-secondary",
                        onclick: move |_| crate::util::defer(public_join::cancel),
                        "Cancel"
                    }
                }
            }
        }
    }
}
