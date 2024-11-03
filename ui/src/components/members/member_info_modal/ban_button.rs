use crate::room_data::{CurrentRoom, Rooms};
use crate::util::{get_current_system_time, use_current_room_data};
use common::room_state::ban::{AuthorizedUserBan, UserBan};
use common::room_state::member::MemberId;
use dioxus::prelude::*;
use dioxus_logger::tracing::info;
use common::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};
use freenet_scaffold::ComposableState;

#[component]
pub fn BanButton(
    member_id: MemberId,
    is_downstream: bool,
) -> Element {
    info!("Rendering BanButton for member_id: {:?} is_downstream: {:?}", member_id, is_downstream);

    let mut rooms = use_context::<Signal<Rooms>>();
    let current_room = use_context::<Signal<CurrentRoom>>();
    let current_room_data = use_current_room_data(rooms, current_room);

    let handle_ban = move |_| {
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

            info!("Applying ban to room: {:?}", delta);

            info!("Room state before applying ban: {:?}", rooms.read().map.get(&current_room).unwrap().room_state);

            rooms.write()
                .map.get_mut(&current_room).unwrap()
                .room_state.apply_delta(
                    &room_data.room_state,
                    &ChatRoomParametersV1 { owner: current_room },
                    &Some(delta)
                ).unwrap();

            // Show room state after applying the ban
            info!("Room state after applying ban: {:?}", rooms.read().map.get(&current_room).unwrap().room_state);
        }
    };

    if is_downstream {
        rsx! {
            button {
                class: "button is-danger mt-3",
                onclick: handle_ban,
                "Ban User"
            }
        }
    } else {
        rsx! { "" }
    }
}
