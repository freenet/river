use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use common::ChatRoomStateV1;
use common::state::member::MemberId;

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
                    {current_room_state.read().as_ref().map(|room_state| {
                        let members = &room_state.members.members;
                        let member_info = &room_state.member_info.member_info;
                        
                        rsx! {
                            members.iter().map(|member| {
                                let member_id = member.member.id();
                                let nickname = member_info
                                    .iter()
                                    .find(|info| info.member_info.member_id == member_id)
                                    .map(|info| info.member_info.preferred_nickname.clone())
                                    .unwrap_or_else(|| format!("User {:?}", member_id));
                                
                                rsx! {
                                    li { key: "{member_id:?}",
                                        div { class: "is-flex is-align-items-center",
                                            span { class: "icon is-small mr-2",
                                                i { class: "fas fa-user" }
                                            }
                                            span { "{nickname}" }
                                        }
                                    }
                                }
                            })
                        }
                    })}
                }
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
