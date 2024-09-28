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

    // Convert members to Vector of (nickname, member_id)
    let members = match members() {
        Some((member_info, members)) => {
            members
                .members
                .iter()
                .map(|member| {
                    let nickname = member_info
                        .member_info
                        .iter()
                        .find(|mi| mi.member_info.member_id == member.member.owner_member_id)
                        .map(|mi| mi.member_info.preferred_nickname.clone())
                        .unwrap_or_else(|| "Unknown".to_string());
                    (nickname, member.member.owner_member_id)
                })
                .collect::<Vec<_>>()
        }
        None => Vec::new(),
    };
    
    rsx! {
        aside { class: "member-list",
            h2 { class: "member-list-title", "MEMBERS" }
            ul { class: "member-list-list",
                for (nickname, member_id) in members {
                        li {
                            key: "{member_id}",
                            class: "member-list-item",
                            "{nickname}"
                        }
                    }
            }
        }
    }
}

