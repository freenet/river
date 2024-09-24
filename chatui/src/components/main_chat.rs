use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use common::ChatRoomStateV1;

#[component]
pub fn MainChat(
    current_room: Signal<Option<VerifyingKey>>,
    current_room_state: Memo<Option<ChatRoomStateV1>>
) -> Element {
    let mut new_message = use_signal(String::new);

    rsx! {
        div { class: "main-chat",
            div { class: "chat-messages",
                {current_room_state.read().as_ref().map(|room_state| {
                    rsx! {
                        // TODO: Implement message rendering based on room_state
                        div { "Messages will be rendered here" }
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
                                    // TODO: Implement message sending logic
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
