//! The desktop answer to "where is Freenet, and how do I get one running?".

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use river_app::{InstallStep, NodeHost, NodeProbe};

use crate::install;
use crate::release::{self, Release};

/// The node's client API on its default port. `/v1/version` answers as soon as the HTTP
/// server is up, before the node has joined the network. A node on another port is not found.
const VERSION_URL: &str = "http://127.0.0.1:7509/v1/version";

/// The longest a `freenet service ...` command may take.
const COMMAND_DEADLINE: Duration = Duration::from_secs(120);

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

    fn running_version(&self) -> Option<String> {
        let body = self
            .agent
            .get(VERSION_URL)
            .call()
            .ok()?
            .into_string()
            .ok()?;
        let value: serde_json::Value = serde_json::from_str(&body).ok()?;
        value.get("version")?.as_str().map(str::to_owned)
    }
}

impl NodeHost for DesktopHost {
    fn probe(&self) -> NodeProbe {
        if let Some(version) = self.running_version() {
            NodeProbe::Running { version }
        } else if find_installed().is_some() {
            NodeProbe::InstalledNotRunning
        } else {
            NodeProbe::NotInstalled
        }
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
            self.running_version().is_some()
        })
    }

    fn start(&self) -> Result<(), String> {
        match find_installed().ok_or("Freenet isn't installed")? {
            Installed::MacApp(app) => run(Command::new("/usr/bin/open").arg(app)),
            Installed::Binary(bin) => {
                let installed_by_river = install::binary_install_path().as_deref() == Some(&bin);
                start_binary(
                    &bin,
                    &SystemRunner,
                    installed_by_river,
                    system_service_installed(),
                )
            }
        }
    }
}

/// How `start_binary` runs things, so its decisions can be tested without running Freenet.
trait Runner {
    fn run(&self, bin: &Path, args: &[&str]) -> Result<(), String>;
    /// Start a long-running process and leave it running.
    fn spawn_detached(&self, bin: &Path, args: &[&str]) -> Result<(), String>;
}

struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, bin: &Path, args: &[&str]) -> Result<(), String> {
        run(Command::new(bin).args(args))
    }

    fn spawn_detached(&self, bin: &Path, args: &[&str]) -> Result<(), String> {
        let mut cmd = Command::new(bin);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            // Its own process group, so a Ctrl-C in a terminal that launched River leaves it.
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        no_console_window(&mut cmd);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("couldn't run {}: {e}", bin.display()))?;
        // Reap it if it exits while River is running, so it doesn't linger as a zombie.
        thread::spawn(move || child.wait());
        Ok(())
    }
}

/// Start the node from a `freenet` binary.
///
/// 1. `freenet service start`: the normal case.
/// 2. If that fails and this is the copy River installed, register the service first. River
///    only registers a service for a binary it installed itself; any other copy belongs to
///    whoever installed it.
/// 3. If there is no service manager River can use (no systemd user session, for example),
///    run the node directly. It then runs until logout rather than at every login.
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
            "couldn't start the Freenet at {}: {first}",
            bin.display()
        ));
    }
    let registered = runner
        .run(bin, &["service", "install"])
        .and_then(|()| runner.run(bin, &["service", "start"]));
    match registered {
        Ok(()) => Ok(()),
        Err(_) => runner.spawn_detached(bin, &["network"]),
    }
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
    let mut output = String::new();
    let _ = stderr.seek(SeekFrom::Start(0));
    let _ = stderr.take(64 * 1024).read_to_string(&mut output);
    let detail = output
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("no error output");
    Err(format!("{} failed: {detail}", describe(cmd)))
}

/// Windows: don't flash a console window for a console program run from River.
fn no_console_window(cmd: &mut Command) {
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
        fn spawn_detached(&self, _: &Path, args: &[&str]) -> Result<(), String> {
            self.calls
                .borrow_mut()
                .push(format!("detached {}", args.join(" ")));
            Ok(())
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
    fn without_a_service_manager_the_node_is_run_directly() {
        let r = FakeRunner::failing(&["service start", "service install"]);
        start_binary(Path::new(BIN), &r, true, false).unwrap();
        assert_eq!(
            r.calls(),
            vec!["service start", "service install", "detached network"]
        );

        // Registered, but the service still won't start.
        let r = FakeRunner::failing(&["service start", "service start"]);
        start_binary(Path::new(BIN), &r, true, false).unwrap();
        assert_eq!(
            r.calls(),
            vec![
                "service start",
                "service install",
                "service start",
                "detached network"
            ]
        );
    }

    #[test]
    fn someone_elses_copy_is_never_registered() {
        let r = FakeRunner::failing(&["service start"]);
        let err = start_binary(Path::new("/usr/bin/freenet"), &r, false, false).unwrap_err();
        assert_eq!(r.calls(), vec!["service start"]);
        assert!(
            err.contains("/usr/bin/freenet") && err.contains("service start failed"),
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
            started.elapsed() < Duration::from_secs(5),
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
