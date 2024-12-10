#![allow(non_snake_case)]

use dioxus::prelude::*;

mod components;
mod example_data;
mod util;
mod room_data;
mod constants;

use components::app::App;

fn main() {
    launch(App);
}
