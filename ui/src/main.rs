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

fn main() {
    // Initialize console error panic hook for better error messages
    console_error_panic_hook::set_once();
    
    // Initialize Dioxus logger - this will handle platform-specific logging
    // including web via tracing-wasm
    dioxus_logger::init(log::LevelFilter::Debug).expect("failed to init logger");
    
    log::info!("River chat application starting...");
    
    launch(App);
}
