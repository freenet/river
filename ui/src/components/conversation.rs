use crate::components::app::{EditRoomModalSignal, CURRENT_ROOM, EDIT_ROOM_MODAL, ROOMS};
use crate::room_data::{CurrentRoom, Rooms, SendMessageError};
use crate::util::get_current_system_time;
mod message_input;
mod not_member_notification;
use self::not_member_notification::NotMemberNotification;
use crate::components::conversation::message_input::MessageInput;
use chrono::{DateTime, Utc};
use dioxus::logger::tracing::*;
use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::FaPencil;
use dioxus_free_icons::Icon;
use freenet_scaffold::ComposableState;
use river_common::room_state::member::MemberId;
use river_common::room_state::member_info::MemberInfoV1;
use river_common::room_state::message::{AuthorizedMessageV1, MessageV1};
use river_common::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};
use std::rc::Rc;

#[component]
pub fn Conversation() -> Element {
    let current_room_data = CURRENT_ROOM
        .read()
        .owner_key
        .and_then(|key| ROOMS.read().map.get(&key).cloned());
    let last_chat_element = use_signal(|| None as Option<Rc<MountedData>>);
    let mut new_message = use_signal(|| "".to_string());
    let edit_room_modal_signal = use_context::<Signal<EditRoomModalSignal>>();

    let current_room_label = use_memo({
        move || {
            let rooms = ROOMS.read();
            let current_room = CURRENT_ROOM.read().owner_key;
            current_room
                .and_then(|key| rooms.map.get(&key))
                .map(|room_data| {
                    room_data
                        .room_state
                        .configuration
                        .configuration
                        .name
                        .clone()
                })
                .unwrap_or_else(|| "No Room Selected".to_string())
        }
    });

    // Trigger scroll to bottom when recent messages change
    use_effect(move || {
        let container = last_chat_element();
        if let Some(container) = container {
            wasm_bindgen_futures::spawn_local(async move {
                let _ = container.scroll_to(ScrollBehavior::Smooth).await;
            });
        }
    });

    let handle_send_message = {
        let current_room_data = current_room_data.clone();
        move || {
            let message = new_message.peek().to_string();
            if !message.is_empty() {
                new_message.set(String::new());
                if let (Some(current_room), Some(current_room_data)) =
                    (CURRENT_ROOM.read().owner_key, current_room_data)
                {
                    let message = MessageV1 {
                        room_owner: MemberId::from(current_room),
                        author: MemberId::from(&current_room_data.self_sk.verifying_key()),
                        content: message,
                        time: get_current_system_time(),
                    };
                    let auth_message =
                        AuthorizedMessageV1::new(message, &current_room_data.self_sk);
                    let delta = ChatRoomStateV1Delta {
                        recent_messages: Some(vec![auth_message.clone()]),
                        ..Default::default()
                    };
                    info!("Sending message: {:?}", auth_message);
                    ROOMS.write()
                        .map
                        .get_mut(&current_room)
                        .unwrap()
                        .room_state
                        .apply_delta(
                            &current_room_data.room_state,
                            &ChatRoomParametersV1 {
                                owner: current_room,
                            },
                            &Some(delta),
                        )
                        .unwrap();
                }
            } else {
                warn!("Message is empty");
            }
        }
    };

    rsx! {
        div { class: "main-chat",
            div { class: "room-header has-text-centered py-3 mb-4",
                div { class: "is-flex is-align-items-center is-justify-content-center",
                    h2 { class: "room-name is-size-4 has-text-weight-bold",
                        "{current_room_label}" // Wrapped in braces for interpolation
                    }
                    {
                        current_room_data.as_ref().map(|_room_data| {
                            rsx! {
                                button {
                                    class: "room-edit-button ml-2",
                                    title: "Edit room",
                                    onclick: move |_| {
                                        let current_room = CURRENT_ROOM.read().owner_key.unwrap();
                                        EDIT_ROOM_MODAL.write().room = Some(current_room);
                                    },
                                    Icon { icon: FaPencil, width: 14, height: 14 }
                                }
                            }
                        })
                    }
                }
            }
            div { class: "chat-messages",
                {
                    current_room_data.as_ref().map(|room_data| {
                        let room_state = room_data.room_state.clone();
                        if room_state.recent_messages.messages.is_empty() {
                            rsx! { /* Empty state, can be left blank or add a placeholder here */ }
                        } else {
                            let messages = &room_state.recent_messages.messages;
                            rsx! {
                                {messages.iter().enumerate().map(|(index, message)| {
                                    let is_last = index == messages.len() - 1;
                                    rsx! {
                                        MessageItem {
                                            key: "{message.id().0:?}", // Ensure this is a unique key expression
                                            message: message.clone(),
                                            member_info: room_state.member_info.clone(),
                                            last_chat_element: if is_last { Some(last_chat_element.clone()) } else { None },
                                        }
                                    }
                                })}
                            }
                        }
                    })
                }
            }
            {
                match current_room_data.as_ref() {
                    Some(room_data) => {
                        match room_data.can_send_message() {
                            Ok(()) => rsx! {
                                MessageInput {
                                    new_message: new_message,
                                    handle_send_message: move |_evt| {
                                        let handle = handle_send_message.clone();
                                        handle()
                                    },
                                }
                            },
                            Err(SendMessageError::UserNotMember) => {
                                let user_vk = room_data.self_sk.verifying_key();
                                let user_id = MemberId::from(&user_vk);
                                if !room_data.room_state.members.members.iter().any(|m| MemberId::from(&m.member.member_vk) == user_id) {
                                    rsx! {
                                        NotMemberNotification {
                                            user_verifying_key: user_vk
                                        }
                                    }
                                } else {
                                    rsx! {
                                        MessageInput {
                                            new_message: new_message,
                                            handle_send_message: move |_evt| {
                                                let handle = handle_send_message.clone();
                                                handle()
                                            },
                                        }
                                    }
                                }
                            },
                            Err(SendMessageError::UserBanned) => rsx! {
                                div { class: "notification is-danger",
                                    "You have been banned from sending messages in this room."
                                }
                            },
                        }
                    },
                    None => rsx! {
                        div { class: "welcome-message",
                            img {
                                src : asset!("/assets/freenet_logo.svg"),
                                alt: "Freenet Logo"
                            }
                            h1 { "Welcome to River" }
                            p { "Create a new room, or get invited to an existing one." }
                        }
                    },
                }
            }
        }
    }
}

#[component]
fn MessageItem(
    message: AuthorizedMessageV1,
    member_info: MemberInfoV1,
    last_chat_element: Option<Signal<Option<Rc<MountedData>>>>,
) -> Element {
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

    let content = markdown::to_html(message.message.content.as_str());

    let is_active_signal = use_signal(|| false);
    let mut is_active = is_active_signal.clone();

    rsx! {
        div { class: "box mb-3",
              onmounted: move |cx| {
                if let Some(mut last_chat_element) = last_chat_element {
                    last_chat_element.set(Some(cx.data()))
                }
            },
            article { class: "media",
                div { class: "media-content",
                    div { class: "content",
                        p {
                            strong {
                                class: "mr-2 clickable-username",
                                onclick: move |_| is_active.set(true),
                                "{member_name}"
                            }
                            small { class: "has-text-grey", "{time}" }
                            br {},
                            span {
                                dangerous_inner_html : "{content}"
                            }
                        }
                    }
                }
            }
        }
    }
}
