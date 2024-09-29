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
