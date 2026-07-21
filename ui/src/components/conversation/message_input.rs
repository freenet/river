use dioxus::prelude::*;
use wasm_bindgen::JsCast;

use super::emoji_picker::EmojiPicker;
use super::mention::{
    apply_mention_selection, handle_mention_keydown, update_mention_from_input,
    MentionAutocomplete, MentionDropdown,
};
use super::ReplyContext;
use river_core::room_state::member::MemberId;
use river_core::room_state::message::RoomMessageBody;

/// Encoded `content_len()` the draft will have when sent — the measure the
/// contract enforces (`max_message_size` bounds encoded bytes, not typed
/// text). Mirrors `handle_send_message`'s body construction: replies embed
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

fn auto_resize_message_input() {
    let Some(el) = get_message_textarea() else {
        return;
    };
    // Reset height to auto to measure scrollHeight correctly, then clamp to ~7 lines.
    el.style().set_property("height", "auto").ok();
    let new_height = el.scroll_height().min(168);
    el.style()
        .set_property("height", &format!("{}px", new_height))
        .ok();
}

fn get_message_textarea() -> Option<web_sys::HtmlTextAreaElement> {
    web_sys::window()?
        .document()?
        .get_element_by_id("message-input")?
        .dyn_into::<web_sys::HtmlTextAreaElement>()
        .ok()
}

/// Message input component that owns its own state.
/// This isolates keystroke handling from the parent component,
/// preventing expensive re-renders of the message list on each keystroke.
#[component]
pub fn MessageInput(
    handle_send_message: EventHandler<(String, Option<ReplyContext>)>,
    replying_to: Signal<Option<ReplyContext>>,
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
    // Own the message state locally - keystrokes only re-render this component
    let mut message_text = use_signal(String::new);
    let mut show_emoji_picker = use_signal(|| false);
    let mut mention = use_signal(|| None as Option<MentionAutocomplete>);

    let mut send_message = move || {
        let text = message_text.peek().to_string();
        let within_limit =
            measure_draft(&text, replying_to.peek().as_ref(), is_private) <= max_message_size;
        if !text.is_empty() && within_limit {
            let reply_ctx = replying_to.peek().clone();
            message_text.set(String::new());
            replying_to.set(None);
            mention.set(None);
            handle_send_message.call((text, reply_ctx));
            // Defer resize so it runs after Dioxus flushes the cleared value to the DOM;
            // measuring scrollHeight synchronously here still sees the pre-send content.
            crate::util::defer(auto_resize_message_input);
        }
    };

    // Handle emoji selection - insert at end of message
    let handle_emoji_select = move |emoji: String| {
        let current = message_text.peek().to_string();
        message_text.set(format!("{}{}", current, emoji));
    };

    rsx! {
        // Backdrop for emoji picker - outside the message bar to avoid z-index issues
        if show_emoji_picker() {
            div {
                class: "fixed inset-0 z-40",
                onclick: move |_| show_emoji_picker.set(false),
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
        div { class: "flex-shrink-0 border-t border-border bg-panel relative z-50",
            div { class: "max-w-4xl mx-auto px-4 py-3",
                // Reply preview strip
                {
                    let reply = replying_to.read();
                    if let Some(ctx) = reply.as_ref() {
                        let author = ctx.author_name.clone();
                        let preview = ctx.content_preview.clone();
                        rsx! {
                            div { class: "flex items-center gap-2 mb-2 px-3 py-1.5 bg-surface border-l-2 border-accent rounded text-sm text-text-muted",
                                span { class: "flex-1 truncate",
                                    span { class: "font-medium", "\u{21a9} @{author}: " }
                                    "{preview}"
                                }
                                button {
                                    class: "text-text-muted hover:text-text transition-colors flex-shrink-0",
                                    title: "Cancel reply",
                                    onclick: move |_| replying_to.set(None),
                                    "\u{00d7}"
                                }
                            }
                        }
                    } else {
                        rsx! {}
                    }
                }
                form {
                    class: "flex gap-3 items-end",
                    onsubmit: move |evt| {
                        evt.prevent_default();
                        send_message();
                    },
                    // Emoji picker button and popup
                    div { class: "relative self-center",
                        button {
                            r#type: "button",
                            class: "p-2.5 rounded-xl hover:bg-surface transition-colors",
                            title: "Insert emoji",
                            onclick: move |_| show_emoji_picker.set(!show_emoji_picker()),
                            span {
                                class: "text-lg",
                                style: "filter: grayscale(100%); opacity: 0.6;",
                                "🙂"
                            }
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
                                message_text,
                                mention,
                                i,
                                auto_resize_message_input,
                            ),
                        }
                        textarea {
                            id: "message-input",
                            "data-testid": "message-input",
                            class: "w-full px-4 py-2.5 bg-surface border border-border rounded-xl text-text placeholder-text-muted focus:outline-none focus:ring-2 focus:ring-accent/50 focus:border-accent transition-colors resize-none min-h-[44px] overflow-y-auto",
                            style: "max-height: 168px;",
                            placeholder: "Type your message...",
                            value: "{message_text}",
                            rows: "1",
                            oninput: move |evt| {
                                let value = evt.value().to_string();
                                message_text.set(value.clone());
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
                                    message_text,
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
                                if key == Key::ArrowUp && message_text.peek().is_empty() {
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
                            let encoded_bytes = measure_draft(
                                &message_text.read(),
                                replying_to.read().as_ref(),
                                is_private,
                            );
                            // `/ 5 * 4` (not `* 4 / 5`): max_message_size is
                            // room-config-controlled, so multiply-first can
                            // overflow on a hostile/corrupt config.
                            let threshold = max_message_size / 5 * 4; // 80%
                            if encoded_bytes > threshold {
                                let over = encoded_bytes > max_message_size;
                                if over {
                                    rsx! {
                                        div { class: "text-xs text-right mt-1 pr-1 text-red-600 dark:text-red-400 font-medium",
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
                        let encoded_bytes = measure_draft(
                            &message_text.read(),
                            replying_to.read().as_ref(),
                            is_private,
                        );
                        let over_limit = encoded_bytes > max_message_size;
                        let btn_class = if over_limit {
                            "px-5 py-2.5 bg-gray-400 dark:bg-gray-600 text-white font-medium rounded-xl opacity-50 cursor-not-allowed"
                        } else {
                            "px-5 py-2.5 bg-accent hover:bg-accent-hover text-white font-medium rounded-xl transition-colors cursor-pointer"
                        };
                        let tooltip = if over_limit {
                            format!("Message exceeds the {} byte limit", max_message_size)
                        } else {
                            String::new()
                        };
                        rsx! {
                            button {
                                r#type: "button",
                                "data-testid": "send-message-button",
                                class: "{btn_class}",
                                disabled: over_limit,
                                title: "{tooltip}",
                                // Use explicit onclick instead of form submit — on iOS Safari,
                                // tapping a submit button while the keyboard is visible causes
                                // the keyboard to dismiss first, which triggers a viewport resize
                                // that cancels the click→submit event chain.
                                onclick: move |_| send_message(),
                                "Send"
                            }
                        }
                    }
                }
            }
        }
    }
}
