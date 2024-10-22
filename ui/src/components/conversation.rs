use crate::room_data::{CurrentRoom, Rooms, SendMessageError};
use crate::components::app::EditRoomModalSignal;
use crate::util::{get_current_room_data, get_current_system_time};
mod message_input;
mod not_member_notification;
use self::message_input::MessageInput;
use self::not_member_notification::NotMemberNotification;
use chrono::{DateTime, Utc};
use common::room_state::member::MemberId;
use common::room_state::member_info::MemberInfoV1;
use common::room_state::message::{AuthorizedMessageV1, MessageV1};
use common::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};
use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::FaPencil;
use dioxus_free_icons::Icon;
use dioxus_logger::tracing::{info, warn};
use freenet_scaffold::ComposableState;
use std::rc::Rc;
use wasm_bindgen_futures::spawn_local;

#[component]
pub fn Conversation() -> Element {
    let mut rooms = use_context::<Signal<Rooms>>();
    let current_room = use_context::<Signal<CurrentRoom>>();
    let mut edit_room_modal_signal = use_context::<Signal<EditRoomModalSignal>>();
    let current_room_data = get_current_room_data(rooms, current_room);

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

    let mut new_message = use_signal(String::new);
    let last_message_element: Signal<Option<Rc<MountedData>>> = use_signal(|| None);

    use_effect(move || {
        if let Some(element) = last_message_element.cloned() {
            spawn_local(async move {
                let _ = element.scroll_to(ScrollBehavior::Smooth).await;
            });
        }
    });

    let mut handle_send_message = move || {
        let message = new_message.peek().to_string();
        if !message.is_empty() {
            new_message.set(String::new());
            if let (Some(current_room), Some(current_room_data)) = (current_room.read().owner_key, current_room_data.read().as_ref()) {
                    let message = MessageV1 {
                        room_owner: MemberId::new(&current_room),
                        author: MemberId::new(&current_room_data.user_signing_key.verifying_key()),
                        content: message,
                        time: get_current_system_time(),
                    };
                    let auth_message = AuthorizedMessageV1::new(message, &current_room_data.user_signing_key);
                    let delta = ChatRoomStateV1Delta {
                        recent_messages: Some(vec![auth_message.clone()]),
                        configuration: None,
                        bans: None,
                        members: None,
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
            }
        } else {
            warn!("Message is empty");
        }
    };

    rsx! {
        div { class: "main-chat",
            div { class: "room-header has-text-centered py-3 mb-4",
                h2 { class: "room-name is-size-4 has-text-weight-bold",
                    "{current_room_label}"
                    {
                        current_room_data.read().as_ref().map(|_room_data| {
                            let current_room = current_room.read().owner_key.unwrap();
                            rsx! {
                                button {
                                    class: "room-edit-button ml-2",
                                    title: "Edit room",
                                    onclick: move |_| {
                                        edit_room_modal_signal.write().room = Some(current_room);
                                    },
                                    Icon { icon: FaPencil, width: 12, height: 12 }
                                }
                            }
                        })
                    }
                }
            }
            div { class: "chat-messages",
                {
                    current_room_data.read().as_ref().map(|room_data| {
                        let room_state = room_data.room_state.clone();
                        if room_state.recent_messages.messages.is_empty() {
                            rsx! {}
                        } else {
                            let last_message_index = room_state.recent_messages.messages.len() - 1;
                            rsx! {
                                {room_state.recent_messages.messages.iter().enumerate().map(|(ix, message)| {
                                    rsx! {
                                        MessageItem {
                                            key: "{message.id().0:?}",
                                            message: message.clone(),
                                            member_info: room_state.member_info.clone(),
                                            last_message_element: if ix == last_message_index { Some(last_message_element.clone()) } else { None },
                                        }
                                    }
                                })}
                            }
                        }
                    })
                }
            }
            {
                match current_room_data.read().as_ref().map(|room_data| room_data.can_send_message()) {
                    Some(Ok(())) => rsx! {
                        MessageInput {
                            new_message: new_message,
                            handle_send_message: move |_| handle_send_message(),
                        }
                    },
                    Some(Err(SendMessageError::UserNotMember)) => {
                        if let Some(room_data) = current_room_data.read().as_ref() {
                                rsx! {
                                    NotMemberNotification {
                                        user_verifying_key: room_data.user_signing_key.verifying_key()
                                    }
                                }
                        } else {
                            rsx! {
                                div { class: "notification is-light",
                                    "No room data available."
                                }
                            }
                        }
                    },
                    Some(Err(SendMessageError::UserBanned)) => rsx! {
                        div { class: "notification is-danger",
                            "You have been banned from sending messages in this room."
                        }
                    },
                    None => rsx! {
                        div { class: "notification is-light",
                            "No room selected."
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
    last_message_element: Option<Signal<Option<Rc<MountedData>>>>,
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
                if let Some(mut last_message_signal) = last_message_element {
                    last_message_signal.set(Some(cx.data()));
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
