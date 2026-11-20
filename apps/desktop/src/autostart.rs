//! Start River when the user logs in, for this user only (no administrator rights).
//!
//! Linux writes an XDG autostart entry, macOS a LaunchAgent, Windows a value under the user's
//! `Run` key. All three start `river --background`, which opens no window when the tray is
//! there to bring it back from (see `background`).
//!
//! A macOS LaunchAgent is read by launchd at login, so turning it on or off takes effect at the
//! next login, not at once. Linux and Windows read their entries at login too, which is when
//! it matters.
//!
//! The file formats are built by plain functions that take the paths, so tests check what is
//! written without touching the real login items.

#![cfg_attr(windows, allow(dead_code))]

use std::path::{Path, PathBuf};

/// The argument the login item passes: start in the tray, with no window.
pub const BACKGROUND_FLAG: &str = "--background";

/// The name of the autostart entry (the file's stem, the registry value).
const NAME: &str = "river";
/// The LaunchAgent's label, River's bundle identifier.
const LABEL: &str = "org.freenet.River";

/// Where and how River's login item is kept on this system.
pub struct LoginItem {
    exe: PathBuf,
    place: Place,
}

enum Place {
    /// A file, with the text to put in it for the executable.
    File {
        path: PathBuf,
        body: fn(&Path) -> String,
        /// Whether the system would run the file's text at login.
        on: fn(&str) -> bool,
    },
    #[cfg(windows)]
    Registry,
}

impl LoginItem {
    /// The login item for this user and the running program, or `None` where the system has
    /// no place for one (no home directory).
    pub fn for_this_user() -> Option<Self> {
        let exe = own_executable()?;
        let base = directories::BaseDirs::new()?;
        #[cfg(windows)]
        {
            let _ = base;
            Some(LoginItem {
                exe,
                place: Place::Registry,
            })
        }
        #[cfg(not(windows))]
        {
            let place = file_place(std::env::consts::OS, base.config_dir(), base.home_dir())?;
            Some(LoginItem { exe, place })
        }
    }

    /// Whether the system would start River at login: the entry exists and is not switched
    /// off (a desktop's own startup settings tool does that by editing the entry).
    pub fn enabled(&self) -> bool {
        match &self.place {
            Place::File { path, on, .. } => std::fs::read_to_string(path).is_ok_and(|t| on(&t)),
            #[cfg(windows)]
            Place::Registry => registry::get(registry::VALUE).is_some(),
        }
    }

    /// Turn the login item on, or off (also when it was never on).
    pub fn set(&self, on: bool) -> Result<(), String> {
        match &self.place {
            Place::File { path, body, .. } => {
                if on {
                    check_exe(&self.exe)?;
                    write_file(path, &body(&self.exe))
                } else {
                    match std::fs::remove_file(path) {
                        Ok(()) => Ok(()),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                        Err(e) => Err(format!("couldn't remove {}: {e}", path.display())),
                    }
                }
            }
            #[cfg(windows)]
            Place::Registry => {
                if on {
                    check_exe(&self.exe)?;
                    registry::set(registry::VALUE, &run_command(&self.exe))
                } else {
                    registry::remove(registry::VALUE)
                }
            }
        }
    }

    /// Point an item that is on at the program running now. River moves (an update, a new
    /// install folder), and an entry left at the old path would start nothing. An item that
    /// is off, or already right, is left alone.
    pub fn refresh(&self) {
        if !self.enabled() || check_exe(&self.exe).is_err() {
            return;
        }
        let current = match &self.place {
            Place::File { path, .. } => std::fs::read_to_string(path).ok(),
            #[cfg(windows)]
            Place::Registry => registry::get(registry::VALUE),
        };
        let wanted = match &self.place {
            Place::File { body, .. } => body(&self.exe),
            #[cfg(windows)]
            Place::Registry => run_command(&self.exe),
        };
        if current.as_deref() != Some(wanted.as_str()) {
            let _ = self.set(true);
        }
    }
}

/// Refuse a path no login item should name: one the kernel reports as deleted (a running copy
/// that was replaced on disk) or with control characters, which cannot be written safely.
fn check_exe(exe: &Path) -> Result<(), String> {
    let text = exe.to_string_lossy();
    if text.ends_with(" (deleted)") {
        return Err(
            "River's program file was replaced while it runs. Restart River, then try again."
                .into(),
        );
    }
    if text.chars().any(char::is_control) {
        return Err("River's program path has characters a login item cannot hold.".into());
    }
    Ok(())
}

/// Whether a desktop entry would run: not `Hidden=true`, and not switched off with
/// `X-GNOME-Autostart-enabled=false`.
pub fn entry_enabled(text: &str) -> bool {
    !text.lines().any(|l| {
        let Some((key, value)) = l.split_once('=') else {
            return false;
        };
        let value = value.trim().to_ascii_lowercase();
        match key.trim() {
            "Hidden" => value == "true",
            "X-GNOME-Autostart-enabled" => value == "false",
            _ => false,
        }
    })
}

/// The program to start: the AppImage when River runs from one (its temporary mount is gone
/// by the next login), else this executable.
fn own_executable() -> Option<PathBuf> {
    if let Some(image) = std::env::var_os("APPIMAGE").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(image));
    }
    std::env::current_exe().ok()
}

/// Where the login item's file goes on `os`, if it is a file there.
fn file_place(os: &str, config_home: &Path, home: &Path) -> Option<Place> {
    match os {
        "macos" => Some(Place::File {
            path: home
                .join("Library")
                .join("LaunchAgents")
                .join(format!("{LABEL}.plist")),
            body: launch_agent,
            on: |_| true,
        }),
        "windows" => None,
        _ => Some(Place::File {
            path: config_home
                .join("autostart")
                .join(format!("{NAME}.desktop")),
            body: desktop_entry,
            on: entry_enabled,
        }),
    }
}

/// The file written whole or not at all, in a directory made if need be.
fn write_file(path: &Path, text: &str) -> Result<(), String> {
    let dir = path.parent().ok_or("bad login item path")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
    let tmp = dir.join(format!(".river-login.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text)
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("couldn't write {}: {e}", path.display())
        })
}

/// A path as one argument of a desktop entry's `Exec` line: in double quotes, with the
/// characters the spec reserves escaped, and `%` doubled so it is not a field code.
fn exec_quoted(path: &str) -> String {
    let mut out = String::from("\"");
    for c in path.chars() {
        match c {
            '\\' => out.push_str("\\\\\\\\"),
            '"' | '`' | '$' => {
                out.push_str("\\\\");
                out.push(c);
            }
            '%' => out.push_str("%%"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The XDG autostart entry (https://specifications.freedesktop.org/autostart-spec/).
pub fn desktop_entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=River\n\
         Comment=Decentralized group chat on Freenet\n\
         Exec={} {BACKGROUND_FLAG}\n\
         Icon=river\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n",
        exec_quoted(&exe.to_string_lossy())
    )
}

fn xml_escaped(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The macOS LaunchAgent, run once when the user logs in.
pub fn launch_agent(exe: &Path) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>{LABEL}</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n\
         \t\t<string>{}</string>\n\
         \t\t<string>{BACKGROUND_FLAG}</string>\n\
         \t</array>\n\
         \t<key>RunAtLoad</key>\n\
         \t<true/>\n\
         </dict>\n\
         </plist>\n",
        xml_escaped(&exe.to_string_lossy())
    )
}

/// The command under the Windows `Run` key: the path quoted, since it may hold spaces.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn run_command(exe: &Path) -> String {
    format!("\"{}\" {BACKGROUND_FLAG}", exe.display())
}

#[cfg(windows)]
mod registry {
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ, RegCloseKey,
        RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    };

    const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
    pub const VALUE: &str = "River";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn open(access: u32) -> Result<HKEY, u32> {
        let subkey = wide(RUN_KEY);
        let mut key: HKEY = std::ptr::null_mut();
        // SAFETY: `subkey` is NUL-terminated and outlives the call; `key` is a valid out pointer.
        let status =
            unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, access, &mut key) };
        if status == ERROR_SUCCESS {
            Ok(key)
        } else {
            Err(status)
        }
    }

    /// The value's text, or `None` when there is none (or it is not a string).
    pub fn get(name: &str) -> Option<String> {
        let key = open(KEY_QUERY_VALUE).ok()?;
        let name = wide(name);
        let mut kind = 0u32;
        let mut buf = [0u16; 2048];
        let mut bytes = std::mem::size_of_val(&buf) as u32;
        // SAFETY: `name` is NUL-terminated; `buf` and `bytes` describe the same writable
        // memory, which outlives the call; `kind` is a valid out pointer.
        let status = unsafe {
            RegQueryValueExW(
                key,
                name.as_ptr(),
                std::ptr::null(),
                &mut kind,
                buf.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        // SAFETY: `key` was opened above and is closed once.
        unsafe { RegCloseKey(key) };
        if status != ERROR_SUCCESS || kind != REG_SZ {
            return None;
        }
        let units = (bytes as usize / 2).min(buf.len());
        let text = &buf[..units];
        let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
        Some(String::from_utf16_lossy(&text[..end]))
    }

    pub fn set(name: &str, command: &str) -> Result<(), String> {
        let key = open(KEY_SET_VALUE)
            .map_err(|e| format!("couldn't open the Run key (registry error {e})"))?;
        let (name, value) = (wide(name), wide(command));
        // SAFETY: both strings are NUL-terminated and outlive the call; the length counts the
        // value's bytes including its terminator, as REG_SZ requires.
        let status = unsafe {
            RegSetValueExW(
                key,
                name.as_ptr(),
                0,
                REG_SZ,
                value.as_ptr().cast(),
                (value.len() * 2) as u32,
            )
        };
        // SAFETY: `key` was opened above and is closed once.
        unsafe { RegCloseKey(key) };
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!(
                "couldn't set the login item (registry error {status})"
            ))
        }
    }

    pub fn remove(name: &str) -> Result<(), String> {
        let key = open(KEY_SET_VALUE)
            .map_err(|e| format!("couldn't open the Run key (registry error {e})"))?;
        let name = wide(name);
        // SAFETY: `name` is NUL-terminated and outlives the call.
        let status = unsafe { RegDeleteValueW(key, name.as_ptr()) };
        // SAFETY: `key` was opened above and is closed once.
        unsafe { RegCloseKey(key) };
        if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            Err(format!(
                "couldn't remove the login item (registry error {status})"
            ))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Writes, reads and removes a value of its own under the real `Run` key (no program
        /// runs it: the name is not River's and the command is inert). Not run by the Linux
        /// CI; it needs a Windows session.
        #[test]
        fn a_run_value_is_written_read_and_removed() {
            let name = format!("RiverTest-{}", std::process::id());
            assert_eq!(get(&name), None);
            set(&name, "\"C:\\inert.exe\" --background").unwrap();
            assert_eq!(
                get(&name).as_deref(),
                Some("\"C:\\inert.exe\" --background")
            );
            remove(&name).unwrap();
            assert_eq!(get(&name), None);
            remove(&name).expect("removing what is not there is fine");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item_in(dir: &Path, os: &str) -> LoginItem {
        LoginItem {
            exe: PathBuf::from("/opt/River App/river"),
            place: file_place(os, &dir.join("config"), &dir.join("home")).expect("a file"),
        }
    }

    #[test]
    fn the_desktop_entry_starts_river_in_the_tray() {
        let text = desktop_entry(Path::new("/opt/River App/river"));
        assert!(text.starts_with("[Desktop Entry]\n"));
        assert!(
            text.contains("\nExec=\"/opt/River App/river\" --background\n"),
            "{text}"
        );
        assert!(text.contains("\nType=Application\n"));
        assert!(text.contains("\nTerminal=false\n"));
    }

    #[test]
    fn exec_arguments_escape_what_the_spec_reserves() {
        assert_eq!(exec_quoted("/a b/r"), "\"/a b/r\"");
        assert_eq!(exec_quoted("/a$b"), "\"/a\\\\$b\"");
        assert_eq!(exec_quoted("/100%/r"), "\"/100%%/r\"");
        assert_eq!(exec_quoted("/q\"/r"), "\"/q\\\\\"/r\"");
        assert_eq!(exec_quoted("/b\\r"), "\"/b\\\\\\\\r\"");
    }

    #[test]
    fn the_launch_agent_runs_at_load_and_escapes_the_path() {
        let text = launch_agent(Path::new("/Applications/R&D <x>/River"));
        assert!(text.contains("<string>org.freenet.River</string>"));
        assert!(text.contains("<string>/Applications/R&amp;D &lt;x&gt;/River</string>"));
        assert!(text.contains("<string>--background</string>"));
        assert!(text.contains("<key>RunAtLoad</key>\n\t<true/>"));
    }

    #[test]
    fn a_hidden_or_switched_off_entry_is_not_enabled() {
        assert!(entry_enabled(&desktop_entry(Path::new("/opt/river"))));
        assert!(!entry_enabled("[Desktop Entry]\nHidden=true\n"));
        assert!(!entry_enabled(
            "[Desktop Entry]\nX-GNOME-Autostart-enabled=false\n"
        ));
        assert!(entry_enabled(
            "[Desktop Entry]\nHidden=false\nX-GNOME-Autostart-enabled=true\n"
        ));
    }

    #[test]
    fn a_program_path_that_would_not_survive_is_refused() {
        assert!(check_exe(Path::new("/opt/river")).is_ok());
        assert!(check_exe(Path::new("/opt/river (deleted)")).is_err());
        assert!(check_exe(Path::new("/opt/ri\nver")).is_err());
        let dir = tempfile::tempdir().unwrap();
        let mut item = item_in(dir.path(), "linux");
        item.exe = PathBuf::from("/opt/river (deleted)");
        assert!(item.set(true).is_err());
        assert!(!item.enabled());
    }

    #[test]
    fn an_item_that_is_on_follows_the_program_and_one_that_is_off_stays_off() {
        let dir = tempfile::tempdir().unwrap();
        let mut item = item_in(dir.path(), "linux");
        item.refresh();
        assert!(!item.enabled(), "refreshing never turns it on");
        item.set(true).unwrap();
        #[allow(irrefutable_let_patterns)] // Windows has a second place.
        let Place::File { path, .. } = &item.place else {
            unreachable!()
        };
        let path = path.clone();
        item.exe = PathBuf::from("/opt/River2/river");
        item.refresh();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("/opt/River2/river")
        );
        // Switched off in the desktop's settings: left alone.
        std::fs::write(&path, "[Desktop Entry]\nHidden=true\nExec=old\n").unwrap();
        item.refresh();
        assert!(std::fs::read_to_string(&path).unwrap().contains("Exec=old"));
        assert!(!item.enabled());
    }

    #[test]
    fn the_windows_command_quotes_the_path() {
        assert_eq!(
            run_command(Path::new("C:\\Program Files\\River\\river.exe")),
            "\"C:\\Program Files\\River\\river.exe\" --background"
        );
    }

    #[test]
    fn each_system_keeps_its_item_in_its_own_place() {
        let (config, home) = (Path::new("/c"), Path::new("/h"));
        let path = |os| match file_place(os, config, home) {
            Some(Place::File { path, .. }) => Some(path),
            _ => None,
        };
        assert_eq!(
            path("linux"),
            Some(PathBuf::from("/c/autostart/river.desktop"))
        );
        assert_eq!(
            path("freebsd"),
            Some(PathBuf::from("/c/autostart/river.desktop"))
        );
        assert_eq!(
            path("macos"),
            Some(PathBuf::from(
                "/h/Library/LaunchAgents/org.freenet.River.plist"
            ))
        );
        assert_eq!(path("windows"), None);
    }

    #[test]
    fn turning_it_on_and_off_writes_and_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        for os in ["linux", "macos"] {
            let item = item_in(dir.path(), os);
            assert!(!item.enabled(), "{os} starts off");
            item.set(false).expect("off when already off");
            item.set(true).unwrap();
            assert!(item.enabled(), "{os} on");
            #[allow(irrefutable_let_patterns)] // Windows has a second place.
            let Place::File { path, .. } = &item.place else {
                unreachable!()
            };
            let text = std::fs::read_to_string(path).unwrap();
            assert!(text.contains("--background"), "{text}");
            // Turning it on again replaces the file and leaves no temporary file behind.
            item.set(true).unwrap();
            let leftovers = std::fs::read_dir(path.parent().unwrap()).unwrap().count();
            assert_eq!(leftovers, 1);
            item.set(false).unwrap();
            assert!(!item.enabled(), "{os} off");
        }
    }
}
