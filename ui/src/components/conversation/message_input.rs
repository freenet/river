use dioxus::prelude::*;

use super::emoji_picker::EmojiPicker;

/// Message input component that owns its own state.
/// This isolates keystroke handling from the parent component,
/// preventing expensive re-renders of the message list on each keystroke.
#[component]
pub fn MessageInput(handle_send_message: EventHandler<String>) -> Element {
    // Own the message state locally - keystrokes only re-render this component
    let mut message_text = use_signal(|| String::new());
    let mut show_emoji_picker = use_signal(|| false);

    let mut send_message = move || {
        let text = message_text.peek().to_string();
        if !text.is_empty() {
            message_text.set(String::new());
            handle_send_message.call(text);
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
                div { class: "flex gap-3 items-center",
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
                        class: "flex-1 px-4 py-2.5 bg-surface border border-border rounded-xl text-text placeholder-text-muted focus:outline-none focus:ring-2 focus:ring-accent/50 focus:border-accent transition-colors resize-none min-h-[44px] max-h-[200px]",
                        placeholder: "Type your message...",
                        value: "{message_text}",
                        rows: "1",
                        oninput: move |evt| message_text.set(evt.value().to_string()),
                        onkeydown: move |evt| {
                            // Enter without Shift sends the message
                            // Shift+Enter creates a new line (default textarea behavior)
                            if evt.key() == Key::Enter && !evt.modifiers().shift() {
                                evt.prevent_default();
                                send_message();
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
