mod ecies;

use std::time::{Duration, SystemTime, UNIX_EPOCH};
use wasm_bindgen::prelude::*;

#[wasm_bindgen(inline_js = "
export function get_current_time() {
    return Date.now();
}
")]
extern "C" {
    fn get_current_time() -> f64;
}

pub fn get_current_system_time() -> SystemTime {
    // Convert milliseconds since epoch to a Duration
    let millis = get_current_time();
    let duration_since_epoch = Duration::from_millis(millis as u64);

    // Add the duration to the UNIX_EPOCH to get the current SystemTime
    UNIX_EPOCH + duration_since_epoch
}

use crate::room_data::{CurrentRoom, Rooms, RoomData};
use dioxus::prelude::*;

pub fn use_current_room_data(
    rooms: Signal<Rooms>,
    current_room: Signal<CurrentRoom>,
) -> Memo<Option<RoomData>> {
    use_memo(move || {
        let current_room = current_room.read();
        let rooms = rooms.read();
        
        current_room
            .owner_key
            .as_ref()
            .and_then(|key| rooms.map.get(key))
            .cloned()
    })
}
