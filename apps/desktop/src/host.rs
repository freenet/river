//! The desktop answer to "where is Freenet, and how do I get one running?".

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use river_app::{InstallStep, NodeHost, NodeProbe};

use crate::install;
use crate::release::{self, Release};

/// The node's client API on its default port. `/v1/version` (Freenet 0.2.71 and later)
/// answers as soon as the HTTP server is up, before the node has joined the network. A node on
/// another port is not found.
const VERSION_URL: &str = "http://127.0.0.1:7509/v1/version";

/// The longest a command River runs (`freenet service ...`, `hdiutil`, `ditto`) may take.
const COMMAND_DEADLINE: Duration = Duration::from_secs(120);

/// What answered on the node's port.
#[derive(Debug, PartialEq, Eq)]
enum Answer {
    Version(String),
    /// Something HTTP answered, but not with a version: a Freenet older than 0.2.71, or
    /// another program on the port.
    NoVersion,
    Nothing,
}

pub struct DesktopHost {
    agent: ureq::Agent,
}

impl DesktopHost {
    pub fn new() -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(2))
            .build();
        DesktopHost { agent }
    }

    fn ask_node(&self) -> Answer {
        match self.agent.get(VERSION_URL).call() {
            Ok(resp) => resp
                .into_string()
                .ok()
                .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok())
                .and_then(|v| v.get("version")?.as_str().map(str::to_owned))
                .map_or(Answer::NoVersion, Answer::Version),
            Err(ureq::Error::Status(..)) => Answer::NoVersion,
            Err(ureq::Error::Transport(_)) => Answer::Nothing,
        }
    }
}

/// Classify what answered. A version River can't parse is let through rather than refused:
/// the node reported one, so it is new enough to have the endpoint.
fn classify(answer: Answer, installed: bool) -> NodeProbe {
    let minimum = release::MIN_VERSION.to_string();
    match answer {
        Answer::Version(version) => match release::parse_version(&version) {
            Ok(v) if v < release::MIN_VERSION => NodeProbe::Outdated {
                running: Some(version),
                minimum,
            },
            _ => NodeProbe::Running { version },
        },
        Answer::NoVersion => NodeProbe::Outdated {
            running: None,
            minimum,
        },
        Answer::Nothing if installed => NodeProbe::InstalledNotRunning,
        Answer::Nothing => NodeProbe::NotInstalled,
    }
}

impl NodeHost for DesktopHost {
    fn probe(&self) -> NodeProbe {
        classify(self.ask_node(), find_installed().is_some())
    }

    fn can_install(&self) -> bool {
        release::asset_name(release::MIN_VERSION).is_ok()
    }

    fn install(&self, progress: &mut dyn FnMut(InstallStep)) -> Result<(), String> {
        progress(InstallStep::FindingRelease);
        let release = Release::latest()?;
        let asset = release::asset_name(release.version)?;
        // Authenticate the manifest BEFORE downloading, so a bad release fails fast.
        let checksums = release.checksums()?;
        checksums.get(&asset)?;
        let bytes = release.fetch(&asset, &mut |received, total| {
            progress(InstallStep::Downloading { received, total })
        })?;
        progress(InstallStep::Verifying);
        release::verify_asset(&checksums, &asset, &bytes)?;
        install::install(&asset, &bytes, progress, &|| {
            matches!(self.ask_node(), Answer::Version(_))
        })
    }

    fn start(&self) -> Result<(), String> {
        match find_installed().ok_or("Freenet isn't installed")? {
            Installed::MacApp(app) => run(Command::new("/usr/bin/open").arg(app)),
            Installed::Binary(bin) => start_binary(
                &bin,
                &SystemRunner,
                install::installed_by_river(&bin),
                system_service_installed(),
            ),
        }
    }
}

/// How `start_binary` runs commands, so its decisions can be tested without running Freenet.
trait Runner {
    fn run(&self, bin: &Path, args: &[&str]) -> Result<(), String>;
}

struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, bin: &Path, args: &[&str]) -> Result<(), String> {
        run(Command::new(bin).args(args))
    }
}

/// Start the node from a `freenet` binary with `freenet service start`.
///
/// If no service is registered yet and River installed this copy (recorded when it installed
/// it), River registers one first, as freenet-core's own installer would. Any other copy
/// belongs to whoever installed it, and River only reports why it wouldn't start.
///
/// River never runs the node outside a service manager: a bare `freenet network` stops for
/// good the first time it finds an update, because nothing is there to apply it and restart.
fn start_binary(
    bin: &Path,
    runner: &dyn Runner,
    installed_by_river: bool,
    system_service: bool,
) -> Result<(), String> {
    let first = match runner.run(bin, &["service", "start"]) {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    if system_service {
        return Err("Freenet is installed as a system service. Start it with \
                    `sudo systemctl start freenet`, then try again."
            .into());
    }
    if !installed_by_river {
        return Err(format!(
            "the Freenet at {} didn't start ({first}). Start it yourself, then try again.",
            bin.display()
        ));
    }
    runner.run(bin, &["service", "install"]).map_err(|e| {
        format!(
            "couldn't set Freenet up as a background service ({e}). On a system without a \
                 systemd user session, run `{} network` yourself, then try again.",
            bin.display()
        )
    })?;
    runner
        .run(bin, &["service", "start"])
        .map_err(|e| format!("Freenet's service was set up but didn't start: {e}"))
}

#[derive(Debug)]
pub enum Installed {
    /// macOS: the Freenet app bundle, which runs the node (and its tray icon) when opened.
    MacApp(PathBuf),
    Binary(PathBuf),
}

pub fn find_installed() -> Option<Installed> {
    if cfg!(target_os = "macos") {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let bundles = [
            Some(PathBuf::from("/Applications/Freenet.app")),
            home.map(|h| h.join("Applications/Freenet.app")),
        ];
        if let Some(app) = bundles.into_iter().flatten().find(|p| p.is_dir()) {
            return Some(Installed::MacApp(app));
        }
    }
    install::binary_install_path()
        .filter(|p| p.is_file())
        .or_else(|| {
            let name = if cfg!(windows) {
                "freenet.exe"
            } else {
                "freenet"
            };
            std::env::var_os("PATH").and_then(|path| on_path(name, &path))
        })
        .map(Installed::Binary)
}

/// `name` in the first absolute directory of `path` that has it. Relative entries (including
/// an empty one, which means the current directory) are skipped.
fn on_path(name: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

fn system_service_installed() -> bool {
    cfg!(target_os = "linux") && Path::new("/etc/systemd/system/freenet.service").exists()
}

/// Run a command to completion, returning the last line of its stderr on failure.
pub fn run(cmd: &mut Command) -> Result<(), String> {
    run_with_deadline(cmd, COMMAND_DEADLINE)
}

/// Every standard handle is set explicitly: a GUI process on Windows has no valid console
/// handles to inherit, and inheriting them makes `spawn` fail (freenet-core #3933/#3934).
/// Stderr goes to an anonymous temporary file rather than a pipe, and River waits on the
/// child's exit, not on end-of-file: a detached process the command leaves behind may hold
/// the handle for as long as it runs, and waiting for a pipe to close would wait for it.
/// At the deadline only the command itself is killed, not anything it started.
fn run_with_deadline(cmd: &mut Command, deadline: Duration) -> Result<(), String> {
    let describe = |cmd: &Command| {
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy()).collect();
        format!("{} {}", cmd.get_program().to_string_lossy(), args.join(" "))
    };
    let mut stderr = tempfile::tempfile().map_err(|e| e.to_string())?;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr.try_clone().map_err(|e| e.to_string())?);
    no_console_window(cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("couldn't run {}: {e}", describe(cmd)))?;
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if started.elapsed() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "{} didn't finish within {} seconds",
                describe(cmd),
                deadline.as_secs()
            ));
        }
        thread::sleep(Duration::from_millis(50));
    };
    if status.success() {
        return Ok(());
    }
    // The end of the output is where the error is.
    const TAIL: u64 = 64 * 1024;
    let len = stderr.seek(SeekFrom::End(0)).unwrap_or(0);
    let _ = stderr.seek(SeekFrom::Start(len.saturating_sub(TAIL)));
    let mut tail = Vec::new();
    let _ = stderr.take(TAIL).read_to_end(&mut tail);
    let tail = String::from_utf8_lossy(&tail);
    let detail = tail
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("no error output");
    Err(format!("{} failed: {detail}", describe(cmd)))
}

/// Windows: don't flash a console window for a console program run from River.
pub fn no_console_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = cmd;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Records each command. Each command listed in `fail` fails once, the first time it runs.
    struct FakeRunner {
        fail: RefCell<Vec<&'static str>>,
        calls: RefCell<Vec<String>>,
    }

    impl FakeRunner {
        fn failing(fail: &[&'static str]) -> Self {
            FakeRunner {
                fail: RefCell::new(fail.to_vec()),
                calls: RefCell::new(Vec::new()),
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl Runner for FakeRunner {
        fn run(&self, _: &Path, args: &[&str]) -> Result<(), String> {
            let call = args.join(" ");
            self.calls.borrow_mut().push(call.clone());
            let mut fail = self.fail.borrow_mut();
            match fail.iter().position(|f| *f == call) {
                Some(i) => {
                    fail.remove(i);
                    Err(format!("{call} failed"))
                }
                None => Ok(()),
            }
        }
    }

    const BIN: &str = "/home/u/.local/bin/freenet";

    #[test]
    fn a_registered_service_is_just_started() {
        let r = FakeRunner::failing(&[]);
        start_binary(Path::new(BIN), &r, true, false).unwrap();
        assert_eq!(r.calls(), vec!["service start"]);
    }

    #[test]
    fn river_registers_the_service_for_its_own_copy() {
        let r = FakeRunner::failing(&["service start"]);
        start_binary(Path::new(BIN), &r, true, false).unwrap();
        assert_eq!(
            r.calls(),
            vec!["service start", "service install", "service start"]
        );
    }

    #[test]
    fn registration_failures_explain_themselves_and_run_nothing_else() {
        let r = FakeRunner::failing(&["service start", "service install"]);
        let err = start_binary(Path::new(BIN), &r, true, false).unwrap_err();
        assert_eq!(r.calls(), vec!["service start", "service install"]);
        assert!(err.contains("service install failed"), "{err}");
        assert!(err.contains(&format!("`{BIN} network`")), "{err}");

        let r = FakeRunner::failing(&["service start", "service start"]);
        let err = start_binary(Path::new(BIN), &r, true, false).unwrap_err();
        assert_eq!(
            r.calls(),
            vec!["service start", "service install", "service start"]
        );
        assert!(err.contains("set up but didn't start"), "{err}");
    }

    #[test]
    fn someone_elses_copy_is_never_registered() {
        let r = FakeRunner::failing(&["service start"]);
        let err = start_binary(Path::new(BIN), &r, false, false).unwrap_err();
        assert_eq!(r.calls(), vec!["service start"]);
        assert!(
            err.contains(BIN) && err.contains("service start failed"),
            "{err}"
        );
    }

    #[test]
    fn a_stopped_system_service_is_left_to_the_administrator() {
        let r = FakeRunner::failing(&["service start"]);
        let err = start_binary(Path::new(BIN), &r, true, true).unwrap_err();
        assert!(err.contains("sudo systemctl start freenet"), "{err}");
        assert_eq!(r.calls(), vec!["service start"]);
    }

    #[test]
    fn what_answers_on_the_port_decides_the_state() {
        let version = |v: &str| Answer::Version(v.into());
        assert_eq!(
            classify(version("0.2.155"), true),
            NodeProbe::Running {
                version: "0.2.155".into()
            }
        );
        assert_eq!(
            classify(version("0.3.0"), false),
            NodeProbe::Running {
                version: "0.3.0".into()
            }
        );
        assert_eq!(
            classify(version("0.2.100"), true),
            NodeProbe::Outdated {
                running: Some("0.2.100".into()),
                minimum: "0.2.155".into()
            }
        );
        // A version River can't parse came from the endpoint, so the node has it.
        assert!(matches!(
            classify(version("0.4.0-rc1"), true),
            NodeProbe::Running { .. }
        ));
        assert_eq!(
            classify(Answer::NoVersion, true),
            NodeProbe::Outdated {
                running: None,
                minimum: "0.2.155".into()
            }
        );
        assert_eq!(
            classify(Answer::Nothing, true),
            NodeProbe::InstalledNotRunning
        );
        assert_eq!(classify(Answer::Nothing, false), NodeProbe::NotInstalled);
    }

    #[test]
    fn relative_path_entries_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("freenet"), b"").unwrap();
        let path =
            std::env::join_paths([Path::new(""), Path::new("relative"), dir.path()]).unwrap();
        assert_eq!(on_path("freenet", &path), Some(dir.path().join("freenet")));
        let only_relative = std::env::join_paths([Path::new("."), Path::new("")]).unwrap();
        assert_eq!(on_path("freenet", &only_relative), None);
    }

    #[cfg(unix)]
    #[test]
    fn run_reports_the_last_stderr_line_and_spawn_failures() {
        assert!(run(Command::new("sh").args(["-c", "exit 0"])).is_ok());
        let err =
            run(Command::new("sh").args(["-c", "echo first >&2; echo boom >&2; echo >&2; exit 3"]))
                .unwrap_err();
        assert!(err.ends_with("failed: boom"), "{err}");
        // Longer than the tail River reads: the LAST line is still the one reported.
        let err = run(Command::new("sh").args([
            "-c",
            "i=0; while [ $i -lt 20000 ]; do echo noise-line-$i >&2; i=$((i+1)); done; \
             echo the-real-error >&2; exit 1",
        ]))
        .unwrap_err();
        assert!(err.ends_with("failed: the-real-error"), "{err}");
        let err = run(&mut Command::new("/definitely/not/here")).unwrap_err();
        assert!(
            err.starts_with("couldn't run /definitely/not/here"),
            "{err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_process_left_holding_stderr_does_not_hold_up_run() {
        // The command exits at once but leaves a background process holding its stderr, as
        // `freenet service start` may leave a node. Waiting for end-of-file would take 30 s.
        let started = Instant::now();
        run(Command::new("sh").args(["-c", "sleep 30 & exit 0"])).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "{:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_command_that_hangs_is_killed_at_the_deadline() {
        let err = run_with_deadline(Command::new("sleep").arg("30"), Duration::from_millis(300))
            .unwrap_err();
        assert!(err.contains("didn't finish within"), "{err}");
    }
}
