use dioxus::logger::tracing::*;
use dioxus::prelude::*;

/// Message input component that owns its own state.
/// This isolates keystroke handling from the parent component,
/// preventing expensive re-renders of the message list on each keystroke.
#[component]
pub fn MessageInput(handle_send_message: EventHandler<String>) -> Element {
    // Own the message state locally - keystrokes only re-render this component
    let mut message_text = use_signal(|| String::new());

    let mut send_message = move || {
        let text = message_text.peek().to_string();
        if !text.is_empty() {
            message_text.set(String::new());
            handle_send_message.call(text);
        }
    };

    rsx! {
        div { class: "flex-shrink-0 border-t border-border bg-panel",
            div { class: "max-w-4xl mx-auto px-4 py-3",
                div { class: "flex gap-3 items-end",
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
                            info!("Send button clicked");
                            send_message();
                        },
                        "Send"
                    }
                }
            }
        }
    }
}
