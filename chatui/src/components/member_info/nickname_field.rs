use std::ops::Deref;
use dioxus::prelude::*;
use common::state::member::{AuthorizedMember, MemberId};
use common::state::member_info::AuthorizedMemberInfo;
use crate::components::app::{CurrentRoom, Rooms};
use crate::util::get_current_room_state;

#[component]
pub fn NicknameField(
    member: AuthorizedMember,
    member_info: AuthorizedMemberInfo
) -> Element {
    let rooms = use_context::<Signal<Rooms>>();
    let current_room = use_context::<Signal<CurrentRoom>>();
    let current_room_state = get_current_room_state(rooms, current_room);
    
    let self_signing_key = use_memo(move || {
        current_room_state
            .read()
            .as_ref()
            .and_then(|room_state| room_state.user_signing_key.clone())
    });
    
    let self_member_id = use_memo(move || {
        self_signing_key
            .read()
            .as_ref()
            .map(|sk| MemberId::new(&sk.verifying_key()))
    });
    
    let is_self = use_memo(move || {
        self_member_id
            .read()
            .as_ref()
            .map(|smi| smi == &member.member.id())
            .unwrap_or(false)
    });

    let mut editing = use_signal(|| false);
    let mut nickname = use_signal(|| member_info.member_info.preferred_nickname.clone());

    let toggle_edit = move |_| {
        editing.set(!editing());
        if !editing() {
            // TODO: Implement callback to apply nickname change
            println!("Applying nickname change: {}", nickname());
        }
    };

    let update_nickname = move |evt: Event<FormData>| {
        nickname.set(evt.value().to_string());
    };

    rsx! {
        div { class: "field",
            label { class: "label", "Nickname" }
            div { class: "control is-flex",
                div { class: "is-flex-grow-1",
                    input {
                        class: "input",
                        value: "{nickname}",
                        readonly: !is_self() || !editing.read().deref(),
                        oninput: update_nickname,
                    }
                }
                if is_self() {
                    div { class: "is-flex-shrink-0 ml-2 nickname-icon-container",
                        span {
                            class: "icon is-clickable nickname-edit-icon",
                            onclick: toggle_edit,
                            i { 
                                class: if editing() { "fas fa-check" } else { "fas fa-edit" }
                            }
                        }
                    }
                }
            }
        }
    }
}

