#![allow(non_snake_case)]

use dioxus_web::Config;
use dioxus_logger::tracing::{Level, info};

mod components;
mod example_data;
use components::app::App;

fn main() {
    // Init logger
    dioxus_logger::init(Level::INFO).expect("failed to init logger");
    info!("starting app");

    dioxus_web::launch_with_props(
        App,
        (),
        Config::default().with_root_name("app"),
    );
}
