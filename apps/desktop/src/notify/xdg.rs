//! Linux and the BSDs: the freedesktop notification protocol, over D-Bus (notify-rust with
//! zbus, so no libdbus).

use std::collections::HashMap;

use notify_rust::{Hint, Timeout};
use river_app::RiverApp;
use river_app::notify::Notification;
use rune::AppHandle;

use super::{Waiters, escape_markup};

pub struct Backend {
    /// The server's id for each room's latest notification, so the next one replaces it
    /// instead of stacking.
    ids: HashMap<String, u32>,
    /// Whether the server reads the text as markup, asked until it answers.
    markup: Option<bool>,
    waiters: Waiters,
}

impl Backend {
    pub fn new() -> Self {
        Backend {
            ids: HashMap::new(),
            markup: None,
            waiters: Waiters::default(),
        }
    }

    pub fn show(&mut self, n: &Notification, handle: &AppHandle<RiverApp>) {
        // Fail closed: when the server cannot be asked, escape, and ask again next time. A
        // server that reads markup would otherwise render tags from a message.
        let markup = match self.markup {
            Some(m) => m,
            None => match notify_rust::get_capabilities() {
                Ok(caps) => {
                    let m = caps.iter().any(|c| c == "body-markup");
                    self.markup = Some(m);
                    m
                }
                Err(_) => true,
            },
        };
        // Only the body is markup; the summary is plain text by the specification.
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
        // A notification that replaced another has the same id. It gets a waiter of its own, so
        // its handle (and D-Bus connection) outlives the replaced one's; the older waiters end
        // when the server reports the id closed.
        let id = shown.id();
        self.ids.insert(n.room_key.clone(), id);
        self.waiters.spawn(
            n.room_key.clone(),
            u64::from(id),
            handle.clone(),
            move |on| {
                let _ = shown.wait_for_response(on);
            },
        );
    }
}
