//! Linux and the BSDs: the freedesktop notification protocol, over D-Bus (notify-rust with
//! zbus, so no libdbus).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use notify_rust::{Hint, Timeout};
use river_app::RiverApp;
use river_app::notify::Notification;
use rune::AppHandle;

use super::{MAX_WAITING, escape_markup, is_click, open_room};

pub struct Backend {
    /// The server's id for each room's latest notification, so the next one replaces it
    /// instead of stacking.
    ids: HashMap<String, u32>,
    /// Whether the server reads the body as markup, asked once on the first notification.
    markup: Option<bool>,
    waiting: Arc<AtomicUsize>,
}

impl Backend {
    pub fn new() -> Self {
        Backend {
            ids: HashMap::new(),
            markup: None,
            waiting: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn show(&mut self, n: &Notification, handle: &AppHandle<RiverApp>) {
        let markup = *self.markup.get_or_insert_with(|| {
            notify_rust::get_capabilities()
                .map(|caps| caps.iter().any(|c| c == "body-markup"))
                .unwrap_or(false)
        });
        let body = if markup {
            escape_markup(&n.body)
        } else {
            n.body.clone()
        };
        let mut note = notify_rust::Notification::new();
        note.appname("River")
            .summary(&n.title)
            .body(&body)
            // The icon theme's name for it, which the .desktop file's `Icon=river` installs.
            .icon("river")
            // Lets the shell attribute the notification to River's launcher entry (name, icon,
            // per-app mute).
            .hint(Hint::DesktopEntry("river".into()))
            .hint(Hint::Category("im.received".into()))
            // The click on the notification itself.
            .action("default", "Open")
            .timeout(Timeout::Default);
        if let Some(id) = self.ids.get(&n.room_key) {
            note.id(*id);
        }
        let shown = match note.show() {
            Ok(shown) => shown,
            Err(e) => {
                eprintln!("river: could not show a notification: {e}");
                // The server may have restarted and forgotten the ids.
                self.ids.clear();
                self.markup = None;
                return;
            }
        };
        self.ids.insert(n.room_key.clone(), shown.id());
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
