mod ecies;

use std::time::*;
use dioxus::prelude::*;
use crate::room_data::{CurrentRoom, RoomData, Rooms};

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen(inline_js = "
export function get_current_time() {
    return Date.now();
}
")]
extern "C" {
    fn get_current_time() -> f64;
}

pub fn get_current_system_time() -> SystemTime {
    #[cfg(target_arch = "wasm32")]
    {
        // Convert milliseconds since epoch to a Duration
        let millis = get_current_time();
        let duration_since_epoch = Duration::from_millis(millis as u64);
        UNIX_EPOCH + duration_since_epoch
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        SystemTime::now()
    }
}


mod name_gen;
pub use name_gen::random_full_name;

pub fn use_current_room_data(
    rooms: &Signal<Rooms>,
    current_room: &Signal<CurrentRoom>,
) -> Signal<Option<RoomData>> {
    use_memo(move || {
        let current_room = current_room.read();
        current_room.owner_key()
            .and_then(|key| rooms.read().map.get(key))
            .cloned()
    })
}


