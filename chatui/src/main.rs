#![allow(non_snake_case)]

use dioxus::prelude::*;
use dioxus_logger::tracing::{Level, info};

fn main() {
    // Init logger
    dioxus_logger::init(Level::INFO).expect("failed to init logger");
    info!("starting app");
    launch(App);
}

#[component(inline_props)]
fn App() -> Element {
    let rooms = use_signal(|| vec!["General", "Random", "Tech"]);
    let current_room = use_signal(|| "General".to_string());

    rsx! {
        "story"
    }
}