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

// Add favicon to the document head
fn add_favicon() {
    let window = web_sys::window().unwrap();
    let document = window.document().unwrap();
    
    // Get the head element using querySelector
    if let Some(head) = document.query_selector("head").ok().flatten() {
        let link = document.create_element("link").unwrap();
        link.set_attribute("rel", "icon").unwrap();
        link.set_attribute("href", "/assets/river_logo.svg").unwrap();
        link.set_attribute("type", "image/svg+xml").unwrap();
        
        head.append_child(&link).unwrap();
    }
}

// Custom implementation for getrandom when targeting wasm32-unknown-unknown
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
#[no_mangle]
unsafe extern "Rust" fn __getrandom_v02_custom(
    dest: *mut u8,
    len: usize,
) -> Result<(), getrandom::Error> {
    use js_sys::Uint8Array;
    use std::num::NonZeroU32;
    use web_sys::window;

    // Get the window object
    let window = window().ok_or_else(|| getrandom::Error::from(NonZeroU32::new(1).unwrap()))?;

    // Get the crypto object directly from window
    let crypto = window
        .crypto()
        .map_err(|_| getrandom::Error::from(NonZeroU32::new(1).unwrap()))?;

    // Create a buffer to hold the random bytes
    let mut buffer = vec![0u8; len];

    // Fill the buffer with random values
    match crypto.get_random_values_with_u8_array(&mut buffer) {
        Ok(_) => {
            // Copy the random bytes to the destination buffer
            let dest_slice = core::slice::from_raw_parts_mut(dest, len);
            dest_slice.copy_from_slice(&buffer);
            Ok(())
        }
        Err(_) => Err(getrandom::Error::from(NonZeroU32::new(1).unwrap())),
    }
}

fn main() {
    dioxus::logger::initialize_default();
    
    // Add favicon when running in browser
    #[cfg(target_arch = "wasm32")]
    {
        add_favicon();
    }
    
    launch(App);
}
