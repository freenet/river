//! Linux and the BSDs: the freedesktop notification protocol, over D-Bus (notify-rust with
//! zbus, so no libdbus).

use std::collections::HashMap;

use notify_rust::{Hint, Timeout};
use river_app::RiverApp;
use river_app::notify::Notification;
use rune::AppHandle;

use super::{ACTION_MARK_READ, ACTION_REPLY, Waiters, escape_markup};

/// What a notification server told us it does.
#[derive(Clone, Copy)]
struct Caps {
    /// Reads markup in the text.
    markup: bool,
    /// Shows action buttons.
    buttons: bool,
}

impl Caps {
    /// When the server could not be asked: assume markup (so text is escaped) and no buttons.
    const UNKNOWN: Caps = Caps {
        markup: true,
        buttons: false,
    };

    fn from_list(list: &[String]) -> Caps {
        Caps {
            markup: list.iter().any(|c| c == "body-markup"),
            buttons: list.iter().any(|c| c == "actions"),
        }
    }
}

pub struct Backend {
    /// The server's id for each room's latest notification, so the next one replaces it
    /// instead of stacking.
    ids: HashMap<String, u32>,
    /// What the server reads and shows, asked until it answers.
    caps: Option<Caps>,
    waiters: Waiters,
}

impl Backend {
    pub fn new() -> Self {
        Backend {
            ids: HashMap::new(),
            caps: None,
            waiters: Waiters::default(),
        }
    }

    pub fn show(&mut self, n: &Notification, handle: &AppHandle<RiverApp>) {
        // Fail closed: when the server cannot be asked, escape, offer no buttons, and ask again
        // next time. A server that reads markup would otherwise render tags from a message.
        let caps = match self.caps {
            Some(caps) => caps,
            None => match notify_rust::get_capabilities() {
                Ok(list) => {
                    let caps = Caps::from_list(&list);
                    self.caps = Some(caps);
                    caps
                }
                Err(_) => Caps::UNKNOWN,
            },
        };
        let markup = caps.markup;
        // Some servers (dunst) read markup in the summary too, and the room name is chosen by
        // the room's owner: escape both exactly when the server reads markup.
        let (title, body) = if markup {
            (escape_markup(&n.title), escape_markup(&n.body))
        } else {
            (n.title.clone(), n.body.clone())
        };
        let mut note = notify_rust::Notification::new();
        note.appname("River")
            .summary(&title)
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
        // Buttons for a notification about one room, where the server shows them. The server
        // has no field to type a reply in (`inline-reply` is a KDE extension this crate does
        // not receive), so Reply opens the room with the composer ready.
        if n.actions && caps.buttons {
            if n.reply {
                note.action(ACTION_REPLY, "Reply");
            }
            note.action(ACTION_MARK_READ, "Mark as read");
        }
        if let Some(id) = self.ids.get(&n.room_key) {
            note.id(*id);
        }
        let shown = match note.show() {
            Ok(shown) => shown,
            Err(e) => {
                eprintln!("river: could not show a notification: {e}");
                // The server may have restarted and forgotten the ids.
                self.ids.clear();
                self.caps = None;
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
            n.message_key.clone(),
            u64::from(id),
            handle.clone(),
            move |on| {
                let _ = shown.wait_for_response(on);
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_come_from_the_servers_list() {
        let list = |names: &[&str]| names.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let c = Caps::from_list(&list(&["actions", "body", "body-markup"]));
        assert!(c.markup && c.buttons);
        let c = Caps::from_list(&list(&["body"]));
        assert!(!c.markup && !c.buttons);
        const { assert!(Caps::UNKNOWN.markup && !Caps::UNKNOWN.buttons) };
    }
}
