use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use common::ChatRoomStateV1;

#[component]
pub fn MemberList(
    current_room: Signal<Option<VerifyingKey>>,
    current_room_state: Memo<Option<ChatRoomStateV1>>
) -> Element {
    rsx! {
        aside { class: "user-list has-background-light",
            div { class: "menu p-4", style: "height: 100%; display: flex; flex-direction: column;",
                p { class: "menu-label", "Users in Room" }
                ul { class: "menu-list", style: "flex-grow: 1; overflow-y: auto;",
                    {current_room_state.read().as_ref().map(|_room_state| {
                        // TODO: Implement user list rendering based on room_state
                        rsx! {
                            li { "User list will be rendered here" }
                        }
                    })}
                }
                div { class: "add-button",
                    button {
                        onclick: move |_| {
                            // TODO: Implement invite user modal opening logic
                        },
                        span { class: "icon is-small", i { class: "fas fa-user-plus" } }
                        span { "Invite User" }
                    }
                }
            }
        }
    }
}
