//! River for the desktop.
//!
//! `cargo run -p river-desktop` opens the window. Headless, `-- --snap out.png --scene NAME`
//! renders a screen without touching the network (`-- --list-scenes` names them).

// No console window behind the app on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod host;
mod install;
mod release;

use std::sync::Arc;

use river_app::{Controller, RiverApp};

fn main() {
    // A headless run (`--snap`, `--tree`, `--script`, ...) gets a window with nothing behind
    // it, so rendering a screen never probes for a node or installs anything.
    if rune::is_headless() {
        rune::run(river_app::spec());
        return;
    }
    let host = Arc::new(host::DesktopHost::new());
    river_app::app(move |handle| RiverApp::attached(Controller::new(host, handle))).run();
}
