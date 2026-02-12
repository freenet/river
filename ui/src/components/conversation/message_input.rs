use dioxus::prelude::*;
use wasm_bindgen::JsCast;

use super::emoji_picker::EmojiPicker;
use super::ReplyContext;

/// Message input component that owns its own state.
/// This isolates keystroke handling from the parent component,
/// preventing expensive re-renders of the message list on each keystroke.
#[component]
pub fn MessageInput(
    handle_send_message: EventHandler<(String, Option<ReplyContext>)>,
    replying_to: Signal<Option<ReplyContext>>,
    on_request_edit_last: EventHandler<()>,
) -> Element {
    // Own the message state locally - keystrokes only re-render this component
    let mut message_text = use_signal(|| String::new());
    let mut show_emoji_picker = use_signal(|| false);

    let auto_resize = move || {
        if let Some(window) = web_sys::window() {
            if let Some(doc) = window.document() {
                if let Some(el) = doc.get_element_by_id("message-input") {
                    if let Ok(el) = el.dyn_into::<web_sys::HtmlElement>() {
                        // Reset height to auto to measure scrollHeight correctly
                        el.style().set_property("height", "auto").ok();
                        let scroll_height = el.scroll_height();
                        // Clamp to max ~7 lines (approx 168px at 14px font + padding)
                        let max_height = 168;
                        let new_height = scroll_height.min(max_height);
                        el.style()
                            .set_property("height", &format!("{}px", new_height))
                            .ok();
                    }
                }
            }
        }
    };

    let mut send_message = move || {
        let text = message_text.peek().to_string();
        if !text.is_empty() {
            let reply_ctx = replying_to.peek().clone();
            message_text.set(String::new());
            replying_to.set(None);
            handle_send_message.call((text, reply_ctx));
            // Reset textarea height after sending
            auto_resize();
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
                div { class: "flex gap-3 items-end",
                    // Emoji picker button and popup
                    div { class: "relative self-center",
                        button {
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
                    textarea {
                        id: "message-input",
                        class: "flex-1 px-4 py-2.5 bg-surface border border-border rounded-xl text-text placeholder-text-muted focus:outline-none focus:ring-2 focus:ring-accent/50 focus:border-accent transition-colors resize-none min-h-[44px] overflow-y-auto",
                        style: "max-height: 168px;",
                        placeholder: "Type your message...",
                        value: "{message_text}",
                        rows: "1",
                        oninput: move |evt| {
                            message_text.set(evt.value().to_string());
                            auto_resize();
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
                    button {
                        class: "px-5 py-2.5 bg-accent hover:bg-accent-hover text-white font-medium rounded-xl transition-colors disabled:opacity-50 disabled:cursor-not-allowed",
                        onclick: move |_| {
                            send_message();
                        },
                        "Send"
                    }
                }
            }
        }
    }
}
