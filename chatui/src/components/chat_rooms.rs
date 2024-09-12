use dioxus::prelude::*;

#[component]
pub fn ChatRooms() -> Element {
    let rooms = use_signal(|| vec!["General", "Freenet Dev", "Privacy Talk", "Decentralization"]);
    let mut current_room = use_signal(|| "General".to_string());

    rsx! {
        aside { class: "chat-rooms has-background-light",
            div { class: "logo-container",
                img { src: "/api/placeholder/125/92", alt: "Freenet Logo", class: "logo" }
            }
            div { class: "menu p-4", style: "flex-grow: 1; display: flex; flex-direction: column;",
                p { class: "menu-label", "Chat Rooms" }
                ul { class: "menu-list", style: "flex-grow: 1; overflow-y: auto;",
                    {rooms.read().iter().map(|room| {
                        rsx! {
                            li {
                                div {
                                    class: format_args!("is-flex is-justify-content-space-between {}", if current_room() == *room { "is-active" } else { "" }),
                                    onclick: move |_| current_room.set(room.to_string()),
                                    a { "{room}" }
                                    span {
                                        class: "more-info",
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
    }
}
