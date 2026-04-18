use dioxus::prelude::*;
use wasm_bindgen::JsCast;

use super::emoji_picker::EmojiPicker;
use super::ReplyContext;

fn auto_resize_message_input() {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Some(doc) = window.document() else {
        return;
    };
    let Some(el) = doc.get_element_by_id("message-input") else {
        return;
    };
    let Ok(el) = el.dyn_into::<web_sys::HtmlElement>() else {
        return;
    };
    // Reset height to auto to measure scrollHeight correctly, then clamp to ~7 lines.
    el.style().set_property("height", "auto").ok();
    let new_height = el.scroll_height().min(168);
    el.style()
        .set_property("height", &format!("{}px", new_height))
        .ok();
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
    max_message_size: usize,
) -> Element {
    // Own the message state locally - keystrokes only re-render this component
    let mut message_text = use_signal(String::new);
    let mut show_emoji_picker = use_signal(|| false);

    let mut send_message = move || {
        let text = message_text.peek().to_string();
        if !text.is_empty() && text.len() <= max_message_size {
            let reply_ctx = replying_to.peek().clone();
            message_text.set(String::new());
            replying_to.set(None);
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
                    // Textarea with optional byte counter
                    div { class: "flex-1 flex flex-col",
                        textarea {
                            id: "message-input",
                            class: "w-full px-4 py-2.5 bg-surface border border-border rounded-xl text-text placeholder-text-muted focus:outline-none focus:ring-2 focus:ring-accent/50 focus:border-accent transition-colors resize-none min-h-[44px] overflow-y-auto",
                            style: "max-height: 168px;",
                            placeholder: "Type your message...",
                            value: "{message_text}",
                            rows: "1",
                            oninput: move |evt| {
                                message_text.set(evt.value().to_string());
                                auto_resize_message_input();
                            },
                            onkeydown: move |evt| {
                                // Enter without Shift sends the message
                                // Shift+Enter creates a new line (default textarea behavior)
                                if evt.key() == Key::Enter && !evt.modifiers().shift() {
                                    evt.prevent_default();
                                    send_message();
                                }
                                // Up arrow in empty input: edit last sent message
                                if evt.key() == Key::ArrowUp && message_text.peek().is_empty() {
                                    evt.prevent_default();
                                    on_request_edit_last.call(());
                                }
                            }
                        }
                        // Byte counter — shown when approaching the limit (>80%)
                        {
                            let text_bytes = message_text.read().len();
                            let threshold = max_message_size * 4 / 5; // 80%
                            if text_bytes > threshold {
                                let over = text_bytes > max_message_size;
                                if over {
                                    rsx! {
                                        div { class: "text-xs text-right mt-1 pr-1 text-red-600 dark:text-red-400 font-medium",
                                            "Message too long \u{2014} {text_bytes}/{max_message_size} bytes"
                                        }
                                    }
                                } else {
                                    rsx! {
                                        div { class: "text-xs text-right mt-1 pr-1 text-text-muted",
                                            "{text_bytes}/{max_message_size}"
                                        }
                                    }
                                }
                            } else {
                                rsx! {}
                            }
                        }
                    }
                    {
                        let text_bytes = message_text.read().len();
                        let over_limit = text_bytes > max_message_size;
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
