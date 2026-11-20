//! River for the desktop.
//!
//! `cargo run -p river-desktop` opens the window. Headless, `-- --snap out.png --scene NAME`
//! renders a screen without touching the network (`-- --list-scenes` names them).
//! `-- --version` prints the app's version. `-- --node host:port` (or `FREENET_NODE=host:port`)
//! uses a Freenet node somewhere else instead of the one on this computer. `-- --node-check`
//! prints the node the arguments and environment name and exits (2 if the address is not valid).
//! `-- --self-update` checks for, installs and starts a newer signed River once, without a
//! window (see `update/`). Windows release builds have no console, so they print nothing.

// No console window behind the app on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod host;
mod install;
mod notify;
mod release;
mod update;

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
    if matches!(std::env::args().nth(1).as_deref(), Some("--update-status")) {
        std::process::exit(update::host::print_status());
    }
    if matches!(std::env::args().nth(1).as_deref(), Some("--self-update")) {
        std::process::exit(update::host::run_cli());
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
    // After the command line has been accepted: a start that exits on a bad option is not a
    // failed update, so it is not counted (see `Updater::on_start`).
    let updater = update::host::DesktopUpdate::new();
    if let Some(u) = &updater {
        u.on_start();
    }
    let on_exit = updater.clone();
    let started = std::time::Instant::now();
    river_app::app(move |handle| {
        // An updater waiting on this process (and probation after a swap) hears that this
        // version works once it has connected to the node (the app reports that itself) or,
        // failing that, after it has run for a while with its event loop turning.
        let updates = updater.map(|u| {
            let (confirm, window) = (u.clone(), handle.clone());
            std::thread::spawn(move || {
                std::thread::sleep(update::CONFIRM_AFTER);
                window.update(move |_, _| river_app::UpdateHost::confirm_started(&*confirm));
            });
            let report = handle.clone();
            river_app::UpdateController::new(u, true, move |event| {
                report.update(move |s, _| s.apply_update(event))
            })
        });
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
        let app = RiverApp::attached_at_startup(
            Controller::new(host, handle).with_node(node),
            link,
            startup,
        )
        .with_notifier(Box::new(notifier));
        match updates {
            Some(updates) => app.with_updates(updates),
            None => app,
        }
    })
    .run();
    // The window closed normally: this start was not a failed one.
    if let Some(u) = on_exit {
        u.clean_exit(started.elapsed());
    }
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
