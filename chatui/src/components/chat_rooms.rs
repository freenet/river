use dioxus::prelude::*;
use crate::models::{ChatState, init_chat_state};

#[component]
pub fn ChatRooms(cx: Scope) -> Element {
    let chat_state = use_ref(cx, init_chat_state);

    cx.render(rsx! {
        aside { class: "chat-rooms has-background-light",
            div { class: "logo-container",
                img { src: "freenet_logo.svg", alt: "Freenet Logo", class: "logo" }
            }
            div { class: "menu p-4", style: "flex-grow: 1; display: flex; flex-direction: column;",
                p { class: "menu-label", "Chat Rooms" }
                ul { class: "menu-list", style: "flex-grow: 1; overflow-y: auto;",
                    {chat_state.read().rooms.values().map(|room| {
                        let room_id = room.id.clone();
                        let room_name = room.name.clone();
                        let is_active = chat_state.read().current_room == room_id;
                        rsx! {
                            li {
                                key: "{room_id}",
                                div {
                                    class: format_args!("room-item is-flex is-justify-content-space-between {}", if is_active { "is-active" } else { "" }),
                                    onclick: move |_| {
                                        chat_state.write().current_room = room_id.clone();
                                    },
                                    span { 
                                        class: format_args!("room-name {}", if is_active { "is-active" } else { "" }),
                                        "{room_name}" 
                                    }
                                    span {
                                        class: format_args!("more-info {}", if is_active { "is-active" } else { "" }),
                                        onclick: move |_| {
                                            // TODO: Implement modal opening logic
                                        },
                                        i { class: "fas fa-ellipsis-h" }
                                    }
                                }
                            }
                        }
                    })}
                }
                div { class: "add-button",
                    button {
                        onclick: move |_| {
                            // TODO: Implement new room modal opening logic
                        },
                        span { class: "icon is-small", i { class: "fas fa-plus" } }
                        span { "Add Room" }
                    }
                }
            }
        }
    })
}
