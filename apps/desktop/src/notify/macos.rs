//! macOS: `UNUserNotificationCenter`, which needs River's signed `.app` bundle (its bundle
//! identifier is `org.freenet.River`, see `packaging/macos/Info.plist`). Run outside the
//! bundle (`cargo run`) there is no bundle identifier and nothing is shown.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use river_app::RiverApp;
use river_app::notify::Notification;
use rune::AppHandle;

use super::Waiters;

pub struct Backend {
    state: State,
    waiters: Waiters,
    /// A number for each notification: the platform replaces by room, and the waiter of a
    /// replaced one is not known to end, so each gets its own.
    next: AtomicU64,
}

enum State {
    /// Not asked yet: the first notification checks the bundle and asks permission.
    Unasked,
    Ready,
    /// Not in a bundle: nothing will change that while River runs.
    NoBundle,
    /// Not allowed, or the question failed. Asked again after a while, since the person may
    /// have allowed it in System Settings.
    Denied(Instant),
}

/// How long to wait before asking again after a refusal.
const ASK_AGAIN: Duration = Duration::from_secs(300);

impl Backend {
    pub fn new() -> Self {
        Backend {
            state: State::Unasked,
            waiters: Waiters::default(),
            next: AtomicU64::new(0),
        }
    }

    pub fn show(&mut self, n: &Notification, handle: &AppHandle<RiverApp>) {
        if let State::Denied(at) = self.state
            && at.elapsed() >= ASK_AGAIN
        {
            self.state = State::Unasked;
        }
        if let State::Unasked = self.state {
            self.state = match notify_rust::check_bundle() {
                Err(e) => {
                    eprintln!("river: no notifications outside the River app bundle: {e}");
                    State::NoBundle
                }
                // The system asks the person the first time only; later calls return the
                // answer they gave.
                Ok(()) => match notify_rust::request_auth_blocking() {
                    Ok(true) => State::Ready,
                    Ok(false) => {
                        eprintln!("river: notifications are not allowed in System Settings");
                        State::Denied(Instant::now())
                    }
                    Err(e) => {
                        eprintln!("river: could not ask to show notifications: {e}");
                        State::Denied(Instant::now())
                    }
                },
            };
        }
        if !matches!(self.state, State::Ready) {
            return;
        }
        let mut note = notify_rust::Notification::new();
        // The same id replaces the room's earlier notification.
        note.summary(&n.title).body(&n.body).id(n.room_key.as_str());
        let shown = match note.show() {
            Ok(shown) => shown,
            Err(e) => {
                eprintln!("river: could not show a notification: {e}");
                return;
            }
        };
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.waiters
            .spawn(n.room_key.clone(), id, handle.clone(), move |on| {
                if let Err(e) = shown.wait_for_response(on) {
                    // Said once: a click on this notification will not be heard.
                    static SAID: std::sync::atomic::AtomicBool =
                        std::sync::atomic::AtomicBool::new(false);
                    if !SAID.swap(true, Ordering::Relaxed) {
                        eprintln!("river: lost track of a notification, so a click on it will not open its room: {e}");
                    }
                }
            });
    }
}
