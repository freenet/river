#![allow(non_snake_case)]

use dioxus_logger::tracing::{Level, info};
use dioxus::prelude::*;
use dioxus_web::Config;

mod components;
mod example_data;
use components::app::App;

fn main() {
    // Init logger
    dioxus_logger::init(Level::INFO).expect("failed to init logger");
    info!("starting app");

    let config = Config::new().with_custom_head(
        r#"
        <link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/bulma@0.9.4/css/bulma.min.css">
        <link rel="stylesheet" href="/main.css">
        "#.to_string()
    );

    dioxus_web::launch(App, config);
}
