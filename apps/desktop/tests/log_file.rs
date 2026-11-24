//! The diagnostics log, end to end. A real process starts the redirect (this file includes
//! `src/log.rs`) with a stderr of each kind, writes, and the test reads what landed where. The
//! probe is this test binary run again with `RIVER_LOG_PROBE` set, so each case has its own
//! process and this test's own stderr is never the one redirected. Unix only: the Windows
//! handle code is built by CI and checked by hand (see the PR).
#![cfg(unix)]
#![allow(dead_code)]

#[path = "../src/log.rs"]
mod log;

use std::fs::File;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const SEP: char = '\u{1f}';

/// The probe. Does nothing in a normal run. Run as a child with `RIVER_LOG_PROBE=<config dir>
/// <command>...`, it calls `log::start` as `main` does (so the choice is made from its real
/// stderr), prints `log=<path or none>` on stdout, then runs the commands: `say:TEXT`
/// (`diag!`), `eprintln:TEXT` (std's, which panics if stderr is broken), `child:TEXT` (a
/// process that inherits stderr writes TEXT), `relaunch:TEXT` (a new probe started the way the
/// updater starts River, run to the end) and `exec:TEXT` (replace this process with one),
/// `execfail:TEXT` (an exec that fails, as the updater's rollback can, then the probe goes on)
/// and `panic:TEXT`.
#[test]
fn probe_entry() {
    let Ok(spec) = std::env::var("RIVER_LOG_PROBE") else {
        return;
    };
    let args: Vec<&str> = spec.split(SEP).collect();
    // As `main` does: the failure handler first, then the log.
    river_app::diag::on_failure(log::silence_stderr);
    let started = log::start(Some(Path::new(args[0])));
    println!(
        "log={}",
        match started {
            log::Started::File(p) => p.display().to_string(),
            log::Started::Left => "none".into(),
            log::Started::Failed => "failed".into(),
        }
    );
    for cmd in &args[1..] {
        let (what, text) = cmd.split_once(':').unwrap();
        match what {
            "say" => river_app::diag!("{text}"),
            "eprintln" => eprintln!("{text}"),
            "child" => {
                let status = Command::new("sh")
                    .args(["-c", &format!("echo '{text}' >&2")])
                    .status()
                    .unwrap();
                assert!(status.success());
            }
            "relaunch" => {
                let status = nested(args[0], text).status().unwrap();
                assert!(status.success(), "the relaunched probe failed");
            }
            "exec" => {
                let err = nested(args[0], text).exec();
                panic!("exec failed: {err}");
            }
            "execfail" => {
                let err = log::exec_with_child_stderr(&mut Command::new("/nonexistent/river"));
                river_app::diag!("exec failed as expected: {} ({text})", err.kind());
            }
            "panic" => panic!("probe panic {text}"),
            _ => panic!("unknown command {cmd}"),
        }
    }
}

/// A probe started as `restart_into` and the Linux relaunch start River: stderr from
/// `log::child_stderr()`.
fn nested(dir: &str, text: &str) -> Command {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "probe_entry", "--nocapture", "--test-threads=1"])
        .env(
            "RIVER_LOG_PROBE",
            [
                dir,
                &format!("say:{text}"),
                &format!("eprintln:{text} (eprintln)"),
            ]
            .join(&SEP.to_string()),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log::child_stderr());
    cmd
}

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("river-log-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(dir: &Path, stderr: Stdio, cmds: &[&str]) -> Output {
    let mut spec = vec![dir.to_str().unwrap()];
    spec.extend_from_slice(cmds);
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "probe_entry", "--nocapture", "--test-threads=1"])
        .env("RIVER_LOG_PROBE", spec.join(&SEP.to_string()))
        .stdin(Stdio::null())
        .stderr(stderr)
        .output()
        .unwrap()
}

fn log_of(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("logs/river.log")).unwrap_or_default()
}

fn said(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `2026-11-23T14:05:09.123Z text`
fn stamped(line: &str, text: &str) -> bool {
    line.len() > 25
        && line.as_bytes()[4] == b'-'
        && line.as_bytes()[10] == b'T'
        && line.as_bytes()[23] == b'Z'
        && &line[25..] == text
}

#[test]
fn a_null_stderr_goes_to_the_file_with_timestamps() {
    let dir = temp("null");
    let out = run(
        &dir,
        Stdio::null(),
        &["say:hello from diag", "eprintln:plain eprintln"],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(
        said(&out).contains("log=") && !said(&out).contains("log=none"),
        "{}",
        said(&out)
    );
    let log = log_of(&dir);
    assert!(log.lines().any(|l| stamped(l, "hello from diag")), "{log}");
    assert!(log.contains("plain eprintln"), "{log}");
    assert!(log.lines().next().unwrap().contains("River"), "{log}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_closed_stderr_is_reopened_by_std_as_null_and_goes_to_the_file() {
    let dir = temp("closed");
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    let spec = [dir.to_str().unwrap(), "say:after close"];
    cmd.args(["--exact", "probe_entry", "--nocapture", "--test-threads=1"])
        .env("RIVER_LOG_PROBE", spec.join(&SEP.to_string()))
        .stdin(Stdio::null());
    // SAFETY: close(2) between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            libc_close_stderr();
            Ok(())
        });
    }
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(log_of(&dir).contains("after close"), "{}", log_of(&dir));
    let _ = std::fs::remove_dir_all(&dir);
}

fn libc_close_stderr() {
    // SAFETY: closing an inherited descriptor in the child before exec.
    unsafe { libc::close(2) };
}

/// The #971 recipe: `river 2> river-log.txt` keeps working, and no log file is made.
#[test]
fn stderr_sent_to_a_file_is_left_alone() {
    let dir = temp("tofile");
    let mine = dir.join("river-log.txt");
    let out = run(
        &dir,
        Stdio::from(File::create(&mine).unwrap()),
        &["say:to my file", "eprintln:plain"],
    );
    assert!(said(&out).contains("log=none"), "{}", said(&out));
    let text = std::fs::read_to_string(&mine).unwrap();
    assert!(text.lines().any(|l| stamped(l, "to my file")), "{text}");
    assert!(text.contains("plain"));
    assert!(
        !dir.join("logs").exists(),
        "no log file when stderr is a file"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `river 2>&1 | tee`, journald and a supervisor's pipe: the parent reads them.
#[test]
fn stderr_sent_to_a_pipe_is_left_alone() {
    let dir = temp("pipe");
    let out = run(&dir, Stdio::piped(), &["say:to the pipe"]);
    assert!(said(&out).contains("log=none"));
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.lines().any(|l| stamped(l, "to the pipe")), "{text}");
    assert!(!dir.join("logs").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A full disk (`/dev/full` fails every write with ENOSPC): `diag!` must not panic, and the
/// plain `eprintln!` that a dependency makes afterwards must not either, because the first
/// failed write pointed stderr at the null device.
#[test]
fn a_stderr_that_fails_every_write_never_panics_diag_or_eprintln() {
    let dir = temp("full");
    let full = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .unwrap();
    let out = run(
        &dir,
        Stdio::from(full),
        &["say:one", "say:two", "eprintln:three", "eprintln:four"],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(said(&out).contains("log=none"), "{}", said(&out));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The rollback `exec` that fails must leave this River's own stderr, the log, in place.
#[test]
fn a_failed_exec_leaves_stderr_as_it_was() {
    let dir = temp("execfail");
    let out = run(
        &dir,
        Stdio::null(),
        &["execfail:x", "say:still here", "eprintln:and here"],
    );
    assert!(out.status.success(), "{out:?}");
    let log = log_of(&dir);
    assert!(log.contains("exec failed as expected"), "{log}");
    assert!(
        log.contains("still here") && log.contains("and here"),
        "{log}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_log_file_is_private_to_the_user() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp("mode");
    run(&dir, Stdio::null(), &["say:x"]);
    let mode = std::fs::metadata(dir.join("logs/river.log"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_child_and_a_relaunched_or_exec_ed_river_write_to_the_same_file_and_none_panics() {
    let dir = temp("children");
    let out = run(
        &dir,
        Stdio::null(),
        &["child:from a child", "relaunch:relaunched", "exec:exec-ed"],
    );
    assert!(out.status.success(), "{out:?}");
    let log = log_of(&dir);
    assert!(log.contains("from a child"), "{log}");
    assert!(log.lines().any(|l| stamped(l, "relaunched")), "{log}");
    assert!(log.contains("relaunched (eprintln)"), "{log}");
    assert!(log.lines().any(|l| stamped(l, "exec-ed")), "{log}");
    assert!(log.contains("exec-ed (eprintln)"), "{log}");
    // Each started River wrote its own header: the relaunched one set up its own log.
    assert!(log.matches("--- River").count() >= 3, "{log}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_panic_message_is_in_the_file_without_the_values_it_quotes() {
    let dir = temp("panic");
    let out = run(
        &dir,
        Stdio::null(),
        &["say:before", "panic:\"my secret nickname\""],
    );
    assert!(!out.status.success());
    let log = log_of(&dir);
    assert!(log.contains("panic in thread"), "{log}");
    assert!(log.contains("probe panic"), "{log}");
    assert!(!log.contains("my secret nickname"), "{log}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_file_over_the_limit_is_rotated_when_the_next_run_starts() {
    let dir = temp("rotate");
    let path = dir.join("logs/river.log");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "old run\n".repeat(300_000)).unwrap(); // about 2.4 MB
    run(&dir, Stdio::null(), &["say:new run"]);
    let current = std::fs::read_to_string(&path).unwrap();
    assert!(
        current.contains("new run") && !current.contains("old run"),
        "{current}"
    );
    assert!(
        std::fs::read_to_string(dir.join("logs/river.log.1"))
            .unwrap()
            .contains("old run")
    );
    // Under the limit: the next run appends.
    run(&dir, Stdio::null(), &["say:third run"]);
    let again = std::fs::read_to_string(&path).unwrap();
    assert!(again.contains("new run") && again.contains("third run"));
    let _ = std::fs::remove_dir_all(&dir);
}
