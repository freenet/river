use std::collections::HashMap;
use std::ops::Deref;
use dioxus::prelude::*;
use ed25519_dalek::{SigningKey, VerifyingKey};
use common::ChatRoomStateV1;
use crate::example_data::create_example_room;

pub fn App() -> Element {
    let rooms: Signal<HashMap<VerifyingKey, (ChatRoomStateV1, Option<SigningKey>)>> = use_signal(|| {
        let mut map = HashMap::new();
        let (verifying_key, room_state) = create_example_room();
        map.insert(verifying_key, (room_state, None));
        map
    });
    let current_room: Signal<Option<VerifyingKey>> = use_signal(|| None);
    let current_room_state: Memo<Option<ChatRoomStateV1>> = use_memo(move || {
        current_room().and_then(|current_room_key| {
            rooms.read().deref().get(&current_room_key).map(|(room_state, _)| room_state.clone())
        })
    });

    rsx! {
        div { class: "chat-container",
            // Chat Rooms
            aside { class: "chat-rooms has-background-light",
                div { class: "logo-container",
                    img { class: "logo", src: "/api/placeholder/125/92", alt: "Freenet Logo" }
                }
                div { class: "menu p-4", style: "flex-grow: 1; display: flex; flex-direction: column;",
                    p { class: "menu-label", "Chat Rooms" }
                    ul { class: "menu-list", style: "flex-grow: 1; overflow-y: auto;",
                        li {
                            div { class: "is-active is-flex is-justify-content-space-between",
                                a { "General" }
                                span { class: "more-info", onclick: move |_| { /* TODO: Implement modal */ },
                                    i { class: "fas fa-ellipsis-h" }
                                }
                            }
                        }
                        // Add more rooms here
                    }
                    div { class: "add-button",
                        button { onclick: move |_| { /* TODO: Implement add room */ },
                            span { class: "icon is-small", i { class: "fas fa-plus" } }
                            span { "Add Room" }
                        }
                    }
                }
            }

            // Main Chat
            div { class: "main-chat",
                div { class: "chat-messages",
                    div { class: "box",
                        strong { "Alice:" }
                        " Welcome to Freenet Chat! How's everyone doing?"
                    }
                    // Add more messages here
                }
                div { class: "new-message",
                    div { class: "field has-addons",
                        div { class: "control is-expanded",
                            input { class: "input", type: "text", placeholder: "Type your message..." }
                        }
                        div { class: "control",
                            button { class: "button is-primary", "Send" }
                        }
                    }
                }
            }

            // User List
            aside { class: "user-list has-background-light",
                div { class: "menu p-4", style: "height: 100%; display: flex; flex-direction: column;",
                    p { class: "menu-label", "Users in Room" }
                    ul { class: "menu-list", style: "flex-grow: 1; overflow-y: auto;",
                        li {
                            div { class: "is-flex is-justify-content-space-between",
                                span { "Alice" }
                                span { class: "more-info", onclick: move |_| { /* TODO: Implement modal */ },
                                    i { class: "fas fa-ellipsis-h" }
                                }
                            }
                        }
                        // Add more users here
                    }
                    div { class: "add-button",
                        button { onclick: move |_| { /* TODO: Implement invite user */ },
                            span { class: "icon is-small", i { class: "fas fa-user-plus" } }
                            span { "Invite User" }
                        }
                    }
                }
            }

            // Modal (placeholder, implement actual modal component later)
            div { id: "infoModal", class: "modal",
                div { class: "modal-background" }
                div { class: "modal-card",
                    header { class: "modal-card-head",
                        p { class: "modal-card-title", id: "modalTitle" }
                        button { class: "delete", aria_label: "close" }
                    }
                    section { class: "modal-card-body",
                        div { id: "modalContent" }
                    }
                    footer { class: "modal-card-foot",
                        button { class: "button is-success", "Save changes" }
                        button { class: "button", "Cancel" }
                    }
                }
            }
        }
    }
}
