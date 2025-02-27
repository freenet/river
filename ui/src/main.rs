#![allow(non_snake_case)]

use dioxus::prelude::*;

mod components;
mod constants;
#[cfg(feature = "example-data")]
mod example_data;
mod invites;
mod room_data;
mod util;

use components::app::App;

fn main() {
    // Initialize console error panic hook for better error messages
    console_error_panic_hook::set_once();
    
    // Initialize tracing for WebAssembly
    tracing_wasm::set_as_global_default_with_config(
        tracing_wasm::WASMLayerConfig::default()
            .set_max_level(log::LevelFilter::Debug)
    );
    
    log::info!("River chat application starting...");
    
    launch(App);
}
