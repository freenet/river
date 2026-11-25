//! The read-only pane for a room opened by `?room=` link (freenet/river#513).
//! State and rules: [`crate::public_room`].

use crate::components::app::chat_delegate::{RoomsLoadState, ROOMS_LOAD_STATE};
use crate::components::app::freenet_api::freenet_synchronizer::SynchronizerStatus;
use crate::components::app::{MobileView, CURRENT_ROOM, MOBILE_VIEW, ROOMS, SYNC_STATUS};
use crate::components::back_to_chat::BackToChatButton;
use crate::components::icons::{line_icon, MENU_ICON_PATH};
use crate::components::room_list::receive_invitation_modal::invitation_accept_disabled;
use crate::public_join::{self, offers_join, JoinPhase, JOIN};
use crate::public_room::{
    close, composer_note, fetch_action, request_fetch, request_subscribe, retry, waiting_hint,
    FetchAction, ViewedRoom, ViewedRoomStatus, VIEWED_ROOM,
};
use crate::room_data::CurrentRoom;
use crate::util::display_name::{contains_hidden_chars, display_nickname, EMOJI_REJECTION_MESSAGE};
use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use river_core::room_state::member::MemberId;
use river_core::room_state::ChatRoomStateV1;
use std::collections::HashMap;
use std::time::UNIX_EPOCH;

/// Whether a load state is final for the viewer: anything but still loading
/// or migrating. A failed load is final too, so a visitor can still view a room
/// (the user's own rooms are not known then, but nothing more will arrive).
pub(crate) fn load_settled(state: RoomsLoadState) -> bool {
    !matches!(state, RoomsLoadState::Loading | RoomsLoadState::Migrating)
}

/// Whether the user's rooms have finished loading. A contended signal counts as
/// NOT settled: an answer is held rather than risk claiming the user's own room
/// as a visitor's, and the pane's effect nudges and re-runs to release it.
pub(crate) fn rooms_loaded() -> bool {
    ROOMS_LOAD_STATE
        .try_read()
        .map(|s| load_settled(*s))
        .unwrap_or(false)
}

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
        let just_connected = connected && !last_connected.replace(connected);
        // Only the lookup's first connect: a reconnect is not part of its timing.
        if just_connected && !asked.get() {
            crate::public_room::lookup_log("ws open");
        }
        if connected {
            // Route an answer held while the user's rooms loaded; the redirect
            // below opens it as theirs when it is.
            match ROOMS_LOAD_STATE.try_read() {
                Ok(state) => {
                    if load_settled(*state) {
                        crate::public_room::release_held();
                    }
                }
                // Stay subscribed: re-run once the signal is readable.
                Err(_) => crate::util::signal_guard::schedule_nudge(),
            }
        }
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
        match fetch_action(connected, just_connected, asked.get(), ours, status) {
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
        ViewedRoomStatus::Loading => {
            // Before the GET is sent the pane is waiting on the connection.
            // An unreadable signal falls back to the usual text.
            let connected = SYNC_STATUS
                .try_read()
                .map(|s| matches!(&*s, SynchronizerStatus::Connected));
            let held = crate::public_room::ANSWER_HELD.try_read().map(|v| *v);
            let hint = match (connected, held) {
                (Ok(c), Ok(h)) => waiting_hint(c, h),
                _ => None,
            };
            (
                "viewed-room-loading",
                "Looking for this room".to_string(),
                hint.unwrap_or("Asking the network for the room. This can take a few seconds.")
                    .to_string(),
            )
        }
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

/// One line of the history.
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
                .duration_since(UNIX_EPOCH.into())
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

/// The people in the room as its member list shows them: the owner first, then
/// the members, by nickname. Provisional (message-only) authors are not listed,
/// as in a joined room's list.
fn member_names(state: &ChatRoomStateV1, owner_vk: Option<&VerifyingKey>) -> Vec<String> {
    let no_secrets = HashMap::new();
    let name_of = |id: MemberId| {
        state
            .member_info
            .canonical(id)
            .map(|mi| display_nickname(&mi.member_info.preferred_nickname, &no_secrets))
            .unwrap_or_else(|| crate::util::display_name::UNKNOWN_MEMBER.to_string())
    };
    let mut names = Vec::new();
    if let Some(owner) = owner_vk {
        names.push(name_of(MemberId::from(*owner)));
    }
    let mut rest: Vec<String> = state
        .members
        .members
        .iter()
        .map(|m| name_of(m.member.id()))
        .collect();
    rest.sort_by_key(|n| n.to_lowercase());
    names.extend(rest);
    names
}

/// The label of the member count in the header and the list.
fn member_count_label(count: usize) -> String {
    if count == 1 {
        "1 member".to_string()
    } else {
        format!("{count} members")
    }
}

/// The members panel for a viewed room: names only, read from the viewed
/// room's state. A visitor is listed once their first message has landed.
#[component]
pub fn ViewedMemberList() -> Element {
    let viewed = match VIEWED_ROOM.try_read() {
        Ok(v) => v.clone(),
        Err(_) => {
            crate::util::signal_guard::schedule_nudge();
            return rsx! {};
        }
    };
    let Some(ViewedRoom {
        owner_vk,
        status: ViewedRoomStatus::Public(state),
    }) = viewed
    else {
        return rsx! {};
    };
    let names = member_names(&state, owner_vk.as_ref());
    let total = names.len();
    rsx! {
        aside {
            "data-testid": "members-rail",
            id: "members-rail",
            class: "w-full md:w-62 flex-shrink-0 bg-rail border-l border-border flex flex-col",
            div { class: "pl-4 pr-2.5 py-3 border-b border-border flex-shrink-0",
                div { class: "flex items-center gap-2 min-h-9",
                    BackToChatButton {
                        class: "md:hidden p-1",
                        icon_size: 14,
                        testid: "members-back-to-chat-button",
                        current_room_badge: "members-current-room-unread-badge",
                    }
                    h2 { class: "flex-1 min-w-0 text-[14.5px] font-bold text-text flex items-center gap-2",
                        span { class: "truncate", "Members" }
                        span { class: "text-xs font-medium text-text-muted tabular-nums", "{total}" }
                    }
                }
            }
            ul {
                "data-testid": "member-list",
                class: "flex-1 px-2 py-2 space-y-0.5 overflow-y-auto min-h-0",
                for (i, name) in names.into_iter().enumerate() {
                    li {
                        key: "{i}",
                        "data-testid": "viewed-room-member",
                        class: "h-10 px-2 gap-2.5 rounded-lg text-[14px] text-text-secondary flex items-center min-w-0",
                        span {
                            "aria-hidden": "true",
                            class: "msg-avatar member-avatar",
                            "{crate::components::conversation::avatar::initials(&name)}"
                        }
                        span { class: "truncate min-w-0", "{name}" }
                    }
                }
            }
        }
    }
}

/// The viewed room's row in the room rail: shown while the room is open, never
/// saved. The x closes it.
#[component]
pub fn ViewedRailRow() -> Element {
    let viewed = match VIEWED_ROOM.try_read() {
        Ok(v) => v.clone(),
        Err(_) => {
            crate::util::signal_guard::schedule_nudge();
            return rsx! {};
        }
    };
    let Some(ViewedRoom {
        owner_vk: Some(_),
        status: ViewedRoomStatus::Public(state),
    }) = viewed
    else {
        return rsx! {};
    };
    let name = state
        .configuration
        .configuration
        .display
        .name
        .to_string_lossy();
    rsx! {
        li {
            "data-testid": "viewed-room-rail-row",
            class: "tooltip-frame rounded-lg select-none",
            div { class: "flex items-center",
                span {
                    class: "flex-1 min-w-0 text-left px-2.5 h-[34px] flex items-center rounded-lg text-[14.5px] bg-surface text-text font-semibold truncate",
                    "{name}"
                }
                button {
                    r#type: "button",
                    "data-testid": "viewed-room-close",
                    class: "flex-shrink-0 mx-1 w-7 h-7 rounded-lg text-text-muted hover:text-text hover:bg-surface transition-colors",
                    title: "Close this room (you haven't joined)",
                    "aria-label": "Close this room (you haven't joined)",
                    onclick: move |_| close(),
                    "\u{00d7}"
                }
            }
        }
    }
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
    let note = composer_note(&state);
    let rows = lines(&state);
    let can_join = offers_join(&state);
    let member_count = member_names(&state, owner_vk.as_ref()).len();
    let queued = match owner_vk {
        Some(o) => JOIN
            .try_read()
            .ok()
            .map(|j| j.queued_for(&o))
            .unwrap_or_default(),
        None => Vec::new(),
    };

    rsx! {
        div { class: "flex-shrink-0 px-3 md:pl-5 md:pr-3 py-3 border-b border-border bg-panel",
            div { class: "flex items-center justify-between gap-2 md:gap-3",
                button {
                    r#type: "button",
                    class: "md:hidden flex-shrink-0 mr-1 p-2 rounded-lg text-text-muted hover:text-text hover:bg-surface transition-colors",
                    "aria-label": "Open room list",
                    onclick: move |_| crate::util::defer(move || *MOBILE_VIEW.write() = MobileView::Rooms),
                    {line_icon(MENU_ICON_PATH, 20)}
                }
                div { class: "min-w-0 flex-1",
                    h2 {
                        class: "text-[16px] leading-6 font-semibold text-text truncate",
                        "data-testid": "viewed-room-name",
                        "{name}"
                    }
                    if let Some(description) = description {
                        p { class: "text-sm text-text-secondary break-words", "{description}" }
                    }
                }
                button {
                    r#type: "button",
                    class: "md:hidden flex-shrink-0 px-2 py-1 rounded-lg text-xs text-text-muted hover:text-text hover:bg-surface transition-colors",
                    "data-testid": "viewed-room-members-button",
                    onclick: move |_| crate::util::defer(move || *MOBILE_VIEW.write() = MobileView::Members),
                    "{member_count_label(member_count)}"
                }
                span {
                    class: "hidden md:inline flex-shrink-0 text-xs text-text-muted",
                    "data-testid": "viewed-room-member-count",
                    "{member_count_label(member_count)}"
                }
            }
        }
        div {
            class: "flex-1 overflow-y-auto min-h-0",
            "data-testid": "viewed-room-messages",
            div { class: "msg-history",
                if rows.is_empty() && queued.is_empty() {
                    p { class: "px-4 py-3 text-sm text-text-muted", "No messages yet." }
                }
                for (i, line) in rows.into_iter().enumerate() {
                    div {
                        key: "{i}",
                        class: "msg-row",
                        "data-testid": "viewed-room-message",
                        div { class: "msg-gutter",
                            div {
                                "aria-hidden": "true",
                                class: "msg-avatar",
                                "{crate::components::conversation::avatar::initials(&line.author)}"
                            }
                        }
                        div { class: "msg-content relative min-w-0",
                            div { class: "flex flex-wrap items-baseline gap-x-2 mb-0.5 leading-5",
                                span { class: "text-[15px] font-bold text-text", "{line.author}" }
                                span { class: "text-xs text-text-muted", "{line.when}" }
                            }
                            div {
                                class: "msg-body text-[15px] leading-[22px] text-text max-w-[76ch]",
                                dangerous_inner_html: "{line.html}",
                            }
                        }
                    }
                }
                for (i, text) in queued.into_iter().enumerate() {
                    div {
                        key: "pending-{i}",
                        class: "msg-row opacity-60",
                        "data-testid": "viewed-room-pending-message",
                        // Bring the waiting message into view, as a sent one
                        // would be in a joined room.
                        onmounted: move |cx| {
                            let element = cx.data();
                            crate::util::safe_spawn_local(async move {
                                let _ = element.scroll_to(ScrollBehavior::Instant).await;
                            });
                        },
                        div { class: "msg-gutter",
                            div { "aria-hidden": "true", class: "msg-avatar msg-avatar-self", "Y" }
                        }
                        div { class: "msg-content relative min-w-0",
                            div { class: "text-xs text-text-muted mb-0.5", "You, waiting to join" }
                            div { class: "msg-body text-[15px] leading-[22px] text-text whitespace-pre-wrap break-words", "{text}" }
                            // The delay, said once and quietly.
                            div {
                                class: "mt-0.5 text-xs text-text-muted",
                                "data-testid": "viewed-room-sending-notice",
                                span { "Sending in a moment\u{2026}" }
                                span { class: "block", "A short delay that helps keep the room free of spam." }
                            }
                        }
                    }
                }
            }
        }
        if can_join {
            if let Some(owner) = owner_vk {
                JoinComposer {
                    key: "{crate::public_room::owner_key_b58(&owner)}",
                    owner_vk: owner,
                    note: String::new(),
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

/// The prompt shown at a visitor's first message: one display name, filled
/// with the generated default. Continue or Enter keeps what is in the box;
/// Escape keeps the default. It never holds up the proof of work.
#[component]
fn NicknamePrompt(default: String, on_done: EventHandler<String>) -> Element {
    let mut nickname = use_signal({
        let default = default.clone();
        move || default
    });
    let has_emoji = contains_hidden_chars(&nickname.read());
    let ceiling = river_core::room_state::member_info::NICKNAME_CEILING;
    let too_long = nickname.read().len() > ceiling;
    let disabled = invitation_accept_disabled(&nickname.read(), None);
    let default_for_esc = default.clone();
    rsx! {
        div {
            class: "fixed inset-0 z-50 flex items-center justify-center",
            div { class: "absolute inset-0 bg-black/50" }
            div {
                "data-testid": "viewed-room-nickname-modal",
                class: "relative z-10 w-full max-w-md mx-4 bg-panel rounded-xl shadow-xl border border-border",
                div { class: "p-6",
                    p { class: "text-[13px] text-text-muted mb-4",
                        "Choose "
                        span { class: "text-text font-medium", "your own display name" }
                        " (not the room's name). It's what other members will see when you send messages:"
                    }
                    div { class: "mb-4",
                        label { class: "block text-sm font-medium text-text mb-1", "Your display name" }
                        input {
                            "data-testid": "viewed-room-nickname-input",
                            class: if has_emoji {
                                "w-full px-3 py-2 bg-surface border border-danger rounded-lg text-text placeholder-text-muted focus:outline-none focus:ring-2 focus:ring-danger focus:border-transparent"
                            } else {
                                "w-full px-3 py-2 bg-surface border border-border rounded-xl text-text placeholder-text-muted focus:outline-none focus:border-text-muted"
                            },
                            "aria-invalid": if has_emoji { "true" } else { "false" },
                            r#type: "text",
                            value: "{nickname}",
                            onmounted: move |cx| {
                                let element = cx.data();
                                crate::util::safe_spawn_local(async move {
                                    let _ = element.set_focus(true).await;
                                });
                            },
                            oninput: move |evt| nickname.set(evt.value()),
                            onkeydown: {
                                let default_for_esc = default_for_esc.clone();
                                move |evt: KeyboardEvent| {
                                    if evt.key() == Key::Enter && !disabled {
                                        evt.prevent_default();
                                        let name = nickname.peek().clone();
                                        on_done.call(name);
                                    } else if evt.key() == Key::Escape {
                                        evt.prevent_default();
                                        on_done.call(default_for_esc.clone());
                                    }
                                }
                            },
                            placeholder: "Your name (e.g. Alex)",
                        }
                        if has_emoji {
                            p { class: "mt-1 text-xs text-danger", role: "alert", "{EMOJI_REJECTION_MESSAGE}" }
                        }
                        if too_long {
                            p { class: "mt-1 text-xs text-danger", role: "alert",
                                "A display name can be at most {ceiling} bytes."
                            }
                        }
                    }
                    div { class: "flex gap-3",
                        button {
                            r#type: "button",
                            "data-testid": "viewed-room-nickname-continue",
                            class: "px-4 py-2 bg-surface hover:bg-surface-hover border border-border-hover text-text font-medium rounded-lg transition-colors disabled:opacity-50 disabled:cursor-not-allowed",
                            disabled,
                            onclick: move |_| {
                                let name = nickname.peek().clone();
                                on_done.call(name);
                            },
                            "Continue"
                        }
                    }
                }
            }
        }
    }
}

/// The composer of a public room that admits by proof of work. A visitor just
/// types and sends: the proof starts on the first focus or keystroke, and a
/// message sent before it is done waits, shown as pending above, and posts
/// when it completes. A re-admitted member (`readmit`) keeps the Rejoin button.
#[component]
pub(crate) fn JoinComposer(
    owner_vk: ed25519_dalek::VerifyingKey,
    note: String,
    #[props(default = false)] readmit: bool,
) -> Element {
    let mut draft = use_signal(String::new);
    // The display-name prompt: `Some(default)` while it is open. Asked once
    // per composer, at the first message. `hold_send` means the message waits
    // for the answer (the proof is already done, so nothing is hidden behind
    // the wait).
    let mut nick_prompt = use_signal(|| None::<String>);
    let mut nick_asked = use_signal(|| false);
    let mut hold_send = use_signal(|| false);

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

    let (phase, queued_here, notice) = match JOIN.try_read() {
        Ok(j) => (
            j.phase_for(&owner_vk),
            j.queued_for(&owner_vk),
            j.notice_for(&owner_vk),
        ),
        Err(_) => {
            crate::util::signal_guard::schedule_nudge();
            (JoinPhase::Idle, Vec::new(), None)
        }
    };
    let can_send = !draft.read().trim().is_empty() && phase != JoinPhase::Submitting;

    // Sends the draft now.
    let mut do_send = move || {
        let text = draft.peek().clone();
        if text.trim().is_empty() {
            return;
        }
        public_join::note_draft(owner_vk, "");
        draft.set(String::new());
        crate::util::defer(move || public_join::send(owner_vk, text));
    };
    let ready = matches!(phase, JoinPhase::Ready);
    let mut send = move || {
        if draft.peek().trim().is_empty() {
            return;
        }
        if !readmit && !*nick_asked.peek() {
            if let Some(default) = public_join::default_nickname() {
                nick_asked.set(true);
                nick_prompt.set(Some(default));
                if ready {
                    hold_send.set(true);
                    return;
                }
            }
        }
        do_send();
    };
    let on_nickname = move |name: String| {
        let name = if name.trim().is_empty() {
            public_join::default_nickname().unwrap_or_default()
        } else {
            name
        };
        public_join::set_nickname(name);
        nick_prompt.set(None);
        if *hold_send.peek() {
            hold_send.set(false);
            do_send();
        }
    };

    // A visitor's first interaction starts the proof; a re-admitted member's
    // Rejoin button does.
    let wake = move || {
        if !readmit {
            crate::util::defer(move || public_join::start_on_interaction(owner_vk));
        }
    };

    let retryable = matches!(&phase, JoinPhase::Failed(f) if f.retryable());
    let failed = matches!(phase, JoinPhase::Failed(_));
    let solving = matches!(phase, JoinPhase::Solving { .. });
    // The line under the composer. A visitor sees nothing about the work
    // itself: the pending message says it once. A re-admitted member keeps
    // the wording of the re-join.
    let (status_testid, status) = if readmit {
        match &phase {
            JoinPhase::Idle => ("viewed-room-join-idle", note.clone()),
            JoinPhase::Solving { tried, expected } => (
                "viewed-room-join-progress",
                if *expected > 0.0 {
                    format!(
                        "Joining: proving you are not a bot ({}%). You can keep writing. Keep this tab visible: a hidden tab works slowly.",
                        ((*tried as f64 / expected) * 100.0).min(99.0) as u32
                    )
                } else {
                    "Joining: proving you are not a bot. You can keep writing. Keep this tab visible: a hidden tab works slowly.".to_string()
                },
            ),
            JoinPhase::Ready => (
                "viewed-room-join-ready",
                "Ready. Your first message makes you a member.".to_string(),
            ),
            JoinPhase::Submitting => ("viewed-room-join-submitting", "Posting...".to_string()),
            JoinPhase::Failed(f) => ("viewed-room-join-error", f.message()),
        }
    } else {
        match &phase {
            JoinPhase::Failed(f) => ("viewed-room-join-error", f.message()),
            _ => ("viewed-room-join-status", String::new()),
        }
    };
    let show_cancel = solving && (readmit || !queued_here.is_empty());
    let show_status_line = !status.is_empty() || notice.is_some() || show_cancel;
    let can_start = readmit && (matches!(phase, JoinPhase::Idle) || retryable);
    let show_retry = !readmit && retryable;

    // A re-admitted member's messages wait here: they are not in the room yet.
    let pending = if readmit { queued_here } else { Vec::new() };

    rsx! {
        if let Some(default) = nick_prompt.read().clone() {
            NicknamePrompt { default, on_done: on_nickname }
        }
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
                    onfocus: move |_| wake(),
                    oninput: move |evt| {
                        let value = evt.value();
                        public_join::note_draft(owner_vk, &value);
                        draft.set(value);
                        wake();
                    },
                    onkeydown: move |evt| {
                        if evt.key() == Key::Enter && !evt.modifiers().shift() {
                            evt.prevent_default();
                            send();
                        }
                    },
                }
                if can_start || show_retry {
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
                        if failed { "Try again" } else { "Rejoin" }
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
            if show_status_line {
                p {
                    class: "mt-2 text-xs text-text-muted flex items-center gap-2",
                    "data-testid": "{status_testid}",
                    "{status}"
                    if let Some(notice) = notice {
                        span { "data-testid": "viewed-room-join-notice", "{notice}" }
                    }
                    if show_cancel {
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
}

#[cfg(test)]
mod view_tests {
    use super::*;

    #[test]
    fn the_member_count_reads_as_a_number_of_members() {
        assert_eq!(member_count_label(1), "1 member");
        assert_eq!(member_count_label(0), "0 members");
        assert_eq!(member_count_label(12), "12 members");
    }
}

#[cfg(test)]
mod load_state_tests {
    use super::*;

    #[test]
    fn only_a_load_still_in_progress_holds_the_viewers_answer() {
        assert!(!load_settled(RoomsLoadState::Loading));
        assert!(!load_settled(RoomsLoadState::Migrating));
        assert!(load_settled(RoomsLoadState::Loaded));
        assert!(
            load_settled(RoomsLoadState::LoadFailed),
            "a failed load must not hold a visitor's answer forever"
        );
    }
}
