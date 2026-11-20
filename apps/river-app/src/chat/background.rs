//! "Background and startup", the dialog that opens from the room list's footer in the desktop
//! app: whether closing the window keeps River running in the tray, and whether River starts
//! when you log in.
//!
//! What the settings do belongs to the shell (the tray icon, the login item); this only shows
//! them and passes a change on to the [`BackgroundHost`] it was given. A shell without one
//! (snapshots, tests, a future mobile app) shows no entry.

use std::sync::Arc;

use rune::El;
use rune_kit::{Button, ButtonVariant, Checkbox, Kit};

use super::dialog_kit::{self as dk, DialogSpec};
use super::style::{self as st};
use super::view::label;
use crate::RiverApp;

/// What the shell offers for running in the background. Every call is a small file or registry
/// access, made on the UI thread.
pub trait BackgroundHost: Send + Sync + 'static {
    /// Whether the system has a tray for River's icon. Without one, closing the window quits.
    fn tray_available(&self) -> bool;

    /// Whether closing the window keeps River running in the tray.
    fn close_to_tray(&self) -> bool;

    /// Change [`close_to_tray`](BackgroundHost::close_to_tray), keeping the choice.
    fn set_close_to_tray(&self, on: bool) -> Result<(), String>;

    /// Whether River is set to start when the user logs in.
    fn start_at_login(&self) -> bool;

    /// Turn the login item on or off, for this user only.
    fn set_start_at_login(&self, on: bool) -> Result<(), String>;
}

/// The dialog and what it shows.
pub struct BackgroundUi {
    host: Arc<dyn BackgroundHost>,
    /// Whether the dialog is open.
    pub open: bool,
    /// What the host last said, read when the dialog opens and after each change.
    pub tray: bool,
    pub close_to_tray: bool,
    pub start_at_login: bool,
    /// Why the last change did not happen.
    pub error: Option<String>,
}

impl BackgroundUi {
    pub fn new(host: Arc<dyn BackgroundHost>) -> Self {
        let mut ui = BackgroundUi {
            host,
            open: false,
            tray: false,
            close_to_tray: false,
            start_at_login: false,
            error: None,
        };
        ui.refresh();
        ui
    }

    fn refresh(&mut self) {
        self.tray = self.host.tray_available();
        self.close_to_tray = self.host.close_to_tray();
        self.start_at_login = self.host.start_at_login();
    }
}

impl super::Chat {
    /// Open "Background and startup", showing the settings as they are now.
    pub fn open_background(&mut self) {
        if let Some(b) = &mut self.background {
            b.refresh();
            b.error = None;
            b.open = true;
        }
    }

    pub fn close_background(&mut self) {
        if let Some(b) = &mut self.background {
            b.open = false;
        }
    }

    /// Flip "Keep River running when the window is closed".
    pub fn toggle_close_to_tray(&mut self) {
        if let Some(b) = &mut self.background {
            let want = !b.close_to_tray;
            b.error = b.host.set_close_to_tray(want).err();
            b.refresh();
        }
    }

    /// Flip "Start River when I log in".
    pub fn toggle_start_at_login(&mut self) {
        if let Some(b) = &mut self.background {
            let want = !b.start_at_login;
            b.error = b.host.set_start_at_login(want).err();
            b.refresh();
        }
    }
}

/// The dialog, when open.
pub(super) fn background_dialog(chat: &super::Chat, k: &Kit, layer: &mut Vec<El>) {
    let Some(b) = chat.background.as_ref().filter(|b| b.open) else {
        return;
    };
    let p = st::palette();
    let mut body = DialogSpec::new(
        "background",
        "Background and startup",
        |s: &mut RiverApp| s.chat.close_background(),
    )
    .body_key("background-body")
    .width(460.0);
    body = body.child(
        Checkbox::new(b.tray && b.close_to_tray)
            .label("Keep River running when the window is closed")
            .description(if b.tray {
                "River stays in the tray, so notifications keep coming."
            } else {
                "There is no tray here, so closing the window quits River."
            })
            .disabled(!b.tray)
            .on_toggle(|s: &mut RiverApp| s.chat.toggle_close_to_tray())
            .build(k)
            .key("background-close-to-tray"),
    );
    body = body.child(
        Checkbox::new(b.start_at_login)
            .label("Start River when I log in")
            .description(if b.tray {
                "River starts hidden in the tray."
            } else {
                "River starts with its window open: there is no tray."
            })
            .on_toggle(|s: &mut RiverApp| s.chat.toggle_start_at_login())
            .build(k)
            .key("background-start-at-login"),
    );
    body = body.child(
        label(
            if b.tray {
                "Quit River with the button below, Ctrl+Q, or the tray icon's menu. If the icon \
                 goes missing, starting River again shows its window."
            } else {
                "GNOME shows a tray only with its AppIndicator extension installed. Without a \
                 tray, closing the window quits River."
            },
            st::SMALL,
            p.text_muted,
        )
        .wrap()
        .w_full()
        .key("background-note"),
    );
    if let Some(e) = &b.error {
        body = body.child(
            label(e.clone(), st::SMALL, p.danger)
                .wrap()
                .w_full()
                .key("background-error"),
        );
    }
    // Quit River ends the app, so it is set apart at the left; Close is where Close always is.
    body.footer(dk::footer(
        vec![
            Button::new("Quit River")
                .variant(ButtonVariant::Ghost)
                .on_press_tx(|_: &mut RiverApp, tx: &mut rune::Tx| tx.quit())
                .build(k)
                .key("background-quit"),
        ],
        vec![dk::quiet_button(
            k,
            "Close",
            "background-close",
            |s: &mut RiverApp| s.chat.close_background(),
        )],
    ))
    .build(k, layer);
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::chat::{Chat, Source};

    /// A shell that remembers what it was told, and can refuse.
    pub(crate) struct Fake {
        pub tray: bool,
        pub state: Mutex<(bool, bool)>,
        pub refuse: bool,
    }

    impl Fake {
        pub fn new(tray: bool) -> Arc<Self> {
            Arc::new(Fake {
                tray,
                state: Mutex::new((true, false)),
                refuse: false,
            })
        }
    }

    impl BackgroundHost for Fake {
        fn tray_available(&self) -> bool {
            self.tray
        }
        fn close_to_tray(&self) -> bool {
            self.state.lock().unwrap().0
        }
        fn set_close_to_tray(&self, on: bool) -> Result<(), String> {
            self.state.lock().unwrap().0 = on;
            Ok(())
        }
        fn start_at_login(&self) -> bool {
            self.state.lock().unwrap().1
        }
        fn set_start_at_login(&self, on: bool) -> Result<(), String> {
            if self.refuse {
                return Err("no permission".into());
            }
            self.state.lock().unwrap().1 = on;
            Ok(())
        }
    }

    fn chat_with(host: Arc<Fake>) -> Chat {
        let mut chat = Chat::new(Source::Example, Vec::new(), 0, 0, 0);
        chat.background = Some(BackgroundUi::new(host));
        chat
    }

    #[test]
    fn the_dialog_shows_what_the_shell_says_and_passes_changes_on() {
        let host = Fake::new(true);
        let mut chat = chat_with(host.clone());
        chat.open_background();
        let b = chat.background.as_ref().unwrap();
        assert!(b.open && b.tray && b.close_to_tray && !b.start_at_login);

        chat.toggle_start_at_login();
        assert_eq!(*host.state.lock().unwrap(), (true, true));
        assert!(chat.background.as_ref().unwrap().start_at_login);

        chat.toggle_close_to_tray();
        assert_eq!(*host.state.lock().unwrap(), (false, true));
        assert!(!chat.background.as_ref().unwrap().close_to_tray);
        assert!(chat.background.as_ref().unwrap().error.is_none());
    }

    #[test]
    fn a_change_the_shell_refuses_shows_why_and_is_not_shown_as_done() {
        let host = Arc::new(Fake {
            tray: true,
            state: Mutex::new((true, false)),
            refuse: true,
        });
        let mut chat = chat_with(host);
        chat.open_background();
        chat.toggle_start_at_login();
        let b = chat.background.as_ref().unwrap();
        assert!(!b.start_at_login, "the box shows what the system says");
        assert_eq!(b.error.as_deref(), Some("no permission"));
        // Opening again starts clean.
        chat.close_background();
        chat.open_background();
        assert!(chat.background.as_ref().unwrap().error.is_none());
    }

    #[test]
    fn the_overview_and_the_background_dialog_do_not_open_over_each_other() {
        let mut chat = chat_with(Fake::new(true));
        chat.set_rooms(crate::chat::example::data().rooms);
        chat.open_background();
        assert!(
            !chat.toggle_overview(),
            "the overview does not open over it"
        );
        assert!(!chat.overview);
        chat.close_background();
        assert!(chat.toggle_overview());
        assert!(!chat.escape(), "Escape belongs to the overview's dialog");
        assert!(
            !chat.toggle_switcher(),
            "the switcher does not open over it"
        );
        assert!(chat.switcher.is_none());
        assert!(chat.toggle_overview(), "the same call closes it");
    }

    #[test]
    fn the_switcher_and_escape_leave_the_open_background_dialog_alone() {
        let mut chat = chat_with(Fake::new(true));
        chat.set_rooms(crate::chat::example::data().rooms);
        chat.open_background();
        assert!(
            !chat.toggle_switcher(),
            "the switcher does not open over it"
        );
        assert!(chat.switcher.is_none());
        assert!(!chat.escape(), "Escape belongs to the dialog");
        chat.close_background();
        assert!(chat.toggle_switcher());
    }

    #[test]
    fn without_a_shell_there_is_nothing_to_open() {
        let mut chat = Chat::new(Source::Example, Vec::new(), 0, 0, 0);
        chat.open_background();
        chat.toggle_close_to_tray();
        chat.toggle_start_at_login();
        assert!(chat.background.is_none());
    }

    #[test]
    fn the_search_panel_does_not_stay_open_under_this_dialog() {
        let mut chat = chat_with(Fake::new(true));
        chat.set_rooms(crate::chat::example::data().rooms);
        assert!(chat.search_chord());
        chat.open_background();
        assert!(
            !chat.search_chord(),
            "the chord is refused while it is open"
        );
        assert!(chat.search.is_none(), "and the panel is gone");
    }
}
