use dioxus::prelude::*;
use dioxus_router::prelude::*;
use crate::example_data::create_example_room;
use std::collections::HashMap;

pub fn App(cx: Scope) -> Element {
    let rooms = use_signal(|| {
        let mut rooms = HashMap::new();
        let (room_key, room_state) = create_example_room();
        rooms.insert(room_key, (room_state, None));
        rooms
    });

    let current_room = use_signal(|| None);
    let current_room_state = use_memo(|| current_room.read().and_then(|key| rooms.read().get(&key).map(|(state, _)| state.clone())), [current_room, rooms]);

    cx.render(rsx! {
        Router {
            Switch {
                Route { to: "/", ChatRooms { rooms: rooms, current_room: current_room } }
                Route { to: "/chat", MainChat { current_room: current_room, current_room_state: current_room_state } }
                Route { to: "/members", MemberList { current_room: current_room, current_room_state: current_room_state } }
                Route { to: "/modal", Modal { current_room: current_room, current_room_state: current_room_state } }
            }
        }
    })
}
