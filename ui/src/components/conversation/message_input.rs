use dioxus::logger::tracing::*;
use dioxus::prelude::*;

#[component]
pub fn MessageInput(new_message: Signal<String>, handle_send_message: EventHandler<()>) -> Element {
    rsx! {
        div { class: "flex-shrink-0 border-t border-border bg-panel",
            div { class: "max-w-4xl mx-auto px-4 py-3",
                div { class: "flex gap-3",
                    input {
                        class: "flex-1 px-4 py-2.5 bg-surface border border-border rounded-xl text-text placeholder-text-muted focus:outline-none focus:ring-2 focus:ring-accent/50 focus:border-accent transition-colors",
                        r#type: "text",
                        placeholder: "Type your message...",
                        value: "{new_message}",
                        oninput: move |evt| new_message.set(evt.value().to_string()),
                        onkeydown: move |evt| {
                            if evt.key() == Key::Enter {
                                handle_send_message.call(());
                            }
                        }
                    }
                    button {
                        class: "px-5 py-2.5 bg-accent hover:bg-accent-hover text-white font-medium rounded-xl transition-colors disabled:opacity-50 disabled:cursor-not-allowed",
                        onclick: move |_| {
                            info!("Send button clicked");
                            handle_send_message.call(());
                        },
                        "Send"
                    }
                }
            }
        }
    }
}
