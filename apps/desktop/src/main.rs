//! River for the desktop.
//!
//! `cargo run -p river-desktop` opens the window. Headless, `-- --snap out.png --scene NAME`
//! renders a screen without touching the network (`-- --list-scenes` names them).
//! `-- --version` prints the app's version. `-- --node host:port` (or `FREENET_NODE=host:port`)
//! uses a Freenet node somewhere else instead of the one on this computer. `-- --node-check`
//! prints the node the arguments and environment name and exits (2 if the address is not valid). Windows release builds have no console, so it
//! prints nothing there.

// No console window behind the app on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod host;
mod install;
mod notify;
mod release;

use std::io::IsTerminal;
use std::sync::Arc;

use river_app::address::NodeChoice;
use river_app::{Controller, NodeHost, RiverApp};

fn main() {
    if matches!(std::env::args().nth(1).as_deref(), Some("--version" | "-V")) {
        // Not println!, which panics when stdout is a closed pipe.
        use std::io::Write;
        let _ = writeln!(std::io::stdout(), "river {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if std::env::args_os().any(|a| a == "--node-check") {
        node_check();
    }
    // A headless run (`--snap`, `--tree`, `--script`, ...) gets a window with nothing behind
    // it, so rendering a screen never probes for a node or installs anything.
    if rune::is_headless() {
        rune::run(river_app::spec());
        return;
    }
    #[cfg(windows)]
    notify::register_identity();
    let host = Arc::new(host::DesktopHost::new());
    // The node to use: `--node host:port`, else $FREENET_NODE, else the address saved from the
    // app's own screens, else the Freenet on this computer.
    let startup = NodeChoice::startup(std::env::args_os(), host.config_dir().as_deref());
    if let Some(problem) = &startup.problem {
        // From a terminal this is a command that failed. Anywhere else (a launcher, a Windows
        // release build with no console) nobody would see it, so the window says it.
        if std::io::stderr().is_terminal() {
            say(problem);
            std::process::exit(2);
        }
        say(problem);
    }
    if let Some(notice) = &startup.notice {
        say(notice);
    }
    river_app::app(move |handle| {
        let notifier = notify::DesktopNotifier::new(handle.clone());
        let rooms = handle.clone();
        let node = startup.choice.clone();
        let link = river_app::rooms::Link::for_node(
            &node.address.authority(),
            node.is_existing_node(),
            host.config_dir(),
            move |update, generation| {
                rooms.update(move |s, tx| s.apply_rooms_from(update, generation, tx))
            },
        );
        RiverApp::attached_at_startup(Controller::new(host, handle).with_node(node), link, startup)
            .with_notifier(Box::new(notifier))
    })
    .run();
}

/// `--node-check`: say which node the arguments and environment name, and exit without opening a
/// window. 0 for a usable address, 2 for one that isn't. For scripts, and so the address rules
/// can be tested through the real binary.
fn node_check() -> ! {
    let dirs = directories::ProjectDirs::from("", "Freenet", "River");
    let dir = dirs.as_ref().map(|d| d.config_local_dir().to_owned());
    let startup = NodeChoice::startup(std::env::args_os(), dir.as_deref());
    use std::io::Write;
    match startup.problem {
        Some(problem) => {
            let _ = writeln!(std::io::stderr(), "river: {problem}");
            std::process::exit(2);
        }
        None => {
            let _ = writeln!(std::io::stdout(), "node {}", startup.choice.address);
            std::process::exit(0);
        }
    }
}

/// A line on stderr, which may not exist (a closed stream must not panic).
fn say(line: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "river: {line}");
}
