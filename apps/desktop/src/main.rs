//! River for the desktop.
//!
//! `cargo run -p river-desktop` opens the window. Headless, `-- --snap out.png --scene NAME`
//! renders a screen without touching the network (`-- --list-scenes` names them).
//! `-- --version` prints the app's version. `-- --node host:port` (or `FREENET_NODE=host:port`)
//! uses a Freenet node somewhere else instead of the one on this computer. `-- --node-check`
//! prints the node the arguments and environment name and exits (2 if the address is not valid).
//! `-- --self-update` checks for, installs and starts a newer signed River once, without a
//! window (see `update/`). `-- --background` starts hidden in the tray (what start at login
//! runs). Windows release builds have no console, so they print nothing.
//!
//! Only one River runs per user: a second launch shows the first one's window and exits.

// No console window behind the app on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod autostart;
mod background;
#[cfg(windows)]
mod console;
mod host;
mod install;
mod notify;
mod release;
mod single;
mod tray;
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
    let updater = update::host::DesktopUpdate::new();
    let background_start = std::env::args_os().any(|a| a == autostart::BACKGROUND_FLAG);
    let settings = Arc::new(background::DesktopBackground::new(host.config_dir()));
    // One River per user: a later launch shows this one's window and leaves. This comes before
    // the updater counts the start (`on_start`), so a launch that only shows the first River
    // is not counted as an unconfirmed start of a swapped-in version.
    let instance = host.config_dir().and_then(|dir| {
        let token = std::env::var("XDG_ACTIVATION_TOKEN")
            .or_else(|_| std::env::var("DESKTOP_STARTUP_ID"))
            .ok();
        match single::acquire(&dir, token.as_deref()) {
            Ok(single::Start::First(guard)) => {
                let guard = Arc::new(guard);
                single::register(&guard);
                Some(guard)
            }
            Ok(single::Start::Running { asked }) => {
                // An updater waiting on this launch is released: leaving at once would read as
                // a failed start. Only when the running River was reached.
                if asked && let Some(u) = &updater {
                    u.second_launch_here();
                }
                if !asked {
                    tell_user(
                        "River is already running but did not answer. Look for its icon \
                         in the tray, or quit it and start it again.",
                    );
                }
                std::process::exit(0);
            }
            // Better two Rivers than none.
            Err(e) => {
                say(&format!("couldn't check for another River: {e}"));
                None
            }
        }
    });
    let for_app = updater.clone();
    let for_settings = settings.clone();
    let app = river_app::app(move |handle| {
        // The tray icon is made here on Windows and macOS: init runs on the thread that handles
        // the window system's events, which they need. A failure leaves the window up.
        #[cfg(any(windows, target_os = "macos"))]
        if !tray::start(&handle) {
            handle.open();
        }
        // An updater waiting on this process (and probation after a swap) hears that this
        // version works once it has connected to the node (the app reports that itself) or,
        // failing that, after it has run for a while with its event loop turning.
        let updates = for_app.map(|u| {
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
        .with_notifier(Box::new(notifier))
        .with_background(for_settings);
        match updates {
            Some(updates) => app.with_updates(updates),
            None => app,
        }
    });
    let handle = app.handle();
    // Listening starts before anything slow, so a second launch is answered at once.
    if let Some(guard) = &instance {
        let show = handle.clone();
        let served = guard.serve(move |activation| match activation {
            Some(token) => show.activate(token),
            None => show.open(),
        });
        if let Err(e) = served {
            say(&format!("couldn't listen for a second River: {e}"));
        }
    }
    // After the command line has been accepted and the instance check passed: a start that
    // exits on a bad option, or only shows the other River, is not a failed update, so it is
    // not counted (see `Updater::on_start`).
    if let Some(u) = &updater {
        u.on_start();
    }
    // Only where this process already owns a console (debug or console builds; a release GUI
    // build never attaches to its parent's): Ctrl+C there quits River cleanly.
    #[cfg(windows)]
    console::quit_on_console_signals(handle.clone());
    let on_exit = updater.clone();
    let started = std::time::Instant::now();
    settings.refresh_login();
    // Linux has no event-loop thread to respect, and whether the desktop has a tray decides how
    // the app starts, so ask now.
    #[cfg(all(unix, not(target_os = "macos")))]
    let tray_expected = tray::start(&handle);
    #[cfg(any(windows, target_os = "macos"))]
    let tray_expected = true;
    // Closing the window hides it; `on_close` quits when there is no tray or the person asked
    // for closing to quit. Started at login, there is no window until the tray asks for one.
    let mut app = app.keep_running().on_close(background::on_close);
    if background_start && tray_expected {
        app = app.windowless();
    }
    app.run();
    // The window closed normally: this start was not a failed one.
    if let Some(u) = on_exit {
        u.clean_exit(started.elapsed());
    }
    #[cfg(any(windows, target_os = "macos"))]
    tray::stop();
    drop(instance);
    #[cfg(windows)]
    console::finished();
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

/// Tell the person something when nobody may be reading stderr (a launcher, a Windows release
/// build with no console): on stderr, and as a notification where there is one to show.
fn tell_user(line: &str) {
    say(line);
    #[cfg(not(target_os = "macos"))]
    {
        let mut note = notify_rust::Notification::new();
        note.appname("River").summary("River").body(line);
        #[cfg(windows)]
        note.app_id("org.freenet.River");
        let _ = note.show();
    }
}

/// A line on stderr, which may not exist (a closed stream must not panic).
fn say(line: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "river: {line}");
}
