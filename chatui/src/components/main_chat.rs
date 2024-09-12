use dioxus::prelude::*;

#[component]
pub fn MainChat() -> Element {
    let mut messages = use_signal(|| vec![
        ("Alice", "Welcome to Freenet Chat! How's everyone doing?"),
        ("Bob", "Hey Alice! Excited to be here. Love how private and secure this feels."),
        ("Charlie", "Same here! It's great to have a decentralized chat option."),
    ]);

    let mut new_message = use_signal(String::new);

    rsx! {
        div { class: "main-chat",
            div { class: "chat-messages",
                {messages.read().iter().map(|(sender, content)| {
                    rsx! {
                        div { class: "box",
                            strong { "{sender}: " }
                            "{content}"
                        }
                    }
                })}
            }
            div { class: "new-message",
                div { class: "field has-addons",
                    div { class: "control is-expanded",
                        input {
                            class: "input",
                            r#type: "text",
                            placeholder: "Type your message...",
                            value: "{new_message}",
                            oninput: move |evt| new_message.set(evt.value().to_string())
                        }
                    }
                    div { class: "control",
                        button {
                            class: "button is-primary",
                            onclick: move |_| {
                                let message = new_message.peek().to_string();
                                if !message.is_empty() {
                                    messages.write().push(("You", message));
                                    new_message.set(String::new());
                                }
                            },
                            "Send"
                        }
                    }
                }
            }
        }
    }
}
