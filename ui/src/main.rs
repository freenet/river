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
    use web_sys::window;
    use std::num::NonZeroU32;

    // Create a custom error code (1 = unavailable)
    let unavailable_error = NonZeroU32::new(1).unwrap();
    
    // Get the window object
    let window = window().ok_or_else(|| getrandom::Error::new(unavailable_error))?;
    
    // Get the crypto object
    let crypto = window
        .navigator()
        .crypto()
        .map_err(|_| getrandom::Error::new(unavailable_error))?;
    
    // Create a buffer to hold the random bytes
    let buffer = Uint8Array::new_with_length(len as u32);
    
    // Fill the buffer with random values
    match crypto.get_random_values_with_u8_array(&buffer) {
        Ok(_) => {
            // Copy the random bytes to the destination buffer
            let buf = core::slice::from_raw_parts_mut(dest, len);
            buffer.copy_to(buf);
            Ok(())
        },
        Err(_) => Err(getrandom::Error::new(unavailable_error))
    }
}

fn main() {
    dioxus::logger::initialize_default();
    launch(App);
}
