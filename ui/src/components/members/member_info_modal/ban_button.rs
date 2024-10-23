use crate::room_data::{CurrentRoom, Rooms};
use crate::util::get_current_room_data;
use common::room_state::ban::{AuthorizedUserBan, UserBan};
use common::room_state::member::MemberId;
use dioxus::prelude::*;
use std::time::SystemTime;
use ed25519_dalek::ed25519::signature::Keypair;
use ed25519_dalek::SigningKey;
use common::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};
use freenet_scaffold::ComposableState;

#[component]
pub fn BanButton(
    member_id: MemberId,
    is_downstream: bool,
) -> Element {
    let mut rooms = use_context::<Signal<Rooms>>();
    let current_room = use_context::<Signal<CurrentRoom>>();
    let current_room_data = get_current_room_data(rooms, current_room);

    let handle_ban = move |_| {
        if let Some(room_data) = current_room_data.read().as_ref() {
            if let Some(user_signing_key) = &room_data.user_signing_key {
                    let current_user_id = MemberId::new(&user_signing_key.verifying_key());
                    let owner_member_id = MemberId::new(&current_room.read().owner_key.expect("No owner key"));
                    let ban = UserBan {
                        owner_member_id,
                    banned_at: SystemTime::now(),
                    banned_user: member_id,
                };

                let authorized_ban = AuthorizedUserBan::new(
                    ban,
                    current_user_id,
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
                    .map.get_mut(&current_room.read().owner_key.unwrap()).unwrap()
                    .room_state.apply_delta(
                    &current_room_data.room_state,
                    &ChatRoomParametersV1 { owner: current_room }, &delta
                ).unwrap();
            }
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
