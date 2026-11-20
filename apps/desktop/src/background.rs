//! Running in the background: what closing the window does, and the settings behind
//! "Background and startup" (`river_app::chat::BackgroundHost`).
//!
//! With a tray icon up, closing the window hides it and River keeps running, so notifications
//! keep arriving. Without one, closing quits as it always did. "Keep River running when the
//! window is closed" can switch that off for people who want closing to mean quitting; it is
//! kept in a small file in the config directory. Whether River starts at login is not kept
//! here: the login item itself (`autostart`) is the record.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use river_app::RiverApp;
use river_app::chat::BackgroundHost;
use rune::Tx;

use crate::autostart::LoginItem;

const FILE: &str = "background";
const KEY: &str = "close-to-tray";

/// Whether the tray icon is showing right now.
static TRAY_UP: AtomicBool = AtomicBool::new(false);
/// The person's choice: keep running when the window closes. On unless they turned it off.
static CLOSE_TO_TRAY: AtomicBool = AtomicBool::new(true);

/// The tray came up (or went away: some desktops lose their tray while River runs).
pub fn set_tray_up(up: bool) {
    TRAY_UP.store(up, Ordering::Release);
}

pub fn tray_up() -> bool {
    TRAY_UP.load(Ordering::Acquire)
}

/// What closing the window does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Close {
    /// Hide the window and keep running.
    Hide,
    Quit,
}

/// Closing hides the window only when there is a tray to bring it back from and the person
/// has not asked for closing to quit.
pub fn close_action(tray_up: bool, close_to_tray: bool) -> Close {
    if tray_up && close_to_tray {
        Close::Hide
    } else {
        Close::Quit
    }
}

/// `App::on_close`: the window is closing. River keeps running (the app is built to) unless
/// there is no tray to bring it back from, or the person asked for closing to quit.
pub fn on_close(s: &mut RiverApp, tx: &mut Tx) {
    match close_action(tray_up(), CLOSE_TO_TRAY.load(Ordering::Acquire)) {
        // A hidden window gets no focus-lost event, and a River that still thinks it has focus
        // would show no notification for a message that arrives.
        Close::Hide => s.focus_changed(false),
        Close::Quit => tx.quit(),
    }
}

/// The saved choice, on when there is none.
pub fn load(dir: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(dir.join(FILE)) else {
        return true;
    };
    !text
        .lines()
        .filter_map(|l| l.split_once('='))
        .any(|(k, v)| k.trim() == KEY && matches!(v.trim(), "off" | "false" | "no" | "0"))
}

fn save(dir: &Path, on: bool) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
    let path = dir.join(FILE);
    let tmp = dir.join(format!(".{FILE}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, format!("{KEY}={}\n", if on { "on" } else { "off" }))
        .and_then(|()| std::fs::rename(&tmp, &path))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("couldn't save the setting: {e}")
        })
}

/// The desktop's answer to the settings dialog.
pub struct DesktopBackground {
    dir: Option<PathBuf>,
    login: Option<LoginItem>,
}

impl DesktopBackground {
    /// Read the saved choice into the running app's.
    pub fn new(dir: Option<PathBuf>) -> Self {
        if let Some(d) = &dir {
            CLOSE_TO_TRAY.store(load(d), Ordering::Release);
        }
        DesktopBackground {
            dir,
            login: LoginItem::for_this_user(),
        }
    }
}

impl DesktopBackground {
    /// Point a login item that is on at this program (see `LoginItem::refresh`).
    pub fn refresh_login(&self) {
        if let Some(l) = &self.login {
            l.refresh();
        }
    }
}

impl BackgroundHost for DesktopBackground {
    fn tray_available(&self) -> bool {
        tray_up()
    }

    fn close_to_tray(&self) -> bool {
        CLOSE_TO_TRAY.load(Ordering::Acquire)
    }

    fn set_close_to_tray(&self, on: bool) -> Result<(), String> {
        // In effect for this run even when it cannot be kept for the next.
        CLOSE_TO_TRAY.store(on, Ordering::Release);
        match &self.dir {
            Some(d) => save(d, on),
            None => {
                Err("River has no place to keep settings, so this lasts until it quits.".into())
            }
        }
    }

    fn start_at_login(&self) -> bool {
        self.login.as_ref().is_some_and(LoginItem::enabled)
    }

    fn set_start_at_login(&self, on: bool) -> Result<(), String> {
        match &self.login {
            Some(l) => l.set(on),
            None => Err("This computer has no place for River's login item.".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `on_close` cannot be run without a window system, so pin that hiding reports the lost
    /// focus (a hidden window sends no event, and a River that thinks it is focused shows no
    /// notifications) and that quitting is the other arm.
    #[test]
    fn hiding_the_window_reports_it_unfocused() {
        let src = include_str!("background.rs").replace("\r\n", "\n");
        let body = &src[src.find("pub fn on_close").unwrap()..];
        let hide = body.find("Close::Hide =>").unwrap();
        let quit = body.find("Close::Quit =>").unwrap();
        assert!(hide < quit);
        assert!(body[hide..quit].contains("s.focus_changed(false)"));
        assert!(body[quit..].starts_with("Close::Quit => tx.quit()"));
    }

    #[test]
    fn closing_hides_only_with_a_tray_and_the_setting_on() {
        for (tray, setting, want) in [
            (true, true, Close::Hide),
            (true, false, Close::Quit),
            (false, true, Close::Quit),
            (false, false, Close::Quit),
        ] {
            assert_eq!(
                close_action(tray, setting),
                want,
                "tray {tray}, setting {setting}"
            );
        }
    }

    #[test]
    fn a_setting_without_a_config_directory_applies_now_and_says_it_is_not_kept() {
        let host = DesktopBackground::new(None);
        let err = host.set_close_to_tray(false).unwrap_err();
        assert!(err.contains("until it quits"), "{err}");
        assert!(!host.close_to_tray(), "in effect for this run");
        host.set_close_to_tray(true).unwrap_err();
        assert!(host.close_to_tray());
    }

    #[test]
    fn closing_to_the_tray_is_on_until_the_person_turns_it_off() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()), "no file: on");
        save(dir.path(), false).unwrap();
        assert!(!load(dir.path()));
        save(dir.path(), true).unwrap();
        assert!(load(dir.path()));
    }

    #[test]
    fn an_unreadable_or_unknown_file_leaves_it_on() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE), "nonsense\nother=off\n\u{0}").unwrap();
        assert!(load(dir.path()));
        std::fs::write(dir.path().join(FILE), "close-to-tray = off\n").unwrap();
        assert!(!load(dir.path()), "spaces around = are fine");
    }

    #[test]
    fn saving_leaves_one_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), false).unwrap();
        save(dir.path(), true).unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
