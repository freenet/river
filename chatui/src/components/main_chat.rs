use crate::components::app::{CurrentRoom, Rooms};
use chrono::{DateTime, Utc};
use common::state::member_info::MemberInfoV1;
use common::state::message::AuthorizedMessageV1;
use dioxus::prelude::*;
use web_sys::HtmlElement;

#[component]
pub fn MainChat() -> Element {
    let rooms = use_context::<Signal<Rooms>>();
    let current_room = use_context::<Signal<CurrentRoom>>();
    let current_room_data = use_memo(move || match current_room.read().owner_key {
        Some(owner_key) => rooms.read().map.get(&owner_key).map(|rd| rd.clone()),
        None => None,
    });
    let current_room_label = use_memo(move || {
        current_room_data
            .read()
            .as_ref()
            .map(|room_data| {
                room_data
                    .room_state
                    .configuration
                    .configuration
                    .name
                    .clone()
            })
            .unwrap_or_else(|| "No Room Selected".to_string())
    });
    let new_message = use_signal(String::new);
    let chat_messages_ref = use_node_ref();

    use_effect(move || {
        if let Some(messages_element) = chat_messages_ref.get() {
            let messages_element: &HtmlElement = messages_element.unchecked_ref();
            messages_element.set_scroll_top(messages_element.scroll_height());
        }
    });

    rsx! {
        div { class: "main-chat d-flex flex-column vh-100",
            h2 { class: "room-name has-text-centered is-size-4 has-text-weight-bold py-3 mb-4 has-background-light",
                "{current_room_label}"
            }
            div { 
                class: "chat-messages flex-grow-1 overflow-auto",
                ref: chat_messages_ref,
                {
                    current_room_data.read().as_ref().map(|room_data| {
                    let room_state = room_data.room_state.clone();
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
                                info!("Send button clicked");
                                let message = new_message.peek().to_string();
                                if !message.is_empty() {
                                    new_message.set(String::new());
                                    if let (Some(current_room), Some(current_room_data)) = (current_room.read().owner_key, current_room_data.read().as_ref()) {
                                        if let Some(user_signing_key) = &current_room_data.user_signing_key {
                                        let message = MessageV1 {
                                                room_owner: MemberId::new(&current_room),
                                                author: MemberId::new(&user_signing_key.verifying_key()),
                                                content: message,
                                                time: get_current_system_time(),
                                            };
                                        let auth_message = AuthorizedMessageV1::new(message, user_signing_key);
                                        let delta = ChatRoomStateV1Delta {
                                            recent_messages: Some(vec![auth_message.clone()]),
                                            configuration: None,
                                            bans: None,members: None,
                                            member_info: None,
                                            upgrade: None,
                                        };
                                            info!("Sending message: {:?}", auth_message);
                                        rooms.write()
                                            .map.get_mut(&current_room).unwrap()
                                            .room_state.apply_delta(
                                                &current_room_data.room_state,
                                                &ChatRoomParametersV1 { owner: current_room }, &delta
                                            ).unwrap();
                                        } else {
                                            warn!("User signing key is not set");
                                        }
                                }
                            } else {
                                    warn!("Message is empty");
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
