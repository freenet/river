use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use common::ChatRoomStateV1;
use common::state::message::AuthorizedMessageV1;
use common::state::member_info::MemberInfoV1;
use std::time::SystemTime;

fn format_time_ago(message_time: SystemTime) -> String {
    let now = SystemTime::now();
    let diff = now.duration_since(message_time).unwrap_or_default().as_secs() as i64;

    if diff < 60 {
        format!("{}s ago", diff)
    } else if diff < 3600 {
        format!("{}m ago", diff / 60)
    } else if diff < 86400 {
        format!("{}h ago", diff / 3600)
    } else {
        format!("{}d ago", diff / 86400)
    }
}

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
                    room_state.recent_messages.messages.iter().map(|message| {
                        rsx! {
                            MessageItem {
                                key: "{message.id().0:?}",
                                message: message.clone(),
                                member_info: room_state.member_info.clone()
                            }
                        }
                    }).collect::<Vec<_>>()
                }).unwrap_or_default()}
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
fn MessageItem(message: AuthorizedMessageV1, member_info: MemberInfoV1) -> Element {
    let author_nickname = member_info.member_info
        .iter()
        .find(|info| info.member_info.member_id == message.message.author)
        .map(|info| info.member_info.preferred_nickname.clone())
        .unwrap_or_else(|| format!("Unknown ({:?})", message.message.author.0));

    let time_ago = format_time_ago(message.message.time);

    rsx! {
        div { class: "message-item",
            div { class: "message-header",
                span { class: "message-author", "{author_nickname}" }
                span { class: "message-time", "{time_ago}" }
            }
            p { class: "message-content", "{message.message.content}" }
        }
    }
}
