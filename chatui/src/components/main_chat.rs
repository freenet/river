use common::state::member_info::MemberInfoV1;
use common::state::message::{AuthorizedMessageV1, MessageV1};
use common::ChatRoomStateV1;
use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;

#[component]
pub fn MainChat(
    current_room: Signal<Option<VerifyingKey>>,
    current_room_state: Memo<Option<ChatRoomStateV1>>,
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
                                    message: message.clone(),
                                    member_info: room_state.member_info.clone()
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
                                    new_message.set(String::new());
                                    //match current_room {
                                    //    Some(room_owner_vk) => {
                                     //       let message = MessageV1 {
                                     //           room_owner: room_owner_vk,
                                     //       };
                                     //   }
                                   // }
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

use chrono::{DateTime, Utc};
use common::state::member::MemberId;

#[component]
fn MessageItem(message: AuthorizedMessageV1, member_info: MemberInfoV1) -> Element {
    let author_id = message.message.author;
    let member_name = member_info
        .member_info
        .iter()
        .find(|ami| ami.member_info.member_id == author_id)
        .map(|ami| ami.member_info.preferred_nickname.clone())
        .unwrap_or_else(|| "Unknown".to_string());

    let time = DateTime::<Utc>::from(message.message.time)
        .format("%H:%M")
        .to_string();

    rsx! {
        div { class: "box mb-3",
            article { class: "media",
                div { class: "media-content",
                    div { class: "content",
                        p {
                            strong { class: "mr-2", "{member_name}" }
                            small { class: "has-text-grey", "{time}" }
                            br {},
                            "{message.message.content}"
                        }
                    }
                }
            }
        }
    }
}
