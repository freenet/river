use crate::components::app::{CurrentRoom, Rooms};
use crate::util::{get_current_room_data, get_current_system_time};
use crate::global_context::UserInfoModals;
use crate::components::member_info::MemberInfo;
mod message_input;
use self::message_input::MessageInput;
use chrono::{DateTime, Utc};
use common::room_state::member::MemberId;
use common::room_state::member_info::MemberInfoV1;
use common::room_state::message::{AuthorizedMessageV1, MessageV1};
use common::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};
use dioxus::prelude::*;
use dioxus_logger::tracing::{info, warn};
use freenet_scaffold::ComposableState;
use std::rc::Rc;
use wasm_bindgen_futures::spawn_local;

#[component]
pub fn MainChat() -> Element {
    let mut rooms = use_context::<Signal<Rooms>>();
    let current_room = use_context::<Signal<CurrentRoom>>();
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
                } else {
                    warn!("User signing key is not set");
                }
            }
        } else {
            warn!("Message is empty");
        }
    };

    rsx! {
        div { class: "main-chat",
            h2 { class: "room-name has-text-centered is-size-4 has-text-weight-bold py-3 mb-4",
                "{current_room_label}"
            }
            div { class: "chat-messages",
                {
                    current_room_data.read().as_ref().map(|room_data| {
                        let room_state = room_data.room_state.clone();
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
                    })
                }
            }
            MessageInput {
                new_message: new_message,
                handle_send_message: move |_| handle_send_message(),
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
    let mut user_info_modals = use_context::<Signal<UserInfoModals>>();
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

    use_effect(move || {
        user_info_modals.with_mut(|modals| {
            modals.modals.entry(author_id).or_insert_with(|| is_active_signal.clone());
        });
    });

    let mut is_active = is_active_signal.clone();

    rsx! {
        MemberInfo {
            member_id: author_id,
            is_active: is_active.clone(),
        }
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
