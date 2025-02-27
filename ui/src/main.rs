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
    // Launch with built-in logging
    dioxus::launch(App);
    
    // Alternatively, if you need to configure logging before launch:
    // dioxus_logger::init(Level::DEBUG).expect("failed to init logger");
    // tracing::info!("River chat application starting...");
    // launch(App);
}
