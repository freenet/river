//! The diagnostics log: River's stderr, kept in a file the user can attach to a bug report.
//!
//! A Windows release build has no console, and a River started from a launcher or at login can
//! have no stderr at all, so its diagnostics (`river_app::diag!` lines) went nowhere. When
//! nobody else is reading stderr, [`start`] points it straight at an append-mode file
//! (`dup2` onto fd 2, or `SetStdHandle(STD_ERROR_HANDLE)`): `<config dir>/logs/river.log`.
//!
//! "Nobody else is reading it" means stderr is **invalid or closed, or goes to the null device**
//! (`/dev/null`, `NUL`). Anything else is left alone: a terminal, a file (`2> river-log.txt`,
//! the capture recipe), a pipe (`| tee`) and journald or any other system logger all keep
//! getting the output, so nothing that works today stops working. Then there is no log file
//! and Settings says so.
//!
//! There is no pipe and no thread: the file is the process's stderr, so nothing can hang or
//! panic in a diagnostics path, and a process that inherits fd 2 (an `exec`, a relaunch, a
//! child) holds a plain file. The time on each line comes from `diag!`, not from here.
//!
//! The file is rotated at start only: when `river.log` is over [`LIMIT`] it is renamed to
//! `river.log.1` (replacing the one before) and a new one starts. If the rename fails the file
//! is appended to and keeps growing. Nothing rotates while River runs, so two Rivers writing at
//! once (the old and new one during an update) never move the file under each other. One run is
//! not capped: a River that logs several MB in one run grows the file until its next start.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Rotate at start when the file is over this many bytes.
pub const LIMIT: u64 = 2 * 1024 * 1024;

/// The current file's name; the previous one is the same with `.1` added.
pub const FILE_NAME: &str = "river.log";

/// Where the log goes for a given config directory: a `logs` folder of its own, so that
/// opening the folder from Settings does not show the node's grant token next to the log.
pub fn path_in(config_dir: &Path) -> PathBuf {
    config_dir.join("logs").join(FILE_NAME)
}

/// The path of the previous file.
pub fn previous(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".1");
    PathBuf::from(name)
}

/// What stderr is when River starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stderr {
    /// A terminal.
    Terminal,
    /// The null device: output goes nowhere.
    Null,
    /// Closed, or not a handle at all (a GUI program started with no console).
    Invalid,
    /// A file, a pipe, a socket (journald): somebody may be reading it.
    Other,
}

/// Whether to take stderr over. Only when nothing could be reading it.
pub fn decide(stderr: Stderr) -> bool {
    matches!(stderr, Stderr::Null | Stderr::Invalid)
}

static PATH: OnceLock<PathBuf> = OnceLock::new();

/// The file stderr was pointed at, once [`start`] has done it.
#[cfg(unix)]
pub fn active() -> Option<&'static Path> {
    PATH.get().map(|p| p.as_path())
}

/// Where a River this one starts should send its stderr: nowhere, when this process's stderr
/// is the log file, so the new River looks at a null stderr and sets up its own log (and its
/// Settings say where it is); otherwise what this process has.
#[cfg(unix)]
pub fn child_stderr() -> std::process::Stdio {
    if active().is_some() {
        std::process::Stdio::null()
    } else {
        std::process::Stdio::inherit()
    }
}

/// Point stderr at the log file in `config_dir` when nobody is reading it ([`decide`]).
/// Returns the file's path when it did, `None` when stderr was left as it was (somebody reads
/// it, or the file could not be opened). Call once, after the single-instance check, since it
/// rotates the file.
pub fn start(config_dir: &Path) -> Option<PathBuf> {
    start_with(config_dir, probe())
}

fn start_with(config_dir: &Path, stderr: Stderr) -> Option<PathBuf> {
    if !decide(stderr) {
        return None;
    }
    if let Some(p) = PATH.get() {
        return Some(p.clone());
    }
    let path = path_in(config_dir);
    let file = open_for_run(&path, LIMIT).ok()?;
    sys::take_stderr(file).ok()?;
    let _ = PATH.set(path.clone());
    river_app::diag!(
        "--- River {} started, pid {}, {} ---",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        std::env::consts::OS
    );
    // The default hook prints the panic message as it is, and a message can quote a value
    // (`called Result::unwrap() on an Err value: ... "hello"`), so the log gets a scrubbed one.
    std::panic::set_hook(Box::new(|info| {
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        let at = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        river_app::diag!(
            "panic in thread '{}' at {at}: {}",
            std::thread::current().name().unwrap_or("<unnamed>"),
            river_app::diag::scrub(message)
        );
    }));
    Some(path)
}

/// Rotate `path` if it is over `limit`, then open it for appending, creating its folder. A
/// failed rename is not an error: the file is appended to and grows.
pub fn open_for_run(path: &Path, limit: u64) -> io::Result<File> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    if fs::metadata(path).is_ok_and(|m| m.len() > limit) {
        let _ = fs::rename(path, previous(path));
    }
    OpenOptions::new().create(true).append(true).open(path)
}

fn probe() -> Stderr {
    sys::probe()
}

#[cfg(unix)]
mod sys {
    use super::Stderr;
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;

    pub fn probe() -> Stderr {
        // SAFETY: fstat and isatty on fd 2 with a zeroed out-parameter.
        unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            if libc::fstat(2, &mut st) != 0 {
                return Stderr::Invalid;
            }
            if libc::isatty(2) == 1 {
                return Stderr::Terminal;
            }
            if st.st_mode & libc::S_IFMT == libc::S_IFCHR
                && std::fs::metadata("/dev/null").is_ok_and(|n| n.rdev() == st.st_rdev as u64)
            {
                return Stderr::Null;
            }
        }
        Stderr::Other
    }

    /// Make fd 2 a copy of `file` (opened for appending). `file` is closed after.
    pub fn take_stderr(file: std::fs::File) -> io::Result<()> {
        // SAFETY: dup2 onto fd 2 closes the old fd 2 (if any) and copies `file`'s descriptor.
        if unsafe { libc::dup2(file.as_raw_fd(), 2) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(windows)]
mod sys {
    use super::Stderr;
    use std::io;
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_CHAR, GetFileType};
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, STD_ERROR_HANDLE, SetStdHandle,
    };

    pub fn probe() -> Stderr {
        // SAFETY: GetStdHandle, GetFileType and GetConsoleMode only read the handle.
        unsafe {
            let h = GetStdHandle(STD_ERROR_HANDLE);
            if h.is_null() || h == INVALID_HANDLE_VALUE {
                return Stderr::Invalid;
            }
            if GetFileType(h) == FILE_TYPE_CHAR {
                let mut mode = 0;
                // A console answers; NUL is a character device that does not.
                return if GetConsoleMode(h, &mut mode) != 0 {
                    Stderr::Terminal
                } else {
                    Stderr::Null
                };
            }
        }
        Stderr::Other
    }

    /// Make the standard error handle `file`, which stays open for the life of the process.
    pub fn take_stderr(file: std::fs::File) -> io::Result<()> {
        let handle = file.into_raw_handle();
        // SAFETY: the handle is valid and is deliberately never closed.
        if unsafe { SetStdHandle(STD_ERROR_HANDLE, handle as _) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("river-log-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_path_is_a_logs_folder_of_the_config_dir() {
        let p = path_in(Path::new("/c/River"));
        assert_eq!(p, Path::new("/c/River/logs/river.log"));
        assert_eq!(previous(&p), Path::new("/c/River/logs/river.log.1"));
    }

    #[test]
    fn only_a_stderr_nobody_reads_is_taken_over() {
        assert!(decide(Stderr::Null));
        assert!(decide(Stderr::Invalid));
        // A terminal, a file (`2> river-log.txt`), a pipe (`| tee`), journald.
        assert!(!decide(Stderr::Terminal));
        assert!(!decide(Stderr::Other));
    }

    #[test]
    fn a_stderr_that_is_left_alone_makes_no_file() {
        let dir = temp("leave");
        assert_eq!(start_with(&dir, Stderr::Other), None);
        assert_eq!(start_with(&dir, Stderr::Terminal), None);
        assert!(!dir.join("logs").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_over_the_limit_is_rotated_at_start_and_one_under_it_is_not() {
        let dir = temp("rotate");
        let path = dir.join("logs").join("river.log");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "x".repeat(100)).unwrap();
        // At the limit is not over it.
        open_for_run(&path, 100).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "x".repeat(100));
        assert!(!previous(&path).exists());
        // Over: the old one becomes .1 (replacing an older .1) and the new file is empty.
        fs::write(previous(&path), "older").unwrap();
        fs::write(&path, "y".repeat(101)).unwrap();
        open_for_run(&path, 100).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "");
        assert_eq!(
            fs::read_to_string(previous(&path)).unwrap(),
            "y".repeat(101)
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_rename_keeps_appending_and_loses_nothing() {
        let dir = temp("norename");
        let path = dir.join("logs").join("river.log");
        // A non-empty folder where `.1` should go makes the rename fail.
        fs::create_dir_all(previous(&path).join("inside")).unwrap();
        fs::write(&path, "z".repeat(150)).unwrap();
        let mut f = open_for_run(&path, 100).unwrap();
        io::Write::write_all(&mut f, b"new line\n").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text, format!("{}new line\n", "z".repeat(150)));
        let _ = fs::remove_dir_all(&dir);
    }

    /// `main` decides nothing itself: it calls `start`, which looks at the real stderr, and it
    /// does so after the single-instance check (a second launch must not rotate the file).
    /// Every place that starts another River gives it `log::child_stderr()`.
    #[test]
    fn main_starts_the_log_after_the_instance_check_and_relaunches_pass_the_stderr_choice() {
        let main = include_str!("main.rs");
        let acquired = main.find("single::acquire(").expect("the instance check");
        let started = main.find("log::start(&dir)").expect("main starts the log");
        assert!(acquired < started);
        for (name, src) in [
            ("update/host.rs", include_str!("update/host.rs")),
            ("update/linux.rs", include_str!("update/linux.rs")),
        ] {
            assert!(
                src.contains(".stderr(crate::log::child_stderr())"),
                "{name}"
            );
        }
    }
}
