use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use common::ChatRoomStateV1;
use common::state::message::AuthorizedMessageV1;

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
                        {room_state.recent_messages.messages.iter().map(|message| {
                            rsx! {
                                MessageItem {
                                    key: "{message.id().0:?}",
                                    message: message
                                }
                            }
                        })}
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

#[component]
fn MessageItem(message: &AuthorizedMessageV1) -> Element {
    rsx! {
        div { class: "message-item",
            p { class: "message-author", "{message.message.author.0:?}" }
            p { class: "message-content", "{message.message.content}" }
            p { class: "message-time", "{message.message.time:?}" }
        }
    }
}
