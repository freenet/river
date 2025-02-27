#![allow(non_snake_case)]

use dioxus::prelude::*;
use tracing::Level;

mod components;
mod constants;
#[cfg(feature = "example-data")]
mod example_data;
mod invites;
mod room_data;
mod util;

use components::app::App;

fn main() {
    // Initialize logging for WebAssembly
    dioxus::logger::init(Level::DEBUG).expect("failed to init logger");
    
    log::info!("River chat application starting...");
    
    launch(App);
}
