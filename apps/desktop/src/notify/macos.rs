//! macOS: `UNUserNotificationCenter`, which needs River's signed `.app` bundle (its bundle
//! identifier is `org.freenet.River`, see `packaging/macos/Info.plist`). Run outside the
//! bundle (`cargo run`) there is no bundle identifier and nothing is shown.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use river_app::RiverApp;
use river_app::notify::Notification;
use rune::AppHandle;

use super::{ACTION_MARK_READ, Waiters};

pub struct Backend {
    state: State,
    waiters: Waiters,
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
                    river_app::diag!("river: no notifications outside the River app bundle: {e}");
                    State::NoBundle
                }
                // The system asks the person the first time only; later calls return the
                // answer they gave.
                Ok(()) => match notify_rust::request_auth_blocking() {
                    Ok(true) => State::Ready,
                    Ok(false) => {
                        river_app::diag!("river: notifications are not allowed in System Settings");
                        State::Denied(Instant::now())
                    }
                    Err(e) => {
                        river_app::diag!("river: could not ask to show notifications: {e}");
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
        if n.actions {
            // The system's own reply field (typed text arrives as `Reply`), and a button.
            if n.reply {
                note.reply("Reply");
            }
            note.action(ACTION_MARK_READ, "Mark as read");
        }
        let shown = match note.show() {
            Ok(shown) => shown,
            Err(e) => {
                river_app::diag!("river: could not show a notification: {e}");
                return;
            }
        };
        // The system replaces the room's earlier notification (same identifier), so they share
        // an id here too: a click then acts once, on the message showing now.
        let id = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            n.room_key.hash(&mut h);
            h.finish()
        };
        let waiters = self.waiters.clone();
        let room = n.room_key.clone();
        self.waiters
            .spawn(n.room_key.clone(), n.message_key.clone(), id, handle.clone(), move |on| {
                if let Err(e) = shown.wait_for_response(on)
                    // The system keeps one response sender per identifier, so a notification
                    // that replaced this one took it over and this wait ends: expected, and
                    // the newer waiter hears the click.
                    && !waiters.replaced(&room, id)
                {
                    // Said once: a click on this notification will not be heard.
                    static SAID: std::sync::atomic::AtomicBool =
                        std::sync::atomic::AtomicBool::new(false);
                    if !SAID.swap(true, Ordering::Relaxed) {
                        river_app::diag!("river: lost track of a notification, so a click on it will not open its room: {e}");
                    }
                }
            });
    }
}
