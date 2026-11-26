//! The read-only pane for a room opened by `?room=` link (freenet/river#513).
//! State and rules: [`crate::public_room`].

use crate::components::app::chat_delegate::{RoomsLoadState, ROOMS_LOAD_STATE};
use crate::components::app::freenet_api::freenet_synchronizer::SynchronizerStatus;
use crate::components::app::{MobileView, MOBILE_VIEW, ROOMS, SYNC_STATUS};
use crate::components::back_to_chat::BackToChatButton;
use crate::components::icons::{line_icon, MENU_ICON_PATH};
use crate::components::room_list::receive_invitation_modal::invitation_accept_disabled;
use crate::public_join::{self, offers_join, JoinPhase, JOIN};
use crate::public_room::{
    close, composer_note, fetch_action, request_fetch, request_subscribe, retry, retry_handle,
    waiting_hint, FetchAction, ViewedRoom, ViewedRoomStatus, VIEWED_ROOM,
};
use crate::short_handle::ShortHandle;
use crate::short_resolve;
use crate::util::display_name::{contains_hidden_chars, display_nickname, EMOJI_REJECTION_MESSAGE};
use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use river_core::room_state::member::MemberId;
use river_core::room_state::ChatRoomStateV1;
use std::collections::HashMap;
use std::time::UNIX_EPOCH;

/// What every way of leaving a room you have not joined says.
const CLOSE_LABEL: &str = "Close this room (you haven't joined)";

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
    // The short link whose resolver GET this pane has sent.
    let resolve_asked: std::rc::Rc<std::cell::Cell<Option<ShortHandle>>> =
        use_hook(|| std::rc::Rc::new(std::cell::Cell::new(None)));
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
        // A short link: resolve the handle first, in parallel with nothing
        // else (not gated on the user's rooms loading). The no-sync example
        // build has no socket and is answered from fixtures.
        if let Some(ViewedRoom {
            owner_vk: None,
            handle: Some(handle),
            status,
        }) = viewed.as_ref()
        {
            let can_send = connected || cfg!(feature = "no-sync");
            if short_resolve::should_request(can_send, resolve_asked.get(), *handle, status) {
                resolve_asked.set(Some(*handle));
                short_resolve::request(*handle);
            }
            return;
        }
        // Not (or no longer) a short link being resolved: a later link to the
        // same handle, followed in place, must send its own GET.
        resolve_asked.set(None);
        let Some(ViewedRoom {
            owner_vk: Some(owner_vk),
            status,
            ..
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
            // The timer can fire long after the effect ran (a loaded WebKit
            // delays it by 150 ms or more): the viewer may show another room
            // by then (freenet/river#1047). It re-checks inside.
            crate::public_room::redirect_to_member_room(owner);
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
                other => rsx! {
                    Notice { status: other, owner_vk: viewed.owner_vk, handle: viewed.handle }
                },
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
            title: CLOSE_LABEL,
            "aria-label": CLOSE_LABEL,
            onclick: move |_| close(),
            "Close"
        }
    }
}

#[component]
fn Notice(
    status: ViewedRoomStatus,
    owner_vk: Option<ed25519_dalek::VerifyingKey>,
    handle: Option<ShortHandle>,
) -> Element {
    let (testid, title, body) = match &status {
        ViewedRoomStatus::Resolving => {
            let connected = SYNC_STATUS
                .try_read()
                .map(|s| matches!(&*s, SynchronizerStatus::Connected))
                .unwrap_or(false);
            (
                "viewed-room-resolving",
                "Opening short link".to_string(),
                short_resolve::waiting_hint(connected || cfg!(feature = "no-sync")).to_string(),
            )
        }
        ViewedRoomStatus::HandleNotFound => (
            "viewed-room-handle-not-found",
            "Short link not found".to_string(),
            "This short link isn't registered yet.".to_string(),
        ),
        ViewedRoomStatus::HandleTransportError => (
            "viewed-room-handle-error",
            "Couldn't look up this short link".to_string(),
            "The lookup failed before it got an answer, which says nothing about the link. Check your connection and try again."
                .to_string(),
        ),
        ViewedRoomStatus::HandleTimedOut => (
            "viewed-room-handle-timed-out",
            "No answer yet".to_string(),
            "Nothing replied about this short link. Check your connection and try again."
                .to_string(),
        ),
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
    let can_retry = (matches!(
        status,
        ViewedRoomStatus::NotFound | ViewedRoomStatus::TimedOut
    ) && owner_vk.is_some())
        || (matches!(
            status,
            ViewedRoomStatus::HandleNotFound
                | ViewedRoomStatus::HandleTransportError
                | ViewedRoomStatus::HandleTimedOut
        ) && handle.is_some());
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
                                } else if let Some(handle) = handle {
                                    crate::util::defer(move || retry_handle(handle));
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
    /// Who wrote it, when, and how long: tells one message from another when
    /// the window is full and an arrival replaces the oldest.
    id: String,
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
                id: format!("{:?}:{ms}:{}", m.message.author, text.len()),
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
    crate::util::signal_guard::anchor();
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
        ..
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
    crate::util::signal_guard::anchor();
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
        ..
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
                // On a phone the rail replaces the chat: tapping the row goes back to it.
                button {
                    r#type: "button",
                    "data-testid": "viewed-room-rail-name",
                    class: "flex-1 min-w-0 text-left px-2.5 h-[34px] flex items-center rounded-lg text-[14.5px] bg-surface text-text font-semibold truncate",
                    onclick: move |_| crate::util::defer(move || *MOBILE_VIEW.write() = MobileView::Chat),
                    "{name}"
                }
                button {
                    r#type: "button",
                    "data-testid": "viewed-room-close",
                    class: "flex-shrink-0 mx-1 w-7 h-7 pointer-coarse:w-11 pointer-coarse:h-11 rounded-lg text-text-muted hover:text-text hover:bg-surface transition-colors",
                    title: CLOSE_LABEL,
                    "aria-label": CLOSE_LABEL,
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
    crate::util::signal_guard::anchor();
    let (queued, waiting_line) = match owner_vk {
        Some(o) => match JOIN.try_read() {
            Ok(j) => (
                j.queued_for(&o),
                solving_line(&j.phase_for(&o), j.raised_for(&o), false),
            ),
            Err(_) => {
                crate::util::signal_guard::schedule_nudge();
                (Vec::new(), None)
            }
        },
        None => (Vec::new(), None),
    };
    let last_queued = queued.len().saturating_sub(1);
    // Bring the newest row (or the waiting message) into view when the rows
    // change, as a joined room opens at its newest message, but leave a reader
    // who has scrolled up where they are (see `follow_to_end`).
    let queued_len = queued.len();
    let history_size = HistoryMark {
        rows: rows.len(),
        newest: rows.last().map(|l| l.id.clone()),
        queued: queued_len,
    };
    let mut end_el = use_signal(|| None::<std::rc::Rc<MountedData>>);
    let mut handled = use_signal(|| None::<HistoryMark>);
    if handled.peek().as_ref() != Some(&history_size) {
        // Measured now, before this render's rows reach the page: the pane
        // still has the old content, so this says whether the reader was at
        // its end. (A scroll event would also fire when content is added.)
        let near = history_is_near_bottom();
        let history_size = history_size.clone();
        crate::util::defer(move || {
            // The pane may have closed since: its signals are then gone.
            let state = match (handled.try_peek(), end_el.try_peek()) {
                (Ok(previous), Ok(element)) => Some((previous.clone(), element.clone())),
                _ => None,
            };
            let Some((previous, element)) = state else {
                return;
            };
            if previous.as_ref() == Some(&history_size) {
                return;
            }
            handled.set(Some(history_size.clone()));
            if follow_to_end(previous.map(|m| m.queued), history_size.queued, near) {
                if let Some(element) = element {
                    crate::util::safe_spawn_local(async move {
                        let _ = element.scroll_to(ScrollBehavior::Instant).await;
                    });
                }
            }
        });
    }

    rsx! {
        div { class: "flex-shrink-0 px-3 md:pl-5 md:pr-3 py-3 border-b border-border bg-panel",
            div { class: "flex items-center justify-between gap-2 md:gap-3",
                button {
                    r#type: "button",
                    class: "md:hidden flex-shrink-0 mr-1 p-2 min-w-11 min-h-11 rounded-lg text-text-muted hover:text-text hover:bg-surface transition-colors",
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
                    class: "md:hidden flex-shrink-0 px-2 min-h-11 rounded-lg text-xs text-text-muted hover:text-text hover:bg-surface transition-colors",
                    "data-testid": "viewed-room-members-button",
                    onclick: move |_| crate::util::defer(move || *MOBILE_VIEW.write() = MobileView::Members),
                    "{member_count_label(member_count)}"
                }
                span {
                    class: "hidden md:inline flex-shrink-0 text-xs text-text-muted",
                    "data-testid": "viewed-room-member-count",
                    "{member_count_label(member_count)}"
                }
                // The rail is hidden on a phone; its x is the way out on a wide
                // screen, this one on a narrow one.
                button {
                    r#type: "button",
                    "data-testid": "viewed-room-close-narrow",
                    class: "md:hidden flex-shrink-0 w-11 h-11 rounded-lg text-text-muted hover:text-text hover:bg-surface transition-colors",
                    title: CLOSE_LABEL,
                    "aria-label": CLOSE_LABEL,
                    onclick: move |_| close(),
                    "\u{00d7}"
                }
            }
        }
        div {
            class: "flex-1 overflow-y-auto min-h-0",
            "data-testid": "viewed-room-messages",
            id: HISTORY_PANE_ID,
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
                                if i == last_queued {
                                    if let Some(line) = waiting_line.clone() {
                                        span {
                                            class: "block tabular-nums",
                                            "data-testid": "viewed-room-join-progress",
                                            "{line}"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                div {
                    "aria-hidden": "true",
                    onmounted: move |cx| {
                        // The pane opens at its newest message.
                        let element = cx.data();
                        end_el.set(Some(element.clone()));
                        crate::util::safe_spawn_local(async move {
                            let _ = element.scroll_to(ScrollBehavior::Instant).await;
                        });
                    },
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

/// How the display-name prompt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NameAnswer {
    /// Escape: nothing is sent and the text stays in the box.
    Cancel,
    /// Enter or Continue: send now; `None` keeps the generated default.
    Send(Option<String>),
}

/// Where Tab (or Shift+Tab) goes next in the name dialog: the field, then
/// Continue (skipped while it is disabled), then Cancel, round again.
fn next_dialog_focus(current: &str, continue_disabled: bool, backwards: bool) -> &'static str {
    let order: &[&'static str] = if continue_disabled {
        &[NICKNAME_FIELD_ID, NICKNAME_CANCEL_ID]
    } else {
        &[NICKNAME_FIELD_ID, NICKNAME_CONTINUE_ID, NICKNAME_CANCEL_ID]
    };
    let at = order.iter().position(|id| *id == current).unwrap_or(0);
    let next = if backwards {
        (at + order.len() - 1) % order.len()
    } else {
        (at + 1) % order.len()
    };
    order[next]
}

/// The id of the control that has focus, if any.
fn focused_id() -> String {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.active_element())
        .map(|e| e.id())
        .unwrap_or_default()
}

const NICKNAME_CONTINUE_ID: &str = "viewed-room-nickname-continue-button";
const NICKNAME_CANCEL_ID: &str = "viewed-room-nickname-cancel-button";
const NICKNAME_FIELD_ID: &str = "viewed-room-nickname-field";

/// What a confirmed name means: the default itself, or nothing, is "no
/// choice", so the join keeps whatever default its own key gives.
fn name_choice(default: &str, typed: &str) -> Option<String> {
    let typed = typed.trim();
    if typed.is_empty() || typed == default {
        None
    } else {
        Some(typed.to_string())
    }
}

/// The prompt shown at a visitor's first message: one display name, filled
/// with the generated default. Continue or Enter keeps what is in the box;
/// Escape keeps the default. It never holds up the proof of work.
#[component]
fn NicknamePrompt(default: String, on_done: EventHandler<NameAnswer>) -> Element {
    let mut nickname = use_signal({
        let default = default.clone();
        move || default
    });
    // Focus is kept inside the dialog: the two controls take turns on Tab.
    let mut input_el = use_signal(|| None::<std::rc::Rc<MountedData>>);
    let mut button_el = use_signal(|| None::<std::rc::Rc<MountedData>>);
    let mut cancel_el = use_signal(|| None::<std::rc::Rc<MountedData>>);
    let has_emoji = contains_hidden_chars(&nickname.read());
    let too_long = nickname.read().len() > river_core::room_state::member_info::NICKNAME_CEILING;
    let disabled = invitation_accept_disabled(&nickname.read(), None);
    rsx! {
        div {
            // The layer scrolls and the dialog centres with `m-auto`, which
            // centres when it fits and top-aligns when it does not, so on a
            // short screen (a phone with the keyboard up) every control can
            // be scrolled to and tapped instead of being clipped.
            class: "fixed inset-0 z-50 flex overflow-y-auto overscroll-contain p-4",
            div { class: "fixed inset-0 bg-black/50" }
            div {
                "data-testid": "viewed-room-nickname-modal",
                role: "dialog",
                "aria-modal": "true",
                "aria-labelledby": "viewed-room-nickname-title",
                "aria-describedby": "viewed-room-nickname-text",
                class: "relative z-10 w-full max-w-md m-auto bg-panel rounded-xl shadow-xl border border-border",
                // Escape cancels (nothing is sent), wherever focus is; Tab stays
                // inside the dialog.
                onkeydown: {
                    let default = default.clone();
                    move |evt: KeyboardEvent| match evt.key() {
                        Key::Escape => {
                            evt.prevent_default();
                            on_done.call(NameAnswer::Cancel);
                        }
                        Key::Enter
                            if !disabled
                                && focused_id() != NICKNAME_CONTINUE_ID
                                && focused_id() != NICKNAME_CANCEL_ID =>
                        {
                            evt.prevent_default();
                            let name = nickname.peek().clone();
                            on_done.call(NameAnswer::Send(name_choice(&default, &name)));
                        }
                        Key::Tab => {
                            evt.prevent_default();
                            // From the control that really has focus, so a click
                            // inside the dialog cannot leave this out of step.
                            let target = match next_dialog_focus(
                                &focused_id(),
                                disabled,
                                evt.modifiers().shift(),
                            ) {
                                NICKNAME_CONTINUE_ID => button_el.peek().clone(),
                                NICKNAME_CANCEL_ID => cancel_el.peek().clone(),
                                _ => input_el.peek().clone(),
                            };
                            if let Some(el) = target {
                                crate::util::safe_spawn_local(async move {
                                    let _ = el.set_focus(true).await;
                                });
                            }
                        }
                        _ => {}
                    }
                },
                div { class: "p-6",
                    h2 {
                        id: "viewed-room-nickname-title",
                        class: "text-base font-semibold text-text mb-1",
                        "Pick a name to show next to your message"
                    }
                    p {
                        id: "viewed-room-nickname-text",
                        class: "text-[13px] text-text-muted mb-4",
                        "Your message is ready and will send as soon as you continue."
                    }
                    div { class: "mb-4",
                        label {
                            r#for: "viewed-room-nickname-field",
                            class: "block text-sm font-medium text-text mb-1",
                            "Your display name"
                        }
                        input {
                            id: NICKNAME_FIELD_ID,
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
                                input_el.set(Some(element.clone()));
                                crate::util::safe_spawn_local(async move {
                                    let _ = element.set_focus(true).await;
                                });
                            },
                            oninput: move |evt| nickname.set(evt.value()),
                            placeholder: "Your name (e.g. Alex)",
                        }
                        if has_emoji {
                            p { class: "mt-1 text-xs text-danger", role: "alert", "{EMOJI_REJECTION_MESSAGE}" }
                        }
                        if too_long {
                            p { class: "mt-1 text-xs text-danger", role: "alert",
                                "That display name is too long."
                            }
                        }
                        p { class: "mt-2 text-xs text-text-muted",
                            "Enter or Continue sends it. Cancel keeps your message in the box."
                        }
                    }
                    div { class: "flex gap-3",
                        button {
                            r#type: "button",
                            id: NICKNAME_CONTINUE_ID,
                            "data-testid": "viewed-room-nickname-continue",
                            class: "px-4 py-2 min-h-11 bg-surface hover:bg-surface-hover border border-border-hover text-text font-medium rounded-lg transition-colors disabled:opacity-50 disabled:cursor-not-allowed",
                            disabled,
                            onmounted: move |cx| button_el.set(Some(cx.data())),
                            onclick: {
                                let default = default.clone();
                                move |_| {
                                    let name = nickname.peek().clone();
                                    on_done.call(NameAnswer::Send(name_choice(&default, &name)));
                                }
                            },
                            "Continue"
                        }
                        button {
                            r#type: "button",
                            id: NICKNAME_CANCEL_ID,
                            "data-testid": "viewed-room-nickname-cancel",
                            class: "px-4 py-2 min-h-11 text-text-secondary hover:text-text rounded-lg transition-colors",
                            onmounted: move |cx| cancel_el.set(Some(cx.data())),
                            onclick: move |_| on_done.call(NameAnswer::Cancel),
                            "Cancel"
                        }
                    }
                }
            }
        }
    }
}

/// How close to the end of the history, in pixels, still counts as reading
/// the newest messages.
const NEAR_BOTTOM_PX: f64 = 80.0;

/// The DOM id of the history pane.
const HISTORY_PANE_ID: &str = "viewed-room-history-pane";

/// Whether the history pane, as it is on the page now, is scrolled to its end.
/// True when it is not on the page yet.
fn history_is_near_bottom() -> bool {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(HISTORY_PANE_ID))
        .map(|el| {
            is_near_bottom(
                el.scroll_top() as f64,
                el.scroll_height() as f64,
                el.client_height() as f64,
            )
        })
        .unwrap_or(true)
}

/// Whether the history pane is scrolled to (or within [`NEAR_BOTTOM_PX`] of)
/// its end.
fn is_near_bottom(scroll_top: f64, scroll_height: f64, client_height: f64) -> bool {
    scroll_height - scroll_top - client_height <= NEAR_BOTTOM_PX
}

/// What the history pane shows, for telling that it changed. The newest
/// message's identity is part of it because a full window drops the oldest
/// message as a new one arrives, leaving the row count unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
struct HistoryMark {
    rows: usize,
    newest: Option<String>,
    queued: usize,
}

/// Whether a change to the history should scroll to its end. Always on first
/// render (the pane opens at the newest message); when the visitor's own
/// waiting message has just appeared; otherwise only if the reader is already
/// at the end, so someone reading older messages is not moved by a new one.
/// `previous_queued` is the waiting-message count at the last change, `None`
/// before the first.
fn follow_to_end(previous_queued: Option<usize>, queued: usize, near_bottom: bool) -> bool {
    match previous_queued {
        None => true,
        Some(before) => queued > before || near_bottom,
    }
}

/// The line shown while the work for a join is running. A re-admitted member
/// keeps the re-join wording; a visitor's line sits under the queued message
/// and uses plain words. The percentage is always present and padded, so the
/// line does not change length when the first progress arrives and move Cancel
/// under a pointer that is already on its way down.
fn solving_line(phase: &JoinPhase, raised: bool, readmit: bool) -> Option<String> {
    let JoinPhase::Solving { tried, expected } = phase else {
        return None;
    };
    let pct = if *expected > 0.0 {
        ((*tried as f64 / expected) * 100.0).min(99.0) as u32
    } else {
        0
    };
    Some(match (readmit, raised) {
        (true, true) => {
            "Room security was raised; confirming you're human. You can keep writing.".to_string()
        }
        (true, false) => format!(
            "Joining: proving you are not a bot ({pct:\u{a0}>3}%). You can keep writing. Keep this tab open: a phone may pause a hidden tab."
        ),
        (false, true) => format!(
            "The room asked for a longer wait while you were queued ({pct:\u{a0}>3}%). Keep this tab open: a phone may pause a hidden tab."
        ),
        (false, false) => format!(
            "{pct:\u{a0}>3}% done. Keep this tab open: a phone may pause a hidden tab."
        ),
    })
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
    // per composer, at the first message. The message waits (`hold_send`) for
    // the answer while the proof runs on behind the prompt, so nothing is
    // posted under the default name before the visitor has answered.
    let mut nick_prompt = use_signal(|| None::<String>);
    let mut nick_asked = use_signal(|| false);
    let mut hold_send = use_signal(|| false);
    // Focus goes back to the composer when the prompt closes.
    let mut composer_el = use_signal(|| None::<std::rc::Rc<MountedData>>);

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
    let mut send = move || {
        // The prompt is open: this Enter came before focus reached it. The
        // text waits for the answer.
        if nick_prompt.peek().is_some() {
            return;
        }
        if draft.peek().trim().is_empty() {
            return;
        }
        if !readmit && !*nick_asked.peek() {
            // A failed or cancelled start leaves no key; make one so the
            // prompt can be prefilled.
            if public_join::default_nickname().is_none() {
                public_join::ensure_started(owner_vk);
            }
            if let Some(default) = public_join::default_nickname() {
                nick_asked.set(true);
                nick_prompt.set(Some(default));
                // The proof keeps running behind the prompt; the text is not
                // queued until it is answered, so a proof that finishes first
                // cannot post under the default name.
                hold_send.set(true);
                return;
            }
        }
        do_send();
    };
    let on_nickname = move |answer: NameAnswer| {
        nick_prompt.set(None);
        if let Some(el) = composer_el.peek().clone() {
            crate::util::safe_spawn_local(async move {
                let _ = el.set_focus(true).await;
            });
        }
        match answer {
            NameAnswer::Cancel => {
                // Nothing was sent: the text is still in the box, and the next
                // Send asks again. The proof may keep running.
                hold_send.set(false);
                nick_asked.set(false);
            }
            NameAnswer::Send(name) => {
                public_join::set_nickname(name);
                if *hold_send.peek() {
                    hold_send.set(false);
                    do_send();
                }
            }
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
    // The line under the composer. A visitor's progress sits under the
    // pending message; here they get only the notice and Cancel. A
    // re-admitted member keeps the wording of the re-join.
    let (status_testid, status) = if readmit {
        match &phase {
            JoinPhase::Idle => ("viewed-room-join-idle", note.clone()),
            // The room raised what it asks of newcomers: say so, as an explanation.
            JoinPhase::Solving { .. } => (
                "viewed-room-join-progress",
                solving_line(&phase, raised, true).unwrap_or_default(),
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
                    onmounted: move |cx| composer_el.set(Some(cx.data())),
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
                    class: "mt-2 text-xs text-text-muted flex items-center gap-2 tabular-nums",
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
    fn a_full_window_that_drops_the_oldest_still_counts_as_changed() {
        let before = HistoryMark {
            rows: 100,
            newest: Some("m99".into()),
            queued: 0,
        };
        let after = HistoryMark {
            rows: 100,
            newest: Some("m100".into()),
            queued: 0,
        };
        assert_ne!(before, after, "same row count, new newest message");
        assert!(
            follow_to_end(Some(0), 0, true),
            "a reader at the end follows it"
        );
        assert!(
            !follow_to_end(Some(0), 0, false),
            "a reader scrolled up stays"
        );
    }

    #[test]
    fn tab_goes_field_continue_cancel_and_round() {
        assert_eq!(
            next_dialog_focus(NICKNAME_FIELD_ID, false, false),
            NICKNAME_CONTINUE_ID
        );
        assert_eq!(
            next_dialog_focus(NICKNAME_CONTINUE_ID, false, false),
            NICKNAME_CANCEL_ID
        );
        assert_eq!(
            next_dialog_focus(NICKNAME_CANCEL_ID, false, false),
            NICKNAME_FIELD_ID
        );
        assert_eq!(
            next_dialog_focus(NICKNAME_FIELD_ID, false, true),
            NICKNAME_CANCEL_ID
        );
        assert_eq!(
            next_dialog_focus(NICKNAME_FIELD_ID, true, false),
            NICKNAME_CANCEL_ID
        );
        assert_eq!(next_dialog_focus("", false, false), NICKNAME_CONTINUE_ID);
    }

    #[test]
    fn a_reader_scrolled_up_is_not_moved_by_a_new_message() {
        assert!(
            follow_to_end(None, 0, false),
            "the first render opens at the newest"
        );
        assert!(!follow_to_end(Some(0), 0, false));
        assert!(follow_to_end(Some(0), 0, true), "at the bottom it follows");
        assert!(
            follow_to_end(Some(0), 1, false),
            "the visitor's own waiting message is brought into view"
        );
        assert!(
            !follow_to_end(Some(1), 0, false),
            "a message leaving the queue does not move a reader"
        );
    }

    #[test]
    fn near_the_bottom_means_within_the_threshold() {
        assert!(is_near_bottom(900.0, 1500.0, 600.0));
        assert!(is_near_bottom(900.0 - NEAR_BOTTOM_PX, 1500.0, 600.0));
        assert!(!is_near_bottom(0.0, 1500.0, 600.0));
        assert!(
            is_near_bottom(0.0, 500.0, 600.0),
            "a short history is at its end"
        );
    }

    #[test]
    fn the_default_or_nothing_is_no_choice() {
        assert_eq!(name_choice("Vapor Daylight", "Vapor Daylight"), None);
        assert_eq!(name_choice("Vapor Daylight", "  "), None);
        assert_eq!(
            name_choice("Vapor Daylight", " Quinn "),
            Some("Quinn".into())
        );
    }

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
