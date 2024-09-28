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
                    {members.read().as_ref().map(|members| {
                        let member_info = &members.0;
                        let members = &members.1;
                        members.members.iter().map(|member| {
                            let member_info = member_info.member_info.iter().find(|mi| mi.member_info.member_id == member.member.owner_member_id).unwrap();
                            rsx! {
                                li {
                                    key: "{member.member.owner_member_id}",
                                    class: "user-list-item",
                                    div { class: "user-list",
                                        span { class: "icon is-small",
                                            i { class: "fas fa-user" }
                                        },
                                        span { "{member_info.member_info.preferred_nickname}" }
                                    }
                                }
                            }
                        }).collect::<Vec<_>>()
                    })}
                },
                div { class: "add-button mt-4",
                    button { class: "button is-small is-fullwidth",
                        onclick: move |_| {
                            // TODO: Implement invite user modal opening logic
                        },
                        span { class: "icon is-small", i { class: "fas fa-user-plus" } },
                        span { "Invite User" }
                    }
                }
            }
        }
    }
}
