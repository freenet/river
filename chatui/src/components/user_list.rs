use common::ChatRoomStateV1;
use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;

#[component]
pub fn MemberList(
    current_room: Signal<Option<VerifyingKey>>,
    current_room_state: Memo<Option<ChatRoomStateV1>>,
) -> Element {
    let members = use_memo(move || {
        current_room_state.read().as_ref().map(|room_state| {
            (room_state.member_info.clone(), room_state.members.clone())
        })
    });

    rsx! {
        aside { class: "user-list has-background-light",
            div { class: "menu p-4", style: "height: 100%; display: flex; flex-direction: column;",
                p { class: "menu-label", "Users in Room" },
                ul { class: "menu-list", style: "flex-grow: 1; overflow-y: auto;",
                    {
                    let members_lock = members.read();
                    if let Some((member_info, members)) = &*members_lock {
                        for member in &members.members {
                            let member_info = member_info.member_info.iter().find(|mi| mi.member_info.member_id == member.member.owner_member_id);
                            let nickname = member_info.map(|mi| mi.member_info.preferred_nickname.as_str()).unwrap_or("Unknown");

                                li {
                                    class: "user-list-item", key: "{member.member.member_id}",
                                    span { class: "icon is-small", i { class: "fas fa-user" } },
                                    span { "{nickname}" },
                                },
                        }
                    }
                        }
                }
                div { class: "add-button mt-4",
                    button { class: "button is-small is-fullwidth",
                        onclick: move |_| {
                            // TODO: Implement invite user modal opening logic
                            rsx! {}
                        },
                        span { class: "icon is-small", i { class: "fas fa-user-plus" } },
                        span { "Invite User" },
                    }
                }
            }
        }
    }
}
