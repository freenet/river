use crate::room_data::{CurrentRoom, Rooms};
use crate::util::get_current_room_data;
use common::room_state::ban::{AuthorizedUserBan, UserBan};
use common::room_state::member::MemberId;
use dioxus::prelude::*;
use std::time::SystemTime;

#[component]
pub fn BanButton(
    member_id: MemberId,
    is_downstream: bool,
) -> Element {
    let rooms = use_context::<Signal<Rooms>>();
    let current_room = use_context::<Signal<CurrentRoom>>();
    let current_room_state = get_current_room_data(rooms, current_room);

    let handle_ban = move |_| {
        if let Some(room_data) = current_room_state.read().as_ref() {
            if let Some(signing_key) = &room_data.signing_key {
                let current_user_id = MemberId::new(&signing_key.verifying_key());
                
                let ban = UserBan {
                    owner_member_id: room_data.owner_id,
                    banned_at: SystemTime::now(),
                    banned_user: member_id,
                };

                let authorized_ban = AuthorizedUserBan::new(
                    ban,
                    current_user_id,
                    signing_key,
                );

                room_data.room_state.bans.0.push(authorized_ban);
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
