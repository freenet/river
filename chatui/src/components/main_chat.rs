use std::ops::Deref;
use dioxus::prelude::*;
use crate::models::ChatState;

#[component]
pub fn MainChat() -> Element {
    let chat_state = use_context::<ChatState>();

    let current_room = use_memo(|| chat_state.current_room.read().unwrap());
    
    let room = use_memo(|| chat_state.rooms.get(current_room.read().deref()));
    
    let mut new_message = use_signal(String::new);

    rsx! {
        div { class: "main-chat",
            div { class: "chat-messages",
                {room.read().iter().map(|(sender, content)| {
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
                                    messages.write().push(("You".to_string(), message));
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
