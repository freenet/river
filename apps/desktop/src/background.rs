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
/// Exists from the first start on a computer (one with no config directory yet) until its setup
/// ends, so a first run interrupted before that is offered the login choice again.
const FIRST_RUN_FILE: &str = "first-run";
const KEY: &str = "close-to-tray";
/// Set once River has told the person that closing the window leaves it running.
const TOLD_KEY: &str = "close-to-tray-told";

/// The quit chord in the notice (`Mod+Q` in the app).
#[cfg(target_os = "macos")]
const QUIT_CHORD: &str = "Cmd+Q";
#[cfg(not(target_os = "macos"))]
const QUIT_CHORD: &str = "Ctrl+Q";

fn notice_body() -> String {
    format!(
        "River is still running. Open it or quit from the tray icon, or press {QUIT_CHORD} in \
         the window. Change this in Settings > Background and startup."
    )
}

/// Where the settings file is, for the notice's flag. Set by `DesktopBackground::new`.
static CONFIG_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
/// Whether the person has been told. Kept in the settings file; with no place to keep it, it
/// holds for this run only.
static TOLD: AtomicBool = AtomicBool::new(false);

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

/// Whether hiding the window should now say that River keeps running: when it hides and the
/// person has not been told yet. Quitting says nothing (nothing is left running).
pub fn should_tell(action: Close, told: bool) -> bool {
    action == Close::Hide && !told
}

/// `App::on_close`: the window is closing. River keeps running (the app is built to) unless
/// there is no tray to bring it back from, or the person asked for closing to quit.
pub fn on_close(s: &mut RiverApp, tx: &mut Tx) {
    match close_action(tray_up(), CLOSE_TO_TRAY.load(Ordering::Acquire)) {
        // A hidden window gets no focus-lost event, and a River that still thinks it has focus
        // would show no notification for a message that arrives.
        Close::Hide => {
            s.focus_changed(false);
            tell_once(s);
        }
        Close::Quit => tx.quit(),
    }
}

/// Show the notice the first time closing hides the window. The flag is set when the notifier
/// accepted the notice for showing, which is not proof it was shown (macOS may be refusing
/// permission; the update notice has the same limit). A notifier that could not even queue it
/// leaves the flag unset, so the next hide tries again. Failing to keep the flag is not an
/// error: the person may see the notice once more.
///
/// Runs on the UI thread, as does `set_close_to_tray` (the settings switch), so reading the
/// choice here cannot race a change of it.
fn tell_once(s: &mut RiverApp) {
    tell_with(
        |title, body| s.send_notice(title, body),
        &TOLD,
        CONFIG_DIR.get().map(PathBuf::as_path),
        CLOSE_TO_TRAY.load(Ordering::Acquire),
    );
}

fn tell_with(
    send: impl FnOnce(String, String) -> bool,
    told: &AtomicBool,
    dir: Option<&Path>,
    close_to_tray: bool,
) {
    if !should_tell(Close::Hide, told.load(Ordering::Acquire)) {
        return;
    }
    if send("River is still running".into(), notice_body()) {
        told.store(true, Ordering::Release);
        // The choice as it is now, so an earlier failed save is not reverted by this one.
        if let Some(dir) = dir
            && let Err(e) = save(dir, close_to_tray, true)
        {
            river_app::diag!("river: couldn't remember that the close notice was shown: {e}");
        }
    }
}

fn has_value(text: &str, key: &str, is: impl Fn(&str) -> bool) -> bool {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .any(|(k, v)| k.trim() == key && is(v.trim()))
}

/// The saved choice, on when there is none.
pub fn load(dir: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(dir.join(FILE)) else {
        return true;
    };
    !has_value(&text, KEY, |v| matches!(v, "off" | "false" | "no" | "0"))
}

/// Whether the person has been told that closing leaves River running; false when unknown.
pub fn load_told(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join(FILE))
        .is_ok_and(|t| has_value(&t, TOLD_KEY, |v| matches!(v, "on" | "true" | "yes" | "1")))
}

/// Change the choice, keeping whether the person has been told.
fn save_choice(dir: &Path, on: bool) -> Result<(), String> {
    save(dir, on, load_told(dir))
}

fn save(dir: &Path, on: bool, told: bool) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
    let path = dir.join(FILE);
    let tmp = dir.join(format!(".{FILE}.{}.tmp", std::process::id()));
    // Lines this version does not know are kept as they were.
    let mut text: String = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(
            |l| !matches!(l.split_once('='), Some((k, _)) if matches!(k.trim(), KEY | TOLD_KEY)),
        )
        .map(|l| format!("{l}\n"))
        .collect();
    text.push_str(&format!("{KEY}={}\n", if on { "on" } else { "off" }));
    if told {
        text.push_str(&format!("{TOLD_KEY}=on\n"));
    }
    std::fs::write(&tmp, text)
        .and_then(|()| std::fs::rename(&tmp, &path))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("couldn't save the setting: {e}")
        })
}

/// Decide whether this start is River's first on this computer, before anything else writes to
/// the config directory: a missing or empty directory is, and gets the marker that says so
/// until setup ends. A directory with anything in it (the lock file every earlier start left,
/// the settings) is not, whatever else it lacks, so nobody who has run River gets a login item
/// written for them.
fn note_first_run(dir: &Path) {
    let new = match std::fs::read_dir(dir) {
        Ok(mut entries) => entries.next().is_none(),
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    };
    if new
        && let Err(e) = std::fs::create_dir_all(dir).and_then(|()| {
            std::fs::write(dir.join(FIRST_RUN_FILE), "setup has not finished\n")
        })
    {
        // Not a first run then, as far as anyone can tell: nothing is written for the person.
        river_app::diag!("river: couldn't note the first run: {e}");
    }
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
            note_first_run(d);
            CLOSE_TO_TRAY.store(load(d), Ordering::Release);
            TOLD.store(load_told(d), Ordering::Release);
            let _ = CONFIG_DIR.set(d.clone());
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
            Some(d) => save_choice(d, on),
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

    fn first_run(&self) -> bool {
        self.login.is_some()
            && self
                .dir
                .as_ref()
                .is_some_and(|d| d.join(FIRST_RUN_FILE).exists())
    }

    fn first_run_done(&self) {
        if let Some(d) = &self.dir
            && let Err(e) = std::fs::remove_file(d.join(FIRST_RUN_FILE))
            && e.kind() != std::io::ErrorKind::NotFound
        {
            river_app::diag!("river: couldn't clear the first-run note: {e}");
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
    fn only_a_missing_or_empty_config_directory_is_a_first_run() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("new");
        note_first_run(&missing);
        assert!(missing.join(FIRST_RUN_FILE).exists(), "a missing directory");
        let empty = root.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        note_first_run(&empty);
        assert!(empty.join(FIRST_RUN_FILE).exists(), "an empty directory");
        // An earlier run left its lock file: not a first run, whatever else is missing.
        let old = root.path().join("old");
        std::fs::create_dir(&old).unwrap();
        std::fs::write(old.join("river.lock"), "").unwrap();
        note_first_run(&old);
        assert!(!old.join(FIRST_RUN_FILE).exists());
    }

    #[test]
    fn a_first_run_that_did_not_finish_is_offered_again_and_a_finished_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("river");
        let host = DesktopBackground {
            dir: Some(d.clone()),
            login: LoginItem::for_this_user(),
        };
        note_first_run(&d);
        assert_eq!(host.first_run(), host.login.is_some());
        // Started again before setup ended: the marker is the only thing in the directory.
        note_first_run(&d);
        assert_eq!(host.first_run(), host.login.is_some());
        host.first_run_done();
        assert!(!host.first_run());
        // The directory now has a file in it from the next start on, but the marker is gone.
        std::fs::write(d.join("river.lock"), "").unwrap();
        note_first_run(&d);
        assert!(!host.first_run());
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
        save_choice(dir.path(), false).unwrap();
        assert!(!load(dir.path()));
        save_choice(dir.path(), true).unwrap();
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
        save_choice(dir.path(), false).unwrap();
        save(dir.path(), true, true).unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn the_notice_is_said_when_hiding_and_not_yet_told_only() {
        assert!(should_tell(Close::Hide, false));
        assert!(!should_tell(Close::Hide, true), "once per install");
        assert!(
            !should_tell(Close::Quit, false),
            "quitting leaves nothing running"
        );
        assert!(!should_tell(Close::Quit, true));
    }

    #[test]
    fn a_notice_that_could_not_be_queued_leaves_told_unset_and_a_queued_one_sets_it() {
        let dir = tempfile::tempdir().unwrap();
        let told = AtomicBool::new(false);
        tell_with(|_, _| false, &told, Some(dir.path()), true);
        assert!(!told.load(Ordering::Acquire));
        assert!(!load_told(dir.path()), "nothing kept");

        let mut sent = 0;
        tell_with(
            |_, _| {
                sent += 1;
                true
            },
            &told,
            Some(dir.path()),
            false,
        );
        assert_eq!(sent, 1);
        assert!(told.load(Ordering::Acquire));
        assert!(load_told(dir.path()));
        assert!(!load(dir.path()), "the choice in effect is what was saved");

        tell_with(|_, _| panic!("said twice"), &told, Some(dir.path()), false);
    }

    #[test]
    fn saving_keeps_lines_it_does_not_know() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE), "future=1\nclose-to-tray=on\n").unwrap();
        save_choice(dir.path(), false).unwrap();
        let text = std::fs::read_to_string(dir.path().join(FILE)).unwrap();
        assert!(text.contains("future=1\n"), "{text}");
        assert!(!load(dir.path()));
        save(dir.path(), false, true).unwrap();
        save(dir.path(), true, true).unwrap();
        let text = std::fs::read_to_string(dir.path().join(FILE)).unwrap();
        for key in ["close-to-tray=", "close-to-tray-told="] {
            let n = text.lines().filter(|l| l.starts_with(key)).count();
            assert_eq!(n, 1, "one {key} line: {text}");
        }
        assert!(text.contains("future=1\n"));
    }

    #[test]
    fn the_told_flag_is_kept_beside_the_choice_and_neither_loses_the_other() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!load_told(dir.path()), "no file: not told");
        save_choice(dir.path(), false).unwrap();
        assert!(!load_told(dir.path()));
        save(dir.path(), load(dir.path()), true).unwrap();
        assert!(load_told(dir.path()));
        assert!(!load(dir.path()), "telling does not change the choice");
        save_choice(dir.path(), true).unwrap();
        assert!(load(dir.path()));
        assert!(
            load_told(dir.path()),
            "changing the choice keeps the telling"
        );
    }

    #[test]
    fn the_notice_names_the_quit_chord_and_the_settings_section() {
        let body = notice_body();
        assert!(body.contains("still running"));
        assert!(body.contains(QUIT_CHORD));
        assert!(body.contains("Settings > Background and startup"));
    }
}
