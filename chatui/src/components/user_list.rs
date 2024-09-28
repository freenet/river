use common::state::member::MembersV1;
use common::state::member_info::MemberInfoV1;
use common::ChatRoomStateV1;
use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;

#[component]
pub fn MemberList(
    current_room: Signal<Option<VerifyingKey>>,
    current_room_state: Memo<Option<ChatRoomStateV1>>,
) -> Element {
    let members: Memo<(MemberInfoV1, MembersV1)> = use_memo(move || {
        let read_state = current_room_state();
        let room_state = read_state.as_ref().unwrap();
        (room_state.member_info.clone(), room_state.members.clone())
    });

    rsx! {
        aside { class: "user-list has-background-light",
            div { class: "menu p-4", style: "height: 100%; display: flex; flex-direction: column;",
                p { class: "menu-label", "Users in Room" }
                ul { class: "menu-list", style: "flex-grow: 1; overflow-y: auto;", {
                    members.read().0.member_info.iter().map(|auth_member_info| {
                        let member_info = auth_member_info.member_info.clone();
                        let member = members.read().1.members.iter().find(|m| m.member.id() == member_info.member_id);
                        rsx! {
                            li {
                                key: "{member_info.member_id}",
                                class: "user-list-item",
                                div { class: "user-list",
                                    span { class: "icon is-small",
                                        i { class: "fas fa-user" }
                                    }
                                    span { "{member_info.preferred_nickname}" }
                                    if let Some(member) = member {
                                        span { class: "is-italic ml-2", "({})", member.member.nickname }
                                    }
                                    span { class: "ml-2", 
                                        img {
                                            src: "data:image/png;base64,{}", auth_member_info.member_info.avatar_data
                                        }
                                    }
                                }
                            }
                        }
                    }).collect::<Vec<Element>>().into_iter()
                }}
                div { class: "add-button mt-4",
                    button { class: "button is-small is-fullwidth",
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
