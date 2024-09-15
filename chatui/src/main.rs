#![allow(non_snake_case)]

use dioxus::prelude::*;
use dioxus_desktop::{Config, WindowBuilder};
use dioxus_logger::tracing::{Level, info};

mod components;
mod models;
use components::app::App;

fn main() {
    // Init logger
    dioxus_logger::init(Level::INFO).expect("failed to init logger");
    info!("starting app");

    dioxus_desktop::launch_cfg(
        App,
        Config::new().with_window(
            WindowBuilder::new()
                .with_title("Chat App")
                .with_inner_size(dioxus_desktop::LogicalSize::new(800.0, 600.0)),
        ),
    );
}
