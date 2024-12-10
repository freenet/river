use dioxus::prelude::*;
use dioxus::logger::tracing::*;

#[component]
pub fn MessageInput(
    new_message: Signal<String>,
    handle_send_message: EventHandler<()>,
) -> Element {
    rsx! {
        div { class: "new-message",
            div { class: "field has-addons",
                div { class: "control is-expanded",
                    input {
                        class: "input",
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
                }
                div { class: "control",
                    button {
                        class: "button send-button",
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
