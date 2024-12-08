#![allow(non_snake_case)]

use dioxus::prelude::*;

mod components;
mod example_data;
mod util;
mod room_data;
mod constants;

use components::app::App;

fn main() {
    // Initialize wasm logger
    tracing_wasm::set_as_global_default();
    launch(App);
}
