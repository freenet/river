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

/// Whether closing the window hides it instead of quitting.
pub fn close_hides() -> bool {
    tray_up() && CLOSE_TO_TRAY.load(Ordering::Acquire)
}

/// `App::on_close`: the window is closing. River keeps running (the app is built to) unless
/// there is no tray to bring it back from, or the person asked for closing to quit.
pub fn on_close(_: &mut RiverApp, tx: &mut Tx) {
    if !close_hides() {
        tx.quit();
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
