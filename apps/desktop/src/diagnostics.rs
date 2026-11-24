//! The desktop side of Settings > Diagnostics (`river_app::chat::LogHost`): the log file's
//! path, and opening its folder in the file manager.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use river_app::chat::LogHost;

pub struct DesktopLog {
    /// `None` when stderr was left alone (a terminal, a file, a system logger): there is no
    /// log file.
    path: Option<PathBuf>,
}

impl DesktopLog {
    pub fn new(path: Option<PathBuf>) -> Self {
        DesktopLog { path }
    }
}

impl LogHost for DesktopLog {
    fn log_path(&self) -> Option<PathBuf> {
        self.path.clone()
    }

    fn open_log_folder(&self) -> Result<(), String> {
        let path = self.path.as_ref().ok_or("River keeps no log file")?;
        let dir = path.parent().ok_or("the log has no folder")?;
        let mut cmd = opener(dir);
        // This process may have no console (a Windows release build, a launcher), and a child
        // that inherits invalid standard handles fails with "The handle is invalid". The
        // opener has nothing to read or say.
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("couldn't open {}: {e}", dir.display()))?;
        // Reaped on a thread so it does not linger as a zombie. Not waited for here, and its
        // exit status is not read: Explorer exits 1 after it has opened the folder.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}

/// The system's way to show a folder. Not `Tx::open_url`: Rune opens only http, https and
/// mailto links, and refuses `file:`.
fn opener(dir: &Path) -> Command {
    #[cfg(target_os = "macos")]
    let mut cmd = Command::new("/usr/bin/open");
    #[cfg(windows)]
    let mut cmd = Command::new("explorer.exe");
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = Command::new("xdg-open");
    cmd.arg(dir);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_reports_the_path_it_was_given() {
        let log = DesktopLog::new(Some(PathBuf::from("/c/River/logs/river.log")));
        assert_eq!(
            log.log_path().as_deref(),
            Some(Path::new("/c/River/logs/river.log"))
        );
    }

    #[test]
    fn the_opener_is_given_the_folder_and_not_the_file() {
        let cmd = opener(Path::new("/c/River/logs"));
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, ["/c/River/logs"]);
    }

    #[test]
    fn a_path_with_no_folder_is_an_error_not_a_panic() {
        let log = DesktopLog::new(Some(PathBuf::from("/")));
        assert!(log.open_log_folder().is_err());
        assert!(DesktopLog::new(None).open_log_folder().is_err());
    }
}
