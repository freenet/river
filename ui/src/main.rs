#![allow(non_snake_case)]

use dioxus::prelude::*;
use dioxus_logger::tracing::{info, Level};

mod components;
mod example_data;
mod util;
mod room_data;
mod constants;

use components::app::App;

// Removed unused imports

fn main() {
    // Init logger
    dioxus_logger::init(Level::DEBUG).expect("failed to init logger");
    info!("starting app");
    launch(App);
}
