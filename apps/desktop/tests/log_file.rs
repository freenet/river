//! The diagnostics log, end to end: a real process starts the redirect (this file includes
//! `src/log.rs`), writes with `eprintln!`, and the lines must be in the file. The probe is
//! this test binary run again with `RIVER_LOG_PROBE` set, so each case has its own process and
//! this test's own stderr is never the one redirected. Unix only: the Windows redirect is
//! built by CI and checked by hand (see the PR).
#![cfg(unix)]
#![allow(dead_code)]

#[path = "../src/log.rs"]
mod log;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// The probe. Does nothing in a normal run. Run as a child with `RIVER_LOG_PROBE=<config dir>
/// <off|file|tee> <command>...` (see `run`), it starts the log as `main` does and executes
/// the commands: `say:TEXT`, `partial:TEXT` (no newline), `child:TEXT` (a child process
/// writes TEXT to its stderr), `bulk:N` (N lines of about 1000 bytes) or `panic`.
#[test]
fn probe_entry() {
    let Ok(spec) = std::env::var("RIVER_LOG_PROBE") else {
        return;
    };
    let args: Vec<&str> = spec.split('\u{1f}').collect();
    let mode = match args[1] {
        "off" => log::Mode::Off,
        "tee" => log::Mode::Tee,
        _ => log::Mode::File,
    };
    log::start(Path::new(args[0]), mode);
    for cmd in &args[2..] {
        match cmd.split_once(':') {
            Some(("say", t)) => eprintln!("{t}"),
            Some(("partial", t)) => eprint!("{t}"),
            Some(("child", t)) => {
                let status = Command::new("sh")
                    .args(["-c", &format!("echo '{t}' >&2")])
                    .status()
                    .unwrap();
                assert!(status.success());
            }
            Some(("bulk", n)) => {
                for i in 0..n.parse::<u32>().unwrap() {
                    eprintln!("bulk {i:06} {}", "z".repeat(980));
                }
            }
            _ if *cmd == "panic" => panic!("probe panic message"),
            _ => panic!("unknown command {cmd}"),
        }
    }
    log::flush();
}

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("river-log-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(dir: &Path, mode: &str, cmds: &[&str]) -> Output {
    let mut spec = vec![dir.to_str().unwrap(), mode];
    spec.extend_from_slice(cmds);
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "probe_entry", "--nocapture", "--test-threads=1"])
        .env("RIVER_LOG_PROBE", spec.join("\u{1f}"))
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn log_of(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("logs/river.log")).unwrap_or_default()
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
fn eprintln_lands_in_the_file_with_a_timestamp_and_not_on_stderr() {
    let dir = temp("file");
    let out = run(&dir, "file", &["say:hello from eprintln"]);
    assert!(out.status.success(), "{out:?}");
    let log = log_of(&dir);
    assert!(
        log.lines().any(|l| stamped(l, "hello from eprintln")),
        "{log}"
    );
    assert!(log.lines().next().unwrap().contains("River"), "{log}");
    assert!(out.stderr.is_empty(), "stderr was redirected: {out:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The control: without the redirect, the same program writes nothing to the file. This is
/// what shows the test above is checking the redirect and not some other writer.
#[test]
fn without_the_redirect_the_line_goes_to_stderr_and_not_the_file() {
    let dir = temp("off");
    let out = run(&dir, "off", &["say:hello from eprintln"]);
    assert!(!log_of(&dir).contains("hello from eprintln"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("hello from eprintln"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_terminal_run_prints_as_before_and_also_logs() {
    let dir = temp("tee");
    let out = run(&dir, "tee", &["say:one", "partial:two"]);
    assert_eq!(String::from_utf8_lossy(&out.stderr), "one\ntwo");
    let log = log_of(&dir);
    assert!(log.lines().any(|l| stamped(l, "one")), "{log}");
    assert!(log.lines().any(|l| stamped(l, "two")), "{log}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_child_process_writes_to_the_same_log() {
    let dir = temp("child");
    run(&dir, "file", &["child:from the child"]);
    assert!(
        log_of(&dir).lines().any(|l| stamped(l, "from the child")),
        "{}",
        log_of(&dir)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_panic_message_is_in_the_file_when_the_process_ends() {
    for i in 0..20 {
        let dir = temp(&format!("panic{i}"));
        let out = run(&dir, "file", &["say:before", "panic"]);
        assert!(!out.status.success());
        let log = log_of(&dir);
        assert!(log.contains("probe panic message"), "run {i}: {log}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn the_file_stays_near_two_megabytes_and_keeps_the_current_runs_last_lines() {
    let dir = temp("bulk");
    // About 5 MB in one run.
    run(&dir, "file", &["bulk:5000", "say:the very last line"]);
    let current = std::fs::metadata(dir.join("logs/river.log")).unwrap().len();
    let previous = std::fs::metadata(dir.join("logs/river.log.1"))
        .unwrap()
        .len();
    assert!(current <= 2 * 1024 * 1024, "{current}");
    assert!(
        previous <= 2 * 1024 * 1024 && previous > 1024 * 1024,
        "{previous}"
    );
    assert!(!dir.join("logs/river.log.2").exists());
    let log = log_of(&dir);
    assert!(log.contains("bulk 004999"), "newest bulk line kept");
    assert!(log.contains("the very last line"));
    assert!(!log.contains("bulk 000000"), "the oldest were dropped");
    // A new run keeps appending and does not start over.
    run(&dir, "file", &["say:second run"]);
    assert!(log_of(&dir).contains("the very last line") && log_of(&dir).contains("second run"));
    let _ = std::fs::remove_dir_all(&dir);
}
