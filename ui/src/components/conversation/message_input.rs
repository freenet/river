use crate::components::icons::{
    line_icon, REACT_ICON_PATH, REPLY_TARGET_ICON_PATH, SEND_ICON_PATH,
};
use dioxus::prelude::*;
use wasm_bindgen::JsCast;

use super::emoji_picker::EmojiPicker;
use super::mention::{
    apply_mention_selection, handle_mention_keydown, update_mention_from_input,
    MentionAutocomplete, MentionDropdown, MentionText,
};
use super::outbox::{PendingRow, OUTBOX};
use super::outbox_commit;
use super::{queue_message, value_for_room, ReplyContext};
use ed25519_dalek::VerifyingKey;
use river_core::room_state::member::MemberId;
use river_core::room_state::message::RoomMessageBody;
use std::collections::HashMap;

/// Encoded `content_len()` the draft will have when sent — the measure the
/// contract enforces (`max_message_size` bounds encoded bytes, not typed
/// text). Mirrors `queue_message`'s body construction: replies embed
/// the quoted author + preview, private rooms add the AES-GCM tag. Gating on
/// `text.len()` instead silently lost messages in the encoding-overhead gap
/// (the contract prunes them after the draft is already cleared).
fn measure_draft(text: &str, reply: Option<&ReplyContext>, is_private: bool) -> usize {
    match reply {
        Some(r) => RoomMessageBody::measure_reply(
            text,
            r.message_id.clone(),
            &r.author_name,
            &r.content_preview,
            is_private,
        ),
        None => RoomMessageBody::measure_text(text, is_private),
    }
}

/// Size the composer to its content on engines without `field-sizing: content`
/// (Firefox ESR 140 / Tor Browser, Safari < 26.2). CSS `max-height` is the cap.
///
/// This runs on every keystroke, so the DOM access pattern is the whole cost.
/// Collapsing to `height: auto` before every measurement — the obvious
/// implementation — puts a style write immediately before a `scrollHeight`
/// read, which forces a synchronous layout, and then writes again; measured at
/// ~3 layouts per keystroke and ~6.6 ms of layout + style per keystroke under a
/// 6x CPU throttle (freenet/river#468). The measuring read is unavoidable. Both
/// writes usually are not:
///
/// `scrollHeight` reports `max(content height, clientHeight)`, so it is exactly
/// the content height while the content OVERFLOWS the box, and saturates at the
/// box height once it fits. We set `height` to the content's padding-box height
/// while `box-sizing: border-box` counts the textarea's borders inside that
/// number, so a correctly-sized composer sits its border-width short of its own
/// content and keeps reporting `scrollHeight > clientHeight`. Typing therefore
/// measures in place: no collapse, and no write at all while the height is
/// unchanged.
///
/// `scrollHeight == clientHeight` means the content stopped overflowing, i.e.
/// it shrank below the box. That is the one case where the measurement carries
/// no information and the collapse has to be paid for.
///
/// Note the asymmetry that makes the fast path safe: it is taken only when the
/// content overflows, i.e. only when the box is too SHORT, so it can never
/// leave a stale too-tall composer behind.
///
/// # This is a trade, not a free win
///
/// When the fast path applies the cost is one forced layout and no writes; when
/// it does not, the probing read is wasted and the collapse forces a SECOND
/// layout, which is worse than collapsing unconditionally. So the fast path has
/// to be the common case, and that depends on the textarea's CSS, not on this
/// function. Four properties are load-bearing:
///
/// - `box-sizing: border-box` and a non-zero `border`, which together create the
///   slack that makes a content-sized box report `scrollHeight > clientHeight`;
/// - `min-height` and `line-height`, which must not conflict — one line of text
///   plus the vertical padding has to EXCEED `min-h-[44px]`, or min-height owns
///   the box, the two metrics come out equal, and the fast path never applies.
///
/// The `min-height` half is not hypothetical. `tailwind.css` clamps the body
/// font to 13px below 768px; at the default `line-height: 1.5` that is a 19.5px
/// line box, so a phone-width composer measured 41.5px natural against a 44px
/// minimum, and every keystroke on a phone paid two forced layouts instead of
/// one — the opposite of this function's purpose, on the weakest hardware. The
/// explicit `leading-6` on the textarea is what keeps one line above the
/// minimum at every body font size. Do not remove it as "cosmetic".
///
/// Pinned by the Playwright test `Composer auto-resize cost (#468, JS
/// fallback)`, which runs at BOTH a desktop and a phone viewport and asserts
/// `scrollHeight > clientHeight` before relying on it — pinning one viewport
/// is what let the mobile regression through, since `test.use({viewport})`
/// overrides the project's device viewport and so varies the engine without
/// varying the layout regime.
fn auto_resize_message_input() {
    let Some(el) = get_message_textarea() else {
        return;
    };
    if sizes_itself(&el) {
        return;
    }
    let style = el.style();

    // The reads come off one layout; no write separates them.
    let scroll_height = el.scroll_height();
    let cap = max_height_px(&el);
    let content_height = if scroll_height > el.client_height() {
        scroll_height
    } else {
        style.set_property("height", "auto").ok();
        el.scroll_height()
    };

    let target = format!("{content_height}px");
    // Skip a no-op write: its style invalidation is the bulk of the
    // per-keystroke cost. After a collapse the inline value is `auto`, so that
    // branch always writes.
    if style.get_property_value("height").unwrap_or_default() != target {
        style.set_property("height", &target).ok();
    }

    // Below the cap, `hidden` removes the border slack's ~2px scroll range.
    let overflow = if cap.is_some_and(|max| f64::from(content_height) >= max) {
        "auto"
    } else {
        "hidden"
    };
    if style.get_property_value("overflow-y").unwrap_or_default() != overflow {
        style.set_property("overflow-y", overflow).ok();
    }
}

/// Computed `field-sizing == "content"`, cached on the first call. Computed style,
/// not `CSS.supports`, so a stylesheet (the #468 test's) can force the fallback.
fn sizes_itself(el: &web_sys::HtmlTextAreaElement) -> bool {
    thread_local! {
        static NATIVE: std::cell::OnceCell<bool> = const { std::cell::OnceCell::new() };
    }
    NATIVE.with(|native| {
        *native.get_or_init(|| computed_style(el, "field-sizing").is_some_and(|v| v == "content"))
    })
}

/// The composer's resolved `max-height` in px, or `None` for `none`.
fn max_height_px(el: &web_sys::HtmlTextAreaElement) -> Option<f64> {
    computed_style(el, "max-height")?
        .strip_suffix("px")?
        .parse()
        .ok()
}

/// One computed-style property of `el`, or `None` if it can't be read.
fn computed_style(el: &web_sys::Element, prop: &str) -> Option<String> {
    web_sys::window()?
        .get_computed_style(el)
        .ok()??
        .get_property_value(prop)
        .ok()
}

fn get_message_textarea() -> Option<web_sys::HtmlTextAreaElement> {
    web_sys::window()?
        .document()?
        .get_element_by_id("message-input")?
        .dyn_into::<web_sys::HtmlTextAreaElement>()
        .ok()
}

/// A room and the identity it is written as: whose draft it is.
type DraftKey = (VerifyingKey, VerifyingKey);

/// The composer's drafts, one per room and identity. Held here rather than in
/// `MessageInput`, which is unmounted while the open room shows a notice
/// instead (syncing, or a private room's key not here yet): a draft typed in
/// another room must outlive that, and still hold a reload
/// (`has_unsent_draft`). Only the composer subscribes.
static DRAFTS: GlobalSignal<HashMap<DraftKey, String>> = Signal::global(HashMap::new);

/// The open room's draft. Text typed (or a failed send taken back) in a
/// room stays in that room.
#[derive(Clone, Copy, PartialEq)]
struct RoomDraft {
    key: DraftKey,
}

impl RoomDraft {
    /// The open room's draft. Subscribes the caller.
    fn text(&self) -> String {
        DRAFTS.read().get(&self.key).cloned().unwrap_or_default()
    }

    fn peek(&self) -> String {
        DRAFTS.peek().get(&self.key).cloned().unwrap_or_default()
    }

    /// An emptied draft leaves the map, so it holds only real drafts.
    ///
    /// Written synchronously, from `oninput` too: deferring would lag the
    /// textarea and drop keystrokes (`dioxus-signal-safety.md`, "Bind
    /// `oninput`"). Only the composer's render subscribes to `DRAFTS`.
    fn set(&mut self, text: String) {
        if text.is_empty() {
            if DRAFTS.peek().contains_key(&self.key) {
                DRAFTS.write().remove(&self.key);
            }
        } else {
            DRAFTS.write().insert(self.key, text);
        }
    }
}

/// Whether a draft waits that a reload would lose: one with text, in a room
/// still held as the identity that wrote it. A draft whose room is gone does
/// not count, or it would hold a reload back for the life of the page.
/// Unreadable means "cannot tell": busy, as `user_is_busy` treats it.
#[cfg(target_arch = "wasm32")]
pub(crate) fn has_unsent_draft() -> bool {
    let Ok(drafts) = DRAFTS.try_peek() else {
        return true;
    };
    if drafts.is_empty() {
        return false;
    }
    let Ok(rooms) = crate::components::app::ROOMS.try_peek() else {
        return true;
    };
    any_held_draft(&drafts, |(room, author)| {
        rooms
            .map
            .get(room)
            .is_some_and(|r| r.self_verifying_key() == Some(*author))
    })
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
fn any_held_draft(drafts: &HashMap<DraftKey, String>, held: impl Fn(&DraftKey) -> bool) -> bool {
    drafts
        .iter()
        .any(|(key, text)| !text.trim().is_empty() && held(key))
}

impl MentionText for RoomDraft {
    fn current(&self) -> String {
        self.peek()
    }

    fn replace(&mut self, value: String) {
        self.set(value);
    }
}

/// What joining a failed send's text back into its room's draft gives: the
/// failed text alone when the draft is empty, or both, the failed text
/// first, so nothing typed there meanwhile is lost.
fn draft_with_failed(draft: &str, failed: &str) -> String {
    if draft.trim().is_empty() || draft == failed {
        failed.to_string()
    } else {
        format!("{failed}\n\n{draft}")
    }
}

/// Clear the send error if it is `room`'s: typing or sending in another room
/// leaves a failed send's reason beside its draft.
fn clear_error_of(mut send_error: Signal<Option<(VerifyingKey, String)>>, room: &VerifyingKey) {
    if send_error.peek().as_ref().is_some_and(|(r, _)| r == room) {
        send_error.set(None);
    }
}

/// Message input component that owns its own state.
/// This isolates keystroke handling from the parent component,
/// preventing expensive re-renders of the message list on each keystroke.
///
/// # Sending
///
/// Enter and Send go through one guard (`send_message`), then `queue_message`.
/// An accepted send enters the outbox (`outbox.rs`), and while it is signed
/// and admitted the composer shows its text and reply target from there,
/// read-only, with Send disabled; further Enters and clicks do nothing. Admitted, it leaves the
/// outbox, the message shows once in the history, and the composer is empty
/// and ready for the next. If it fails (signing, or the room refuses it), the
/// composer takes the text and reply target back as its editable draft, with
/// the reason, and Send sends it again as a new message through the same
/// checks. A send belongs to the room and identity it was written in: another
/// room's composer never shows it, and its outcome never clears another
/// room's draft (`room`, `author`). So does a draft (`RoomDraft`) and a reply
/// target (`value_for_room`): neither shows, or is sent, in another room.
#[component]
pub fn MessageInput(
    /// The open room and the identity it sends as.
    room: VerifyingKey,
    author: VerifyingKey,
    /// The reply target, with the room it was started in. Only `room`'s is
    /// shown or sent here.
    replying_to: Signal<Option<(VerifyingKey, ReplyContext)>>,
    on_request_edit_last: EventHandler<()>,
    /// Maximum message content size in bytes (from room configuration).
    /// Bounds the ENCODED content (`RoomMessageBody::content_len`), which is
    /// what the contract validates — not the raw typed text.
    max_message_size: usize,
    /// Whether the room is private (encrypted): private bodies carry the
    /// AES-GCM tag, which counts toward the encoded size.
    is_private: bool,
    /// Mentionable members (id, current nickname), excluding self, sorted by
    /// name. Drives the `@` autocomplete. Changes only when membership changes,
    /// so it does not affect keystroke-level re-rendering.
    members: Vec<(MemberId, String)>,
) -> Element {
    // Keystrokes only re-render this component: only it reads `DRAFTS`.
    let mut draft = RoomDraft {
        key: (room, author),
    };
    let mut show_emoji_picker = use_signal(|| false);
    let mut mention = use_signal(|| None as Option<MentionAutocomplete>);
    // Why the last send in `room` did not go: a refusal, or a failure the
    // draft was taken back from. Cleared by typing and by the next send,
    // in that room.
    let mut send_error = use_signal(|| None as Option<(VerifyingKey, String)>);
    // Set from an accepted send until the outbox holds it (the defer queued
    // after `queue_message`'s): a second Enter or click before then,
    // even before the next render, does nothing.
    let mut queueing = use_hook(|| CopyValue::new(false));
    // The failed send last taken back, so a render before its discard lands
    // cannot take it twice.
    let mut taken_back = use_hook(|| CopyValue::new(None::<u64>));
    // The outbox as the last uncontended read saw it, for this room and
    // identity only (freenet/river#555).
    let mut last_good =
        use_hook(|| CopyValue::new(None::<((VerifyingKey, VerifyingKey), Option<PendingRow>)>));

    crate::util::signal_guard::anchor();
    let composer_send: Option<PendingRow> = match OUTBOX.try_read() {
        Ok(outbox) => {
            let row = outbox.composer_send(&room, &author);
            last_good.set(Some(((room, author), row.clone())));
            row
        }
        Err(_) => {
            crate::util::signal_guard::schedule_nudge();
            value_for_room(&last_good.peek(), &(room, author)).flatten()
        }
    };
    // Being signed or admitted: shown read-only until it settles.
    let sending: Option<PendingRow> = composer_send.filter(|row| row.failed.is_none());
    let locked = sending.is_some() || *queueing.peek();

    // The box swaps between the draft and the send it shows: size it again
    // once that render is in the DOM (engines without `field-sizing`).
    use_effect(use_reactive!(|(locked, room, author)| {
        let _ = (locked, room, author);
        crate::util::defer(auto_resize_message_input);
    }));

    // A failed send comes back as the draft, with its reply target and why.
    use_effect(use_reactive!(|(room, author)| {
        crate::util::signal_guard::anchor();
        let failed = match OUTBOX.try_read() {
            Ok(outbox) => outbox
                .composer_send(&room, &author)
                .filter(|row| row.failed.is_some()),
            Err(_) => {
                crate::util::signal_guard::schedule_nudge();
                return;
            }
        };
        let Some(row) = failed else {
            return;
        };
        if *taken_back.peek() == Some(row.id) {
            return;
        }
        taken_back.set(Some(row.id));
        // `draft` is rebuilt here: this closure is the first render's, and
        // the room is this run's.
        let mut draft = RoomDraft {
            key: (room, author),
        };
        draft.set(draft_with_failed(&draft.peek(), &row.text));
        replying_to.set(row.reply.clone().map(|reply| (room, reply)));
        send_error.set(row.failed.clone().map(|reason| (room, reason)));
        outbox_commit::discard(room, row.id);
        crate::util::defer(auto_resize_message_input);
    }));

    let mut send_message = move || {
        // Read now, not as the last render saw it: two activations can land
        // before the next render.
        let busy = *queueing.peek()
            || OUTBOX.try_peek().map_or(true, |o| {
                o.composer_send(&room, &author)
                    .is_some_and(|row| row.failed.is_none())
            });
        if busy {
            return;
        }
        let text = draft.peek();
        let reply_ctx = value_for_room(&replying_to.peek(), &room);
        let within_limit = measure_draft(&text, reply_ctx.as_ref(), is_private) <= max_message_size;
        if text.is_empty() || !within_limit {
            return;
        }
        match queue_message(room, text, reply_ctx) {
            Ok(()) => {
                queueing.set(true);
                clear_error_of(send_error, &room);
                mention.set(None);
                // Queued after the outbox insert, so the composer shows the
                // send from the outbox before its draft is emptied.
                crate::util::defer(move || {
                    queueing.set(false);
                    draft.set(String::new());
                    // Only the reply that was sent: one started in another
                    // room since is that room's.
                    if value_for_room(&replying_to.peek(), &room).is_some() {
                        replying_to.set(None);
                    }
                });
            }
            Err(reason) => send_error.set(Some((room, reason))),
        }
    };

    // Handle emoji selection - insert at end of message
    let handle_emoji_select = move |emoji: String| {
        let current = draft.peek();
        draft.set(format!("{}{}", current, emoji));
    };

    // What the box shows: the send being signed, or the draft.
    let shown_text = match &sending {
        Some(row) => row.text.clone(),
        None => draft.text(),
    };
    let shown_reply = match &sending {
        Some(row) => row.reply.clone(),
        None => value_for_room(&replying_to.read(), &room),
    };
    let shown_error = send_error
        .read()
        .as_ref()
        .filter(|(r, _)| *r == room && sending.is_none())
        .map(|(_, reason)| reason.clone());

    // Once per render, for the counter and Send; `send_message` re-measures on click.
    let encoded_bytes = measure_draft(&shown_text, shown_reply.as_ref(), is_private);
    let over_limit = encoded_bytes > max_message_size;

    rsx! {
        // Backdrop for emoji picker - outside the message bar to avoid z-index issues
        if show_emoji_picker() {
            div {
                class: "fixed inset-0 z-40",
                onclick: move |_| crate::util::defer(move || show_emoji_picker.set(false)),
            }
        }
        // Backdrop for the @mention dropdown: a click anywhere outside the
        // message bar dismisses it (the bar sits at z-50, above this z-40
        // layer, so clicks inside the textarea/buttons are unaffected).
        if mention.read().is_some() {
            div {
                class: "fixed inset-0 z-40",
                // Defer the signal mutation off the event tick per the Dioxus
                // WASM signal-safety rule (.claude/rules/dioxus-signal-safety.md).
                onclick: move |_| crate::util::defer(move || mention.set(None)),
            }
        }
        div { class: "flex-shrink-0 bg-panel relative z-50",
            "data-testid": "message-composer",
            // Spans the conversation column (`.conversation-column`, centred
            // on a wide window), on the header's left edge otherwise.
            div { class: "px-3 md:px-5 pt-1 pb-3 md:pb-[18px] conversation-column",
                // Reply preview strip
                {
                    if let Some(ctx) = shown_reply.as_ref() {
                        let reply_author = ctx.author_name.clone();
                        let preview = ctx.content_preview.clone();
                        rsx! {
                            div {
                                "data-testid": "composer-reply-preview",
                                class: "flex items-center gap-2 mb-1.5 px-1 text-[13px] text-text-quote",
                                span { "aria-hidden": "true", class: "flex-shrink-0 inline-flex", {line_icon(REPLY_TARGET_ICON_PATH, 14)} }
                                span { class: "flex-1 truncate",
                                    span { class: "font-bold text-text-secondary", "Replying to {reply_author} " }
                                    "{preview}"
                                }
                                // The reply target goes with the send while it
                                // is being signed.
                                if !locked {
                                    button {
                                        "data-testid": "composer-reply-cancel",
                                        class: "text-text-muted hover:text-text transition-colors flex-shrink-0",
                                        title: "Cancel reply",
                                        onclick: move |_| replying_to.set(None),
                                        "\u{00d7}"
                                    }
                                }
                            }
                        }
                    } else {
                        rsx! {}
                    }
                }
                // One rounded box: emoji, the draft, Send. The box takes the
                // focus ring, so the textarea inside needs none of its own.
                form {
                    "data-testid": "message-composer-box",
                    // A send being signed: the box shows it read-only.
                    "data-sending": if locked { "true" } else { "false" },
                    class: "flex items-end gap-0.5 px-1.5 bg-surface border border-border rounded-xl focus-within:border-text-muted transition-colors",
                    onsubmit: move |evt| {
                        evt.prevent_default();
                        send_message();
                    },
                    // Emoji picker button and popup
                    div { class: "relative flex-shrink-0 mb-1",
                        button {
                            r#type: "button",
                            "data-testid": "composer-emoji-button",
                            class: "inline-flex items-center justify-center w-9 h-9 rounded-lg text-text-muted hover:text-text hover:bg-surface-hover transition-colors disabled:opacity-50 disabled:cursor-default",
                            disabled: locked,
                            title: "Insert emoji",
                            "aria-label": "Insert emoji",
                            onclick: move |_| {
                                crate::util::defer(move || {
                                    let open = *show_emoji_picker.peek();
                                    show_emoji_picker.set(!open);
                                })
                            },
                            {line_icon(REACT_ICON_PATH, 20)}
                        }
                        // Emoji picker popup (appears above the button)
                        if show_emoji_picker() {
                            div {
                                class: "absolute bottom-full left-0 mb-2",
                                EmojiPicker {
                                    on_select: handle_emoji_select,
                                    on_close: move |_| show_emoji_picker.set(false),
                                }
                            }
                        }
                    }
                    // Textarea with optional byte counter. `relative` anchors the
                    // @mention autocomplete dropdown.
                    div { class: "flex-1 flex flex-col relative",
                        // @mention autocomplete dropdown (floats above the textarea)
                        MentionDropdown {
                            mention,
                            on_pick: move |i| apply_mention_selection(
                                "message-input".to_string(),
                                draft,
                                mention,
                                i,
                                auto_resize_message_input,
                            ),
                        }
                        textarea {
                            id: "message-input",
                            "data-testid": "message-input",
                            // `field-sizing-content` grows the box natively;
                            // `max-h-[50dvh]` caps it, or a long draft scrolls the
                            // whole app shell, header and Send with it. Phones cap
                            // at `30dvh`: `dvh` ignores the on-screen keyboard, so
                            // half of it would leave almost no history visible
                            // above the keyboard while a long draft is typed. The
                            // 50dvh cap needs height as well as width, or a
                            // landscape phone (844x390) passes a width-only `md:`.
                            // `leading-6` is load-bearing on the JS fallback, not
                            // cosmetic: see `auto_resize_message_input`. It pins the
                            // line box to 24px so one line of text plus `py-2.5`
                            // always exceeds `min-h-[44px]`, at every body font size.
                            // The border is transparent (the box around the
                            // textarea draws the visible one) but must stay: see
                            // `auto_resize_message_input` for the slack it makes.
                            // Without it the mobile font clamp (`tailwind.css`: 13px
                            // below 768px) leaves the natural height under the
                            // minimum, min-height takes over the box, and the
                            // in-place measurement stops working — costing an EXTRA
                            // forced layout per keystroke instead of saving one.
                            class: "w-full px-2 py-2.5 leading-6 text-[15px] bg-transparent border border-transparent text-text placeholder-text-muted focus:outline-hidden resize-none min-h-[44px] field-sizing-content max-h-[30dvh] [@media(min-width:48rem)_and_(min-height:40rem)]:max-h-[50dvh] overflow-y-auto",
                            placeholder: "Type your message...",
                            value: "{shown_text}",
                            // The send being signed is shown, not edited.
                            readonly: locked,
                            "aria-readonly": if locked { "true" } else { "false" },
                            rows: "1",
                            oninput: move |evt| {
                                let value = evt.value().to_string();
                                clear_error_of(send_error, &room);
                                draft.set(value.clone());
                                auto_resize_message_input();
                                // Detect / update the @mention autocomplete.
                                update_mention_from_input("message-input", &value, &members, mention);
                            },
                            onkeydown: move |evt| {
                                let key = evt.key();
                                // @mention navigation takes precedence while the dropdown is open.
                                if handle_mention_keydown(
                                    "message-input",
                                    &evt,
                                    draft,
                                    mention,
                                    auto_resize_message_input,
                                ) {
                                    return;
                                }
                                // Enter without Shift sends the message
                                // Shift+Enter creates a new line (default textarea behavior)
                                if key == Key::Enter && !evt.modifiers().shift() {
                                    evt.prevent_default();
                                    send_message();
                                }
                                // Up arrow in empty input: edit last sent message
                                if key == Key::ArrowUp && draft.peek().is_empty() && !locked {
                                    evt.prevent_default();
                                    on_request_edit_last.call(());
                                }
                            }
                        }
                        // Byte counter — shown when approaching the limit (>80%).
                        // Counts ENCODED bytes (what the contract enforces), so
                        // it stays truthful for replies, private rooms, and
                        // multi-byte characters.
                        {
                            // `/ 5 * 4` (not `* 4 / 5`): max_message_size is
                            // room-config-controlled, so multiply-first can
                            // overflow on a hostile/corrupt config.
                            let threshold = max_message_size / 5 * 4; // 80%
                            if encoded_bytes > threshold {
                                if over_limit {
                                    rsx! {
                                        div { class: "text-xs text-right mt-1 pr-1 text-danger font-medium",
                                            "Message too long \u{2014} {encoded_bytes}/{max_message_size} bytes"
                                        }
                                    }
                                } else {
                                    rsx! {
                                        div { class: "text-xs text-right mt-1 pr-1 text-text-muted",
                                            "{encoded_bytes}/{max_message_size}"
                                        }
                                    }
                                }
                            } else {
                                rsx! {}
                            }
                        }
                    }
                    {
                        // An accent-coloured icon, no solid fill.
                        let btn_class = if over_limit || locked {
                            "flex-shrink-0 mb-1 inline-flex items-center justify-center w-9 h-9 rounded-lg text-text-muted opacity-50 cursor-not-allowed"
                        } else {
                            "flex-shrink-0 mb-1 inline-flex items-center justify-center w-9 h-9 rounded-lg text-link hover:bg-surface-hover transition-colors cursor-pointer"
                        };
                        let tooltip = if over_limit {
                            format!("Message exceeds the {} byte limit", max_message_size)
                        } else if locked {
                            "Sending".to_string()
                        } else {
                            "Send".to_string()
                        };
                        rsx! {
                            button {
                                r#type: "button",
                                "data-testid": "send-message-button",
                                class: "{btn_class}",
                                disabled: over_limit || locked,
                                title: "{tooltip}",
                                // Use explicit onclick instead of form submit — on iOS Safari,
                                // tapping a submit button while the keyboard is visible causes
                                // the keyboard to dismiss first, which triggers a viewport resize
                                // that cancels the click→submit event chain.
                                onclick: move |_| send_message(),
                                "aria-label": "Send",
                                {line_icon(SEND_ICON_PATH, 20)}
                            }
                        }
                    }
                }
                if let Some(reason) = shown_error {
                    div {
                        "data-testid": "composer-send-error",
                        role: "alert",
                        class: "mt-1 px-1 text-xs text-danger [overflow-wrap:anywhere]",
                        "Not sent: {reason}. Press Send to try again."
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use freenet_scaffold::util::FastHash;
    use river_core::room_state::message::MessageId;

    #[test]
    fn a_reply_is_held_for_the_room_it_was_started_in_only() {
        let key = |b: u8| ed25519_dalek::SigningKey::from_bytes(&[b; 32]).verifying_key();
        let held = Some((key(1), reply_ctx()));
        assert_eq!(value_for_room(&held, &key(1)), Some(reply_ctx()));
        assert_eq!(value_for_room(&held, &key(2)), None);
        assert_eq!(value_for_room::<_, ReplyContext>(&None, &key(1)), None);
    }

    #[test]
    fn only_a_held_rooms_real_draft_counts_as_waiting() {
        let key = |b: u8| ed25519_dalek::SigningKey::from_bytes(&[b; 32]).verifying_key();
        let (room, gone) = ((key(1), key(9)), (key(3), key(9)));
        let held = |k: &DraftKey| *k != gone;
        let mut drafts = HashMap::new();
        assert!(!any_held_draft(&drafts, held));
        drafts.insert(room, "  \n".to_string());
        assert!(!any_held_draft(&drafts, held), "blank is no draft");
        drafts.insert(gone, "left behind".to_string());
        assert!(!any_held_draft(&drafts, held), "its room is gone");
        drafts.insert(room, "typed".to_string());
        assert!(any_held_draft(&drafts, held));
    }

    #[test]
    fn a_failed_send_joins_the_draft_without_losing_either() {
        assert_eq!(draft_with_failed("", "sent"), "sent");
        assert_eq!(draft_with_failed("  ", "sent"), "sent");
        assert_eq!(draft_with_failed("sent", "sent"), "sent");
        assert_eq!(
            draft_with_failed("typed since", "sent"),
            "sent\n\ntyped since"
        );
    }

    fn reply_ctx() -> ReplyContext {
        ReplyContext {
            message_id: MessageId(FastHash(42)),
            author_name: "Alice".to_string(),
            content_preview: "préview 🎉 of the quoted message".to_string(),
        }
    }

    /// Pins the gate's plumbing: `measure_draft` must select the reply
    /// measurement when a reply is active and thread `is_private` through —
    /// a revert to `text.len()` (the HostFat message-loss bug) or a dropped
    /// reply/private branch fails here without needing a browser.
    #[test]
    fn measure_draft_matches_room_message_body_measures() {
        let texts = ["", "short", &"a".repeat(998), &"é".repeat(499)];
        for text in texts {
            for is_private in [false, true] {
                assert_eq!(
                    measure_draft(text, None, is_private),
                    RoomMessageBody::measure_text(text, is_private),
                    "text branch: bytes={} private={}",
                    text.len(),
                    is_private
                );
                let r = reply_ctx();
                assert_eq!(
                    measure_draft(text, Some(&r), is_private),
                    RoomMessageBody::measure_reply(
                        text,
                        r.message_id.clone(),
                        &r.author_name,
                        &r.content_preview,
                        is_private
                    ),
                    "reply branch: bytes={} private={}",
                    text.len(),
                    is_private
                );
            }
        }
        // The branches must actually differ (reply embeds metadata) and
        // private must cost more (AES-GCM tag): guards against collapsing
        // the match into a single arm.
        assert!(measure_draft("hi", Some(&reply_ctx()), false) > measure_draft("hi", None, false));
        assert!(measure_draft("hi", None, true) > measure_draft("hi", None, false));
    }
}
