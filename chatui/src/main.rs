#![allow(non_snake_case)]

use dioxus_logger::tracing::{Level, info};
use dioxus::prelude::*;

mod components;
mod example_data;
use components::app::App;

fn main() {
    // Init logger
    dioxus_logger::init(Level::INFO).expect("failed to init logger");
    info!("starting app");
    launch(App);
}
