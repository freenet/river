#![allow(non_snake_case)]

use dioxus::prelude::*;

mod components;
mod constants;
mod example_data;
mod room_data;
mod util;

use components::app::App;

fn main() {
    launch(App);
}
