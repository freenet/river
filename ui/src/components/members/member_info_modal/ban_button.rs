use crate::components::app::MemberInfoModalSignal;
use crate::room_data::{CurrentRoom, RoomData, Rooms};
use crate::util::get_current_system_time;
use river_common::room_state::ban::{AuthorizedUserBan, UserBan};
use river_common::room_state::member::MemberId;
use river_common::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};
use dioxus::prelude::*;
use freenet_scaffold::ComposableState;

#[component]
pub fn BanButton(member_to_ban: MemberId, is_downstream: bool, nickname: String) -> Element {
    // Context signals
    let mut rooms_signal = use_context::<Signal<Rooms>>();
    let current_room_signal = use_context::<Signal<CurrentRoom>>();
    let mut modal_signal = use_context::<Signal<MemberInfoModalSignal>>();

    // Memos
    let current_room_data_signal: Memo<Option<RoomData>> = use_memo(move || {
        let rooms = rooms_signal.read();
        let current_room = current_room_signal.read();
        current_room
            .owner_key
            .as_ref()
            .and_then(|key| rooms.map.get(key).cloned())
    });
    let _self_member_id: Memo<Option<MemberId>> = use_memo(move || {
        rooms_signal
            .read()
            .map
            .get(&current_room_signal.read().owner_key?)
            .map(|r| MemberId::from(&r.self_sk.verifying_key()))
    });

    // Memoized values
    let _owner_key_signal = use_memo(move || current_room_signal.read().owner_key);

    let mut show_confirmation = use_signal(|| false);

    let execute_ban = move |_| {
        if let (Some(current_room), Some(room_data)) = (
            current_room_signal.read().owner_key,
            current_room_data_signal.read().as_ref(),
        ) {
            let user_signing_key = &room_data.self_sk;
            let ban = UserBan {
                owner_member_id: MemberId::from(&current_room),
                banned_at: get_current_system_time(),
                banned_user: member_to_ban,
            };

            let authorized_ban = AuthorizedUserBan::new(
                ban,
                MemberId::from(&user_signing_key.verifying_key()),
                user_signing_key,
            );

            let delta = ChatRoomStateV1Delta {
                bans: Some(vec![authorized_ban]),
                ..Default::default()
            };

            modal_signal.with_mut(|signal| {
                signal.member = None;
            });

            rooms_signal
                .write()
                .map
                .get_mut(&current_room)
                .unwrap()
                .room_state
                .apply_delta(
                    &room_data.room_state,
                    &ChatRoomParametersV1 {
                        owner: current_room,
                    },
                    &Some(delta),
                )
                .unwrap();
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
                            p {
                                "Are you sure you want to ban "
                                strong { "{nickname}" }
                                " (ID: "
                                code { "{member_to_ban}" }
                                ")? This action cannot be undone."
                            }
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
