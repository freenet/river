#![allow(non_snake_case)]

use dioxus::prelude::*;

mod components;
mod constants;
#[cfg(feature = "example-data")]
mod example_data;
mod invites;
mod room_data;
mod util;

use components::app::App;

// Custom implementation for getrandom when targeting wasm32-unknown-unknown
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
#[no_mangle]
unsafe extern "Rust" fn __getrandom_v02_custom(
    dest: *mut u8,
    len: usize,
) -> Result<(), getrandom::Error> {
    use js_sys::Uint8Array;
    use wasm_bindgen::JsCast;
    use web_sys::window;

    // Get the window object
    let window = window().ok_or_else(|| getrandom::Error::from("No window object available"))?;
    
    // Get the crypto object
    let crypto = window
        .crypto()
        .map_err(|_| getrandom::Error::from("No crypto object available"))?;
    
    // Create a buffer to hold the random bytes
    let buffer = Uint8Array::new_with_length(len as u32);
    
    // Fill the buffer with random values
    crypto
        .get_random_values_with_u8_array(&buffer)
        .map_err(|_| getrandom::Error::from("Failed to get random values"))?;
    
    // Copy the random bytes to the destination buffer
    let buf = unsafe { core::slice::from_raw_parts_mut(dest, len) };
    buffer.copy_to(buf);
    
    Ok(())
}

fn main() {
    dioxus::logger::initialize_default();
    launch(App);
}
