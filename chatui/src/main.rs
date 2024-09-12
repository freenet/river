#![allow(non_snake_case)]

use dioxus::prelude::*;
use dioxus_logger::tracing::{Level, info};

mod components;
use components::app::App;

fn main() {
    // Init logger
    dioxus_logger::init(Level::INFO).expect("failed to init logger");
    info!("starting app");

    // Configure the application
    let config = dioxus_web::Config::new().with_default_head(
        r#"
        <link rel="stylesheet" href="/assets/bulma.min.css">
        "#
    );

    // Launch the app with the custom configuration
    dioxus_web::launch_cfg(App, config);
}
