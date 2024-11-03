use crate::room_data::{CurrentRoom, Rooms};
use crate::util::{get_current_system_time, use_current_room_data};
use common::room_state::ban::{AuthorizedUserBan, UserBan};
use common::room_state::member::MemberId;
use dioxus::prelude::*;
use common::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};
use freenet_scaffold::ComposableState;

#[component]
pub fn BanButton(
    member_id: MemberId,
    is_downstream: bool,
) -> Element {
    let mut rooms = use_context::<Signal<Rooms>>();
    let current_room = use_context::<Signal<CurrentRoom>>();
    let current_room_data = use_current_room_data(rooms, current_room);
    let mut show_confirmation = use_signal(|| false);

    let execute_ban = move |_| {
        if let (Some(current_room), Some(room_data)) = (current_room.read().owner_key, current_room_data.read().as_ref()) {
            let user_signing_key = &room_data.self_sk;
            let ban = UserBan {
                owner_member_id: MemberId::from(&current_room),
                banned_at: get_current_system_time(),
                banned_user: member_id,
            };

            let authorized_ban = AuthorizedUserBan::new(
                ban,
                MemberId::from(&user_signing_key.verifying_key()),
                user_signing_key,
            );

            let delta = ChatRoomStateV1Delta {
                recent_messages: None,
                configuration: None,
                bans: Some(vec![authorized_ban]),
                members: None,
                member_info: None,
                upgrade: None,
            };

            rooms.write()
                .map.get_mut(&current_room).unwrap()
                .room_state.apply_delta(
                    &room_data.room_state,
                    &ChatRoomParametersV1 { owner: current_room },
                    &Some(delta)
                ).unwrap();
        }
    };

    if is_downstream {
        rsx! {
            div {
                button {
                    class: "button is-danger mt-3",
                    onclick: move |_| show_confirmation.set(true),
                    "Ban User"
                }

                div {
                    class: "modal",
                    class: if *show_confirmation.read() { "is-active" } else { "" },
                    
                    div { class: "modal-background" }
                    
                    div { class: "modal-card",
                        header { class: "modal-card-head",
                            p { class: "modal-card-title", "Confirm Ban" }
                            button { 
                                class: "delete",
                                onclick: move |_| show_confirmation.set(false),
                                aria_label: "close"
                            }
                        }
                        
                        section { class: "modal-card-body",
                            "Are you sure you want to ban this user? This action cannot be undone."
                        }
                        
                        footer { class: "modal-card-foot",
                            button {
                                class: "button is-danger",
                                onclick: execute_ban,
                                "Yes, Ban User"
                            }
                            button {
                                class: "button",
                                onclick: move |_| show_confirmation.set(false),
                                "Cancel"
                            }
                        }
                    }
                }
            }
        }
    } else {
        rsx! { "" }
    }
}
