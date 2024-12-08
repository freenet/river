#![allow(non_snake_case)]

use dioxus::prelude::*;
use tracing::{info, Level};

mod components;
mod example_data;
mod util;
mod room_data;
mod constants;

use components::app::App;

fn main() {
    // Initialize built-in Dioxus logger
    dioxus::prelude::init_logger(Level::DEBUG);
    info!("starting app");
    launch(App);
}
