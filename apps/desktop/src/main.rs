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

/// Rune's headless tooling flags. A headless run gets a window with nothing behind it, so a
/// snapshot never probes for a node or installs anything.
const HEADLESS_FLAGS: &[&str] = &[
    "--snap",
    "--tree",
    "--script",
    "--list-scenes",
    "--trace",
    "--video",
    "--frames",
    "--strip",
];

fn main() {
    if std::env::args().any(|a| HEADLESS_FLAGS.contains(&a.as_str())) {
        rune::run(river_app::spec());
        return;
    }
    let host = Arc::new(host::DesktopHost::new());
    river_app::app(move |handle| RiverApp::attached(Controller::new(host, handle))).run();
}
