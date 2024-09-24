#![allow(non_snake_case)]

use dioxus::prelude::*;
use dioxus_logger::tracing::{Level, info};

mod components;
mod example_data;
use components::app::App;

fn main() {
    // Init logger
    dioxus_logger::init(Level::INFO).expect("failed to init logger");
    info!("starting app");

    #[cfg(target_arch = "wasm32")]
    dioxus_web::launch(App);

    #[cfg(not(target_arch = "wasm32"))]
    dioxus_desktop::launch(App);
}
