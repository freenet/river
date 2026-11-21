//! "Background and startup", a section of Settings in the desktop app: whether closing the
//! window keeps River running in the tray, whether River starts when you log in, and a way to
//! quit.
//!
//! What the settings do belongs to the shell (the tray icon, the login item); this only shows
//! them and passes a change on to the [`BackgroundHost`] it was given. A shell without one
//! (snapshots, tests, a future mobile app) has no such section. Where the desktop has no tray,
//! the "keep running" switch is left out and a sentence says why closing the window quits.

use std::sync::Arc;

use rune::{El, col};
use rune_kit::{Kit, Switch};

use super::dialog_kit as dk;
use super::settings::{section_rows, setting_row};
use super::style::{self as st, Palette};
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

/// The section and what it shows.
pub struct BackgroundUi {
    host: Arc<dyn BackgroundHost>,
    /// Whether the section is on screen. Kept in step by `Chat::track_settings_sections`.
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

    /// The section came on screen: read the shell again and forget an old error.
    pub(super) fn opened(&mut self) {
        self.refresh();
        self.error = None;
        self.open = true;
    }

    fn refresh(&mut self) {
        self.tray = self.host.tray_available();
        self.close_to_tray = self.host.close_to_tray();
        self.start_at_login = self.host.start_at_login();
    }
}

impl super::Chat {
    /// Open Settings on "Background and startup". Does nothing without a shell that offers it,
    /// or while another dialog is open.
    pub fn open_background(&mut self) {
        if self.background.is_some() && self.open_settings() {
            self.settings.select(super::settings::Section::Background);
            self.track_settings_sections();
        }
    }

    /// Close Settings, which is where the section is.
    pub fn close_background(&mut self) {
        self.close_settings();
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

/// The section's body. Empty without a shell that offers it (the list does not show it then).
pub(super) fn background_section(chat: &super::Chat, p: &Palette, k: &Kit) -> El {
    let Some(b) = chat.background.as_ref() else {
        return col().key("settings-background");
    };
    let mut rows = Vec::new();
    if b.tray {
        rows.push(setting_row(
            "background-close-to-tray-row",
            label(
                "Keep River running when the window is closed",
                st::BODY,
                p.text,
            ),
            Some(note(
                "River stays in the tray, so notifications keep coming.",
                "background-close-to-tray-note",
                p,
            )),
            Switch::new(b.close_to_tray)
                .label("Keep River running when the window is closed")
                .hide_label()
                .on_toggle(|s: &mut RiverApp| s.chat.toggle_close_to_tray())
                .build(k)
                .key("background-close-to-tray"),
            p,
        ));
    }
    rows.push(setting_row(
        "background-start-at-login-row",
        label("Start River when I log in", st::BODY, p.text),
        Some(note(
            if b.tray {
                "River starts hidden in the tray."
            } else {
                "River starts with its window open: there is no tray."
            },
            "background-start-at-login-note",
            p,
        )),
        Switch::new(b.start_at_login)
            .label("Start River when I log in")
            .hide_label()
            .on_toggle(|s: &mut RiverApp| s.chat.toggle_start_at_login())
            .build(k)
            .key("background-start-at-login"),
        p,
    ));
    let mut s = col()
        .key("settings-background")
        .w_full()
        .gap(14.0)
        .child(section_rows("settings-background-rows", rows, p));
    if let Some(e) = &b.error {
        s = s.child(
            label(e.clone(), st::SMALL, p.danger)
                .wrap()
                .w_full()
                .key("background-error"),
        );
    }
    let quit_help = if b.tray {
        format!(
            "Quit River with this button, {}, or the tray icon's menu. If the icon goes missing, \
             starting River again shows its window.",
            super::shortcuts::shown("Mod+Q", cfg!(target_os = "macos"))
        )
    } else {
        "GNOME shows a tray only with its AppIndicator extension installed. Without a tray, \
         closing the window quits River."
            .to_owned()
    };
    // Quit River ends the app, so it is set apart from the switches and drawn as a danger button.
    s.child(
        col()
            .key("background-quit-group")
            .w_full()
            .gap(8.0)
            .child(
                label(quit_help, st::SMALL, p.text_muted)
                    .wrap()
                    .w_full()
                    .key("background-note"),
            )
            .child(rune::row().child(dk::danger_outline_button_tx(
                k,
                "Quit River",
                "background-quit",
                |_: &mut RiverApp, tx: &mut rune::Tx| tx.quit(),
            ))),
    )
}

fn note(text: &str, key: &'static str, p: &Palette) -> El {
    label(text.to_owned(), st::SMALL, p.text_muted)
        .wrap()
        .w_full()
        .key(key)
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
