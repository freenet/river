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
use rand::seq::SliceRandom;

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

const FIRST_NAMES: Vec<&str> = vec![
    "Alice", "Bob", "Charlie", "Diana", "Eve", "Ali",
    "Frank", "Grace", "Hannah", "Ivan", "Jack", "Kyle",
    "Karen", "Liam", "Mona", "Nate", "Olivia",
    "Paul", "Quinn", "Rachel", "Sam", "Tina", "Derek",
    "Uma", "Victor", "Wendy", "Xander", "Yara",
    "Zane", "Amy", "Ben", "Cleo", "Derek", "Ian",
    "Elena", "Finn", "Gina", "Harry", "Isla", "Seth",
    "Jon", "Kara", "Leo", "Mia", "Noah", "Nacho",
];

const LAST_NAMES: Vec<&str> = vec![
    "Smith", "Johnson", "Williams", "Brown", "Jones", "Golden",
    "Garcia", "Miller", "Davis", "Rodriguez", "Martinez",
    "Hernandez", "Lopez", "Gonzalez", "Wilson", "Anderson",
    "Thomas", "Taylor", "Moore", "Jackson", "Martin", "Clarke", "Meier"
];

pub fn random_full_name() -> String {
    let mut rng = rand::thread_rng();
    let first_names = FIRST_NAMES;
    let last_names = LAST_NAMES;
    let first = first_names.choose(&mut rng).unwrap();
    let last = last_names.choose(&mut rng).unwrap();
    format!("{} {}", first, last)
}
