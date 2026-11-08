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
    /// Something HTTP answered definitely without a version (see [`ask`]): a Freenet older
    /// than 0.2.71, or another program on the port.
    NoVersion,
    Nothing,
}

pub struct DesktopHost {
    agent: ureq::Agent,
}

impl DesktopHost {
    pub fn new() -> Self {
        DesktopHost {
            agent: probe_agent(),
        }
    }

    fn ask_node(&self) -> Answer {
        ask(&self.agent, VERSION_URL)
    }
}

/// A short timeout, and no redirects: the node's own endpoint never redirects.
fn probe_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(2))
        .redirects(0)
        .build()
}

/// Ask `url` for the node's version.
///
/// Freenet answers `/v1/version` with its version whenever its HTTP server is up, and a
/// Freenet before 0.2.71 answers 404. So any other HTTP answer means something that isn't a
/// usable Freenet holds the port, and is reported, not waited on. The exception is a 502, 503
/// or 504, which only a proxy in front of a node that isn't up yet would send: that, like a
/// transport failure (refused, timed out, not HTTP), counts as nothing answering yet.
fn ask(agent: &ureq::Agent, url: &str) -> Answer {
    match agent.get(url).call() {
        Ok(resp) => resp
            .into_string()
            .ok()
            .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok())
            .and_then(|v| v.get("version")?.as_str().map(str::to_owned))
            .map_or(Answer::NoVersion, Answer::Version),
        Err(ureq::Error::Status(502..=504, _) | ureq::Error::Transport(_)) => Answer::Nothing,
        Err(ureq::Error::Status(..)) => Answer::NoVersion,
    }
}

/// Classify what answered. Only the `MAJOR.MINOR.PATCH` part of a version is compared, so a
/// suffixed version (`0.2.100-rc.1`) is still held to the floor. A version River can't parse
/// at all is let through: the node reported one, so it is new enough to have the endpoint.
fn classify(answer: Answer, installed: bool) -> NodeProbe {
    let minimum = release::MIN_NODE_VERSION.to_string();
    match answer {
        Answer::Version(version) => {
            let pre_release = version.split('+').next().unwrap_or_default().contains('-');
            match release::parse_version(version.split(['-', '+']).next().unwrap_or_default()) {
                // A pre-release comes before its release, so `0.2.155-rc.1` is below 0.2.155.
                Ok(v)
                    if v < release::MIN_NODE_VERSION
                        || (v == release::MIN_NODE_VERSION && pre_release) =>
                {
                    NodeProbe::Outdated {
                        running: Some(version),
                        minimum,
                    }
                }
                _ => NodeProbe::Running { version },
            }
        }
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

    fn config_dir(&self) -> Option<std::path::PathBuf> {
        directories::ProjectDirs::from("", "Freenet", "River").map(|d| d.config_dir().to_owned())
    }

    fn can_install(&self) -> bool {
        release::asset_name(release::MIN_INSTALL_VERSION).is_ok()
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
                // Asked only when needed: it hashes the binary.
                &|| install::installed_by_river(&bin),
                registered_services(),
                &mut install::forget_install,
            ),
        }
    }
}

/// Which Freenet services are registered on this machine, judged by the unit files
/// freenet-core writes, at the paths it uses (it doesn't follow `XDG_CONFIG_HOME` either):
/// Linux `~/.config/systemd/user/freenet.service` and `/etc/systemd/system/freenet.service`,
/// macOS `~/Library/LaunchAgents/org.freenet.node.plist`. Windows has no unit file;
/// `service start` there always launches the wrapper.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Services {
    user: bool,
    system: bool,
}

fn registered_services() -> Services {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    registered_services_in(std::env::consts::OS, home.as_deref(), Path::new("/"))
}

/// [`registered_services`] for `os`, with `home` and the filesystem `root` given, for tests.
fn registered_services_in(os: &str, home: Option<&Path>, root: &Path) -> Services {
    let user_unit = match os {
        "linux" => home.map(|h| h.join(".config/systemd/user/freenet.service")),
        "macos" => home.map(|h| h.join("Library/LaunchAgents/org.freenet.node.plist")),
        _ => None,
    };
    Services {
        user: user_unit.is_some_and(|p| p.exists()),
        system: os == "linux" && root.join("etc/systemd/system/freenet.service").exists(),
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
/// River registers a service (`freenet service install`, as freenet-core's own installer
/// would) only when no service unit exists at all and River installed this copy, as recorded
/// when it installed it. River never runs `service install` over an existing unit: freenet-core
/// rewrites the unit file unconditionally, which would replace one the user wrote or edited.
/// Any registered service that fails to start is reported with what to do next, checking the
/// user unit first as freenet-core does. Any copy River didn't install belongs to whoever
/// installed it, and River only reports why it didn't start.
///
/// River never runs the node outside a service manager: a bare `freenet network` stops for
/// good the first time it finds an update, because nothing is there to apply it and restart.
///
/// `done_with_record` runs when a service exists (a start works, a unit file is there, or
/// River registered one), so River's record of its install can be dropped: from then on the
/// binary is started like any other, and if the user later removes the service, River
/// doesn't put it back.
fn start_binary(
    bin: &Path,
    runner: &dyn Runner,
    installed_by_river: &dyn Fn() -> bool,
    services: Services,
    done_with_record: &mut dyn FnMut(),
) -> Result<(), String> {
    let first = match runner.run(bin, &["service", "start"]) {
        Ok(()) => {
            // A start that works shows a service exists, however it got there.
            done_with_record();
            return Ok(());
        }
        Err(e) => e,
    };
    if services.user || services.system {
        // A service unit exists, however it got there, and River never registers over one,
        // so its record has done its job. If the user later deletes the unit, River treats the
        // binary like any other (it reports why it didn't start rather than registering).
        done_with_record();
    }
    // freenet-core uses the user unit when there is one, so check it first.
    if services.user {
        return Err(user_service_failed(bin, &first, services.system));
    }
    if services.system {
        return Err(format!(
            "Freenet's system service didn't start ({first}). Start it with \
             `sudo systemctl start freenet`, then try again."
        ));
    }
    // Asked only now: it hashes the binary.
    if !installed_by_river() {
        return Err(format!(
            "the Freenet at {} didn't start ({first}). Start it yourself, then try again.",
            bin.display()
        ));
    }
    runner.run(bin, &["service", "install"]).map_err(|e| {
        let hint = if cfg!(target_os = "linux") {
            format!(" {}", no_user_session_recipe(bin))
        } else {
            String::new()
        };
        format!("couldn't set Freenet up as a background service ({e}).{hint}")
    })?;
    done_with_record();
    runner
        .run(bin, &["service", "start"])
        .map_err(|e| format!("Freenet's service was set up but didn't start: {e}"))
}

/// Linux without a systemd user session: the steps that get Freenet running as a system
/// service instead. The leftover user unit has to go first, because freenet-core prefers a
/// user unit over a system one. Never a bare `freenet network`: it stops for good at its first
/// update, with nothing to apply it and restart.
fn no_user_session_recipe(bin: &Path) -> String {
    format!(
        "If `systemctl --user` can't connect (there is no systemd user session here), remove \
         `~/.config/systemd/user/freenet.service` if it exists, set Freenet up for the whole \
         system with `sudo {} service install --system`, start it with `sudo systemctl \
         start freenet`, then try again. Freenet needs systemd on Linux.",
        shell_quote(&bin.to_string_lossy())
    )
}

/// `s` as one POSIX shell word: single-quoted, so nothing inside is expanded, with any single
/// quote written as `'\''`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// What to say when the user service exists but didn't start. freenet-core prefers the user
/// unit, so a system unit next to it is ignored until the user unit is removed.
fn user_service_failed(bin: &Path, first: &str, system_unit_too: bool) -> String {
    if !cfg!(target_os = "linux") {
        return format!("Freenet's background service didn't start ({first}). Try again.");
    }
    let next = if system_unit_too {
        "There is also a system service, which Freenet won't use while the user one exists. \
         To use it instead, remove `~/.config/systemd/user/freenet.service` and run `sudo \
         systemctl start freenet`, then try again."
            .to_string()
    } else {
        no_user_session_recipe(bin)
    };
    format!(
        "Freenet's background service didn't start ({first}). See why with `systemctl --user \
         status freenet`. {next}"
    )
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

/// Run a command to completion, returning what its stderr says went wrong on failure (see
/// [`error_line`]).
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
    // The error is near the end of the output.
    const TAIL: u64 = 64 * 1024;
    let len = stderr.seek(SeekFrom::End(0)).unwrap_or(0);
    let _ = stderr.seek(SeekFrom::Start(len.saturating_sub(TAIL)));
    let mut tail = Vec::new();
    let _ = stderr.take(TAIL).read_to_end(&mut tail);
    let tail = String::from_utf8_lossy(&tail);
    Err(format!("{} failed: {}", describe(cmd), error_line(&tail)))
}

/// What a failed command's stderr says went wrong.
///
/// freenet-core prints its error with anyhow's `{:?}` format: `Error: <message>`, then, for an
/// error with context, a blank line, `Caused by:` and the causes (numbered `0: ...` when there
/// are several), the last being the root cause; hint lines (`Hint:`, `Try: ...`) may follow
/// the message. So: the last `Error:` line at the start of a line (column 0), plus its root
/// cause when there is one; failing that, the last indented one. Output without an `Error:`
/// line (clap prints `error:`, matched too) reports its last line.
fn error_line(output: &str) -> String {
    let lines: Vec<&str> = output.lines().map(str::trim).collect();
    let error_at = |line: &str| -> Option<String> {
        let head = line.get(..6)?;
        let rest = line[6..].trim();
        (head.eq_ignore_ascii_case("error:") && !rest.is_empty()).then(|| rest.to_string())
    };
    let raw: Vec<&str> = output.lines().collect();
    // At column 0 first, so an indented cause that happens to begin with "error:" isn't taken
    // for the message; an indented `Error:` only when nothing starts a line with one. Known
    // limit of that fallback: if everything is indented AND a cause begins with "error:", the
    // cause is reported without its message. No known source indents freenet-core's output.
    let found = (0..raw.len())
        .rev()
        .find_map(|i| error_at(raw[i]).map(|rest| (i, rest)))
        .or_else(|| {
            (0..lines.len())
                .rev()
                .find_map(|i| error_at(lines[i]).map(|rest| (i, rest)))
        });
    let Some((at, message)) = found else {
        return lines
            .iter()
            .rev()
            .find(|l| !l.is_empty())
            .map_or("no error output".into(), |l| l.to_string());
    };
    let after = &lines[at + 1..];
    let root_cause = after
        .iter()
        .position(|l| *l == "Caused by:")
        .and_then(|start| {
            after[start + 1..]
                .iter()
                .take_while(|l| !l.is_empty())
                .last()
        })
        .map(|cause| strip_cause_number(cause));
    match root_cause {
        Some(cause) => format!("{message}: {cause}"),
        None => message,
    }
}

/// `3: Permission denied` -> `Permission denied`; anything else unchanged.
fn strip_cause_number(cause: &str) -> &str {
    match cause.split_once(": ") {
        Some((n, rest)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => rest,
        _ => cause,
    }
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

    const NONE: Services = Services {
        user: false,
        system: false,
    };
    const USER: Services = Services {
        user: true,
        system: false,
    };
    const SYSTEM: Services = Services {
        user: false,
        system: true,
    };

    /// Run `start_binary`, returning its result and how many times it dropped River's record.
    fn start(r: &FakeRunner, ours: bool, services: Services) -> (Result<(), String>, usize) {
        let mut dropped = 0;
        let result = start_binary(Path::new(BIN), r, &|| ours, services, &mut || dropped += 1);
        (result, dropped)
    }

    #[test]
    fn a_registered_service_is_just_started() {
        let r = FakeRunner::failing(&[]);
        let (result, dropped) = start(&r, true, USER);
        result.unwrap();
        assert_eq!(r.calls(), vec!["service start"]);
        assert_eq!(dropped, 1, "a working start shows a service exists");
    }

    #[test]
    fn a_registered_service_that_fails_is_reported_never_registered_again() {
        // Whoever installed the binary, River never runs `service install` over an existing
        // unit: freenet-core rewrites the unit file, which could replace one the user edited.
        for ours in [false, true] {
            let r = FakeRunner::failing(&["service start"]);
            let (result, dropped) = start(&r, ours, USER);
            let err = result.unwrap_err();
            assert_eq!(r.calls(), vec!["service start"], "ours = {ours}");
            assert!(err.contains("background service didn't start"), "{err}");
            assert!(
                err.contains("service start failed"),
                "the real error is kept: {err}"
            );
            if cfg!(target_os = "linux") {
                assert!(err.contains("systemctl --user status freenet"), "{err}");
                // The way out without a user session is in every such message, not just the
                // first one, or a second attempt leaves the user with a command that fails.
                assert!(
                    err.contains("remove `~/.config/systemd/user/freenet.service`"),
                    "{err}"
                );
                assert!(
                    err.contains(&format!("`sudo '{BIN}' service install --system`")),
                    "{err}"
                );
                assert!(err.contains("sudo systemctl start freenet"), "{err}");
                assert!(!err.contains("also a system service"), "{err}");
            }
            assert_eq!(
                dropped, 1,
                "a unit exists, so River's record has done its job"
            );
        }
    }

    #[test]
    fn a_leftover_user_unit_beside_a_system_unit_gets_a_way_out() {
        // A user install that failed part-way (no user session) left a user unit; the user
        // then set up the system service. freenet-core still picks the user unit, so the
        // advice must say to remove it, and River must not touch either unit.
        let both = Services {
            user: true,
            system: true,
        };
        for ours in [false, true] {
            let r = FakeRunner::failing(&["service start"]);
            let (result, dropped) = start(&r, ours, both);
            let err = result.unwrap_err();
            assert_eq!(dropped, 1, "a unit exists, so the record goes");
            assert_eq!(r.calls(), vec!["service start"], "ours = {ours}");
            if cfg!(target_os = "linux") {
                assert!(err.contains("also a system service"), "{err}");
                assert!(
                    err.contains("remove `~/.config/systemd/user/freenet.service`"),
                    "{err}"
                );
                assert!(err.contains("sudo systemctl start freenet"), "{err}");
                // The system service is already set up: no install step this time.
                assert!(!err.contains("service install --system"), "{err}");
            }
        }
    }

    #[test]
    fn whether_river_owns_the_binary_is_only_asked_when_needed() {
        // Asking hashes a 66 MB file, so it is skipped when the answer can't matter: a start
        // that works, or any registered service.
        let asked = std::cell::Cell::new(false);
        let ours = || {
            asked.set(true);
            true
        };
        let r = FakeRunner::failing(&[]);
        start_binary(Path::new(BIN), &r, &ours, NONE, &mut || {}).unwrap();
        for services in [SYSTEM, USER] {
            let r = FakeRunner::failing(&["service start"]);
            assert!(start_binary(Path::new(BIN), &r, &ours, services, &mut || {}).is_err());
        }
        assert!(!asked.get());
    }

    #[test]
    fn registered_services_are_found_where_freenet_core_puts_them() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let h = Some(home.path());
        assert_eq!(registered_services_in("linux", h, root.path()), NONE);

        let user = home.path().join(".config/systemd/user/freenet.service");
        std::fs::create_dir_all(user.parent().unwrap()).unwrap();
        std::fs::write(&user, b"").unwrap();
        assert_eq!(registered_services_in("linux", h, root.path()), USER);
        assert_eq!(
            registered_services_in("linux", None, root.path()),
            NONE,
            "no home"
        );

        let system = root.path().join("etc/systemd/system/freenet.service");
        std::fs::create_dir_all(system.parent().unwrap()).unwrap();
        std::fs::write(&system, b"").unwrap();
        let both = Services {
            user: true,
            system: true,
        };
        assert_eq!(registered_services_in("linux", h, root.path()), both);

        let plist = home
            .path()
            .join("Library/LaunchAgents/org.freenet.node.plist");
        assert_eq!(registered_services_in("macos", h, root.path()), NONE);
        std::fs::create_dir_all(plist.parent().unwrap()).unwrap();
        std::fs::write(&plist, b"").unwrap();
        assert_eq!(registered_services_in("macos", h, root.path()), USER);
        assert_eq!(registered_services_in("windows", h, root.path()), NONE);
    }

    #[test]
    fn river_registers_the_service_for_its_own_copy() {
        let r = FakeRunner::failing(&["service start"]);
        let (result, registered) = start(&r, true, NONE);
        result.unwrap();
        assert_eq!(
            r.calls(),
            vec!["service start", "service install", "service start"]
        );
        assert_eq!(registered, 1);

        // Registered, then the start fails: the service exists, so the record still goes.
        let r = FakeRunner::failing(&["service start", "service start"]);
        let (result, registered) = start(&r, true, NONE);
        assert!(result.is_err());
        assert_eq!(registered, 1);

        // Registration fails before writing a unit: the record stays, so a retry with still
        // no unit can register. (When freenet-core got as far as writing the unit, the retry
        // sees it and reports it instead; River never registers over a unit.)
        let r = FakeRunner::failing(&["service start", "service install"]);
        assert_eq!(start(&r, true, NONE).1, 0);
    }

    #[test]
    fn registration_failures_explain_themselves_and_run_nothing_else() {
        let r = FakeRunner::failing(&["service start", "service install"]);
        let err = start(&r, true, NONE).0.unwrap_err();
        assert_eq!(r.calls(), vec!["service start", "service install"]);
        assert!(err.contains("service install failed"), "{err}");
        assert!(
            !err.contains(" network`"),
            "never suggests a bare node: {err}"
        );
        if cfg!(target_os = "linux") {
            // All the steps that make the advice work, in an order that does.
            assert!(
                err.contains("remove `~/.config/systemd/user/freenet.service`"),
                "{err}"
            );
            assert!(
                err.contains(&format!("`sudo '{BIN}' service install --system`")),
                "{err}"
            );
            assert!(err.contains("sudo systemctl start freenet"), "{err}");
            assert!(err.contains("Freenet needs systemd on Linux"), "{err}");
        }

        let r = FakeRunner::failing(&["service start", "service start"]);
        let err = start(&r, true, NONE).0.unwrap_err();
        assert_eq!(
            r.calls(),
            vec!["service start", "service install", "service start"]
        );
        assert!(err.contains("set up but didn't start"), "{err}");
    }

    #[test]
    fn someone_elses_copy_is_never_registered() {
        let r = FakeRunner::failing(&["service start"]);
        let err = start(&r, false, NONE).0.unwrap_err();
        assert_eq!(r.calls(), vec!["service start"]);
        assert!(
            err.contains(BIN) && err.contains("service start failed"),
            "{err}"
        );
    }

    #[test]
    fn a_stopped_system_service_is_left_to_the_administrator() {
        let r = FakeRunner::failing(&["service start"]);
        let (result, dropped) = start(&r, true, SYSTEM);
        let err = result.unwrap_err();
        assert_eq!(dropped, 1, "a unit exists, so the record goes");
        assert!(err.contains("sudo systemctl start freenet"), "{err}");
        assert!(
            err.contains("service start failed"),
            "the real error is kept: {err}"
        );
        assert_eq!(r.calls(), vec!["service start"]);
    }

    #[test]
    fn what_answers_on_the_port_decides_the_state() {
        // Derived from the floor, so raising it doesn't break this test.
        use release::Version;
        let floor = release::MIN_NODE_VERSION;
        let newer = Version(floor.0, floor.1 + 1, 0);
        let older = match floor {
            Version(a, b, c) if c > 0 => Version(a, b, c - 1),
            Version(a, b, _) if b > 0 => Version(a, b - 1, 999),
            Version(a, _, _) => Version(a - 1, 999, 999),
        };
        let version = |v: String| Answer::Version(v);
        assert_eq!(
            classify(version(floor.to_string()), true),
            NodeProbe::Running {
                version: floor.to_string()
            }
        );
        assert_eq!(
            classify(version(newer.to_string()), false),
            NodeProbe::Running {
                version: newer.to_string()
            }
        );
        // A suffix doesn't get a version past the floor.
        assert!(matches!(
            classify(version(format!("{older}-rc.1")), true),
            NodeProbe::Outdated { .. }
        ));
        assert!(matches!(
            classify(version(format!("{newer}+build.7")), true),
            NodeProbe::Running { .. }
        ));
        // ...and doesn't push a newer version below it either.
        assert!(matches!(
            classify(version(format!("{newer}-rc.1")), true),
            NodeProbe::Running { .. }
        ));
        // A pre-release of the floor itself comes before it; build metadata doesn't.
        assert!(matches!(
            classify(version(format!("{floor}-rc.1")), true),
            NodeProbe::Outdated { .. }
        ));
        assert!(matches!(
            classify(version(format!("{floor}+build.7")), true),
            NodeProbe::Running { .. }
        ));
        assert_eq!(
            classify(version(older.to_string()), true),
            NodeProbe::Outdated {
                running: Some(older.to_string()),
                minimum: floor.to_string()
            }
        );
        // A version River can't parse came from the endpoint, so the node has it.
        assert!(matches!(
            classify(version("nightly".into()), true),
            NodeProbe::Running { .. }
        ));
        assert_eq!(
            classify(Answer::NoVersion, true),
            NodeProbe::Outdated {
                running: None,
                minimum: floor.to_string()
            }
        );
        assert_eq!(
            classify(Answer::Nothing, true),
            NodeProbe::InstalledNotRunning
        );
        assert_eq!(classify(Answer::Nothing, false), NodeProbe::NotInstalled);
    }

    /// Serve one canned HTTP response on a local port and return its URL.
    fn serve_once(response: &'static str) -> String {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/version", listener.local_addr().unwrap());
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Read the whole request before answering: closing with unread data can reset the
            // connection and lose the response.
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
            }
            let _ = stream.write_all(response.as_bytes());
        });
        url
    }

    #[test]
    fn what_the_port_says_is_read_correctly() {
        let agent = probe_agent();
        let ok = "HTTP/1.1 200 OK\r\ncontent-length: 23\r\n\r\n{\"version\":\"0.2.155\"}\r\n";
        assert_eq!(
            ask(&agent, &serve_once(ok)),
            Answer::Version("0.2.155".into())
        );
        let html = "HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello";
        assert_eq!(ask(&agent, &serve_once(html)), Answer::NoVersion);
        let missing = "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n";
        assert_eq!(ask(&agent, &serve_once(missing)), Answer::NoVersion);
        // A proxy in front of a node that isn't up yet answers 502-504: not ready, not outdated.
        for status in [
            "502 Bad Gateway",
            "503 Service Unavailable",
            "504 Gateway Timeout",
        ] {
            let response: &'static str = Box::leak(
                format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\n\r\n").into_boxed_str(),
            );
            assert_eq!(
                ask(&agent, &serve_once(response)),
                Answer::Nothing,
                "{status}"
            );
        }
        // Freenet never answers its version endpoint with these, so whatever does isn't a
        // usable Freenet and is reported rather than waited on.
        let auth = "HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\n\r\n";
        assert_eq!(ask(&agent, &serve_once(auth)), Answer::NoVersion);
        let broken = "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\n\r\n";
        assert_eq!(ask(&agent, &serve_once(broken)), Answer::NoVersion);
        // A redirect is not followed: the node's endpoint never redirects.
        let moved =
            "HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1:9/x\r\ncontent-length: 0\r\n\r\n";
        assert_eq!(ask(&agent, &serve_once(moved)), Answer::NoVersion);

        // Something accepts the connection but never answers, so the call times out: a
        // transport failure, which `ask` treats the same way as a refused connection. The
        // listener is held for the whole call, so no other program can take the port.
        let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/version", silent.local_addr().unwrap());
        assert_eq!(ask(&agent, &url), Answer::Nothing);
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

    #[test]
    fn the_reported_line_is_the_error_not_a_trailing_hint() {
        // freenet-core's `bail!` with a hint: the message, not the trailing `Try:` line.
        let hinted = "Error: Failed to reload systemd\n\nHint: User systemd session not \
                      available\nTry: sudo freenet service install --system\n";
        assert_eq!(error_line(hinted), "Failed to reload systemd");
        // anyhow `.context(...)`: the message and its root cause, which is what to fix.
        let with_cause = "Error: Failed to create systemd user directory\n\nCaused by:\n    \
                          Permission denied (os error 13)\n";
        assert_eq!(
            error_line(with_cause),
            "Failed to create systemd user directory: Permission denied (os error 13)"
        );
        // Several causes, numbered, then a backtrace: the last cause, unnumbered.
        let chain = "Error: Failed to write service file\n\nCaused by:\n    0: writing unit\n    \
                     1: No space left on device (os error 28)\n\nStack backtrace:\n   0: main\n";
        assert_eq!(
            error_line(chain),
            "Failed to write service file: No space left on device (os error 28)"
        );
        // clap's lowercase `error:`, before its usage text.
        let clap = "error: unrecognized subcommand 'service'\n\nUsage: freenet [OPTIONS]\n\n\
                    For more information, try '--help'.\n";
        assert_eq!(error_line(clap), "unrecognized subcommand 'service'");
        // Several `Error:` lines (other output first, freenet-core's error last): the last.
        assert_eq!(error_line("Error: outer\nError: inner\nHint: x\n"), "inner");
        // An indented cause that begins with "error:" is a cause, not the message.
        let cause_says_error = "Error: Failed to X\n\nCaused by:\n    error: Y\n";
        assert_eq!(error_line(cause_says_error), "Failed to X: error: Y");
        // An `Error:` that only ever appears indented (a wrapper indenting output) still
        // counts, rather than falling through to a trailing hint.
        let indented = "  Error: Failed to Z\n  Try: something\n";
        assert_eq!(error_line(indented), "Failed to Z");
        // Anything else: its last line.
        assert_eq!(error_line("first\nboom\n\n"), "boom");
        assert_eq!(error_line(""), "no error output");
        assert_eq!(error_line("Error:\nreal problem\n"), "real problem");
    }

    #[test]
    fn a_suggested_command_survives_any_path() {
        assert_eq!(
            shell_quote("/home/u/.local/bin/freenet"),
            "'/home/u/.local/bin/freenet'"
        );
        assert_eq!(shell_quote("/a b/$x/`y`/\"z\""), "'/a b/$x/`y`/\"z\"'");
        assert_eq!(shell_quote("/it's/freenet"), r"'/it'\''s/freenet'");
        // And the advice actually uses it.
        let recipe = no_user_session_recipe(Path::new("/a b/it's/freenet"));
        assert!(
            recipe.contains(r"`sudo '/a b/it'\''s/freenet' service install --system`"),
            "{recipe}"
        );
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
