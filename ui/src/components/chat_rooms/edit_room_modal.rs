use dioxus::prelude::*;
use ed25519_dalek::SigningKey;
use crate::components::app::EditRoomModalActive;
use crate::room_data::{RoomData, Rooms};

#[component]
fn EditRoomModal() -> Element {
    let rooms = use_context::<Signal<Rooms>>();
    let edit_room_signal: Signal<EditRoomModalActive> = use_context::<Signal<EditRoomModalActive>>();
    let editing_room: Memo<Option<RoomData>> = use_memo(move || {
        if let Some(editing_room_vk) = edit_room_signal.read().room {
            rooms.read().map.iter().find_map(|(room_vk, room_data)| {
                if &editing_room_vk == room_vk {
                    Some(room_data.clone())
                } else {
                    None
                }
            })
        } else {
            None
        }
    });
    
    if let Some(room_data) = editing_room() {
        todo!("Edit room modal for room similar to member_info modal")
    }
}