use crate::components::app::{CURRENT_ROOM, EDIT_ROOM_MODAL, ROOMS};
use crate::room_data::SendMessageError;
use crate::util::ecies::{encrypt_with_symmetric_key};
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
use river_core::room_state::member::MemberId;
use river_core::room_state::member_info::MemberInfoV1;
use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};
use std::rc::Rc;

#[component]
pub fn Conversation() -> Element {
    let current_room_data = {
        let current_room = CURRENT_ROOM.read();
        if let Some(key) = current_room.owner_key {
            let rooms = ROOMS.read();
            rooms.map.get(&key).cloned()
        } else {
            None
        }
    };
    let last_chat_element = use_signal(|| None as Option<Rc<MountedData>>);
    let mut new_message = use_signal(|| "".to_string());

    let current_room_label = use_memo({
        move || {
            let current_room = CURRENT_ROOM.read();
            if let Some(key) = current_room.owner_key {
                let rooms = ROOMS.read();
                if let Some(room_data) = rooms.map.get(&key) {
                    return room_data
                        .room_state
                        .configuration
                        .configuration
                        .display
                        .name
                        .to_string_lossy();
                }
            }
            "No Room Selected".to_string()
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
            let message_text = new_message.peek().to_string();
            if !message_text.is_empty() {
                new_message.set(String::new());
                if let (Some(current_room), Some(current_room_data)) =
                    (CURRENT_ROOM.read().owner_key, current_room_data)
                {
                    // Encrypt message if room is private and we have the secret
                    let content = if current_room_data.is_private() {
                        if let Some((secret, version)) = current_room_data.get_secret() {
                            let (ciphertext, nonce) = encrypt_with_symmetric_key(secret, message_text.as_bytes());
                            RoomMessageBody::Private {
                                ciphertext,
                                nonce,
                                secret_version: version,
                            }
                        } else {
                            warn!("Room is private but no secret available, sending as public");
                            RoomMessageBody::public(message_text.clone())
                        }
                    } else {
                        RoomMessageBody::public(message_text.clone())
                    };

                    let message = MessageV1 {
                        room_owner: MemberId::from(current_room),
                        author: MemberId::from(&current_room_data.self_sk.verifying_key()),
                        content,
                        time: get_current_system_time(),
                    };
                    let auth_message =
                        AuthorizedMessageV1::new(message, &current_room_data.self_sk);
                    let delta = ChatRoomStateV1Delta {
                        recent_messages: Some(vec![auth_message.clone()]),
                        ..Default::default()
                    };
                    info!("Sending message: {:?}", auth_message);
                    ROOMS.with_mut(|rooms| {
                        if let Some(room_data) = rooms.map.get_mut(&current_room) {
                            if let Err(e) = room_data.room_state.apply_delta(
                                &current_room_data.room_state,
                                &ChatRoomParametersV1 {
                                    owner: current_room,
                                },
                                &Some(delta),
                            ) {
                                error!("Failed to apply message delta: {:?}", e);
                            }
                        }
                    });
                }
            } else {
                warn!("Message is empty");
            }
        }
    };

    rsx! {
        div { class: "main-chat",
            // Only show the room header when a room is selected
            {
                current_room_data.as_ref().map(|_| {
                    rsx! {
                        div { class: "room-header has-text-centered py-3 mb-4",
                            div { class: "is-flex is-align-items-center is-justify-content-center",
                                h2 { class: "room-name is-size-4 has-text-weight-bold",
                                    "{current_room_label}"
                                }
                                {
                                    current_room_data.as_ref().map(|_room_data| {
                                        rsx! {
                                            button {
                                                class: "room-edit-button ml-2",
                                                title: "Edit room",
                                                onclick: move |_| {
                                                    if let Some(current_room) = CURRENT_ROOM.read().owner_key {
                                                        EDIT_ROOM_MODAL.with_mut(|modal| {
                                                            modal.room = Some(current_room);
                                                        });
                                                    }
                                                },
                                                Icon { icon: FaPencil, width: 14, height: 14 }
                                            }
                                        }
                                    })
                                }
                            }
                        }
                    }
                })
            }
            div { class: "chat-messages",
                {
                    current_room_data.as_ref().map(|room_data| {
                        let room_state = room_data.room_state.clone();
                        if room_state.recent_messages.messages.is_empty() {
                            rsx! { /* Empty state, can be left blank or add a placeholder here */ }
                        } else {
                            let messages = &room_state.recent_messages.messages;
                            let room_secret = room_data.current_secret;
                            let room_secret_version = room_data.current_secret_version;
                            rsx! {
                                {messages.iter().enumerate().map(|(index, message)| {
                                    let is_last = index == messages.len() - 1;
                                    rsx! {
                                        MessageItem {
                                            key: "{message.id().0:?}", // Ensure this is a unique key expression
                                            message: message.clone(),
                                            member_info: room_state.member_info.clone(),
                                            last_chat_element: if is_last { Some(last_chat_element) } else { None },
                                            room_secret: room_secret,
                                            room_secret_version: room_secret_version,
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
    room_secret: Option<[u8; 32]>,
    room_secret_version: Option<u32>,
) -> Element {
    let author_id = message.message.author;
    let member_name = member_info
        .member_info
        .iter()
        .find(|ami| ami.member_info.member_id == author_id)
        .map(|ami| ami.member_info.preferred_nickname.to_string_lossy())
        .unwrap_or_else(|| "Unknown".to_string());

    let time = DateTime::<Utc>::from(message.message.time)
        .format("%H:%M")
        .to_string();

    // Decrypt message content if it's encrypted and we have the secret
    let content_text = match &message.message.content {
        RoomMessageBody::Public { plaintext } => plaintext.clone(),
        RoomMessageBody::Private { ciphertext, nonce, secret_version } => {
            if let (Some(secret), Some(current_version)) = (room_secret.as_ref(), room_secret_version) {
                if current_version == *secret_version {
                    use crate::util::ecies::decrypt_with_symmetric_key;
                    decrypt_with_symmetric_key(secret, ciphertext.as_slice(), nonce)
                        .map(|bytes: Vec<u8>| String::from_utf8_lossy(&bytes).to_string())
                        .unwrap_or_else(|e| {
                            warn!("Failed to decrypt message: {}", e);
                            message.message.content.to_string_lossy()
                        })
                } else {
                    format!("[Encrypted message with different secret version: v{} (current: v{})]", secret_version, current_version)
                }
            } else {
                message.message.content.to_string_lossy()
            }
        }
    };

    let content = markdown::to_html(&content_text);

    let is_active_signal = use_signal(|| false);
    let mut is_active = is_active_signal;

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
