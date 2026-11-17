//! macOS: `UNUserNotificationCenter`, which needs River's signed `.app` bundle (its bundle
//! identifier is `org.freenet.River`, see `packaging/macos/Info.plist`). Run outside the
//! bundle (`cargo run`) there is no bundle identifier and nothing is shown.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use river_app::RiverApp;
use river_app::notify::Notification;
use rune::AppHandle;

use super::{MAX_WAITING, is_click, open_room};

pub struct Backend {
    state: State,
    waiting: Arc<AtomicUsize>,
}

enum State {
    /// Not asked yet: the first notification checks the bundle and asks permission.
    Unasked,
    Ready,
    /// Not in a bundle, or the person said no. Said once, then silent.
    Off,
}

impl Backend {
    pub fn new() -> Self {
        Backend {
            state: State::Unasked,
            waiting: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn show(&mut self, n: &Notification, handle: &AppHandle<RiverApp>) {
        if let State::Unasked = self.state {
            self.state = match notify_rust::check_bundle() {
                Err(e) => {
                    eprintln!("river: no notifications outside the River app bundle: {e}");
                    State::Off
                }
                // The system asks the person the first time only; later calls return the
                // answer they gave.
                Ok(()) => match notify_rust::request_auth_blocking() {
                    Ok(true) => State::Ready,
                    Ok(false) => {
                        eprintln!("river: notifications are not allowed in System Settings");
                        State::Off
                    }
                    Err(e) => {
                        eprintln!("river: could not ask to show notifications: {e}");
                        State::Off
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
        if self.waiting.fetch_add(1, Ordering::AcqRel) >= MAX_WAITING {
            self.waiting.fetch_sub(1, Ordering::AcqRel);
            return;
        }
        let (waiting, handle, room) = (self.waiting.clone(), handle.clone(), n.room_key.clone());
        let spawned = std::thread::Builder::new()
            .name("river-notify-click".into())
            .spawn(move || {
                let _ = shown.wait_for_response(|response: &notify_rust::NotificationResponse| {
                    if is_click(response) {
                        open_room(&handle, room);
                    }
                });
                waiting.fetch_sub(1, Ordering::AcqRel);
            });
        if let Err(e) = spawned {
            self.waiting.fetch_sub(1, Ordering::AcqRel);
            eprintln!("river: clicking a notification will not open its room: {e}");
        }
    }
}
