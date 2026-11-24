// Test code stamps arbitrary times; the rule is for what production signs (freenet/river#971).
#![allow(clippy::disallowed_methods)]
//! A River that was just swapped in and is run with a bad option, over and over, must not be
//! rolled back: a start that exits on its command line is not a failed update. Runs the real
//! binary on a pseudo-terminal (`script`), because only a terminal makes a bad `--node` exit
//! instead of opening a window that shows the problem. Linux only; skipped without `script`.
#![cfg(target_os = "linux")]

use std::process::Command;

#[test]
fn bad_options_do_not_count_as_failed_starts() {
    if Command::new("script").arg("--version").output().is_err() {
        eprintln!("skipped: no `script` command for a pseudo-terminal");
        return;
    }
    let dir = std::env::temp_dir().join(format!("river-start-count-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let data = dir.join("data");
    let state_dir = data.join("river");
    std::fs::create_dir_all(&state_dir).unwrap();
    let version = env!("CARGO_PKG_VERSION");
    let install = dir.join("river-installed");
    std::fs::write(&install, b"the swapped-in binary").unwrap();
    let state = format!(
        r#"{{"probation":{{"from":"0.0.1","to":"{version}","install_path":"{}","previous":"{}","previous_sha256":"00","starts":0}}}}"#,
        install.display(),
        dir.join("nothing-here").display()
    );
    let state_file = state_dir.join("update-state.json");
    std::fs::write(&state_file, state).unwrap();

    for _ in 0..4 {
        let cmd = format!("'{}' --node 'bad node'", env!("CARGO_BIN_EXE_river"));
        let out = Command::new("script")
            .args(["-qec", &cmd, "/dev/null"])
            .env("HOME", &dir)
            .env("XDG_DATA_HOME", &data)
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env_remove("FREENET_NODE")
            .output()
            .expect("script runs");
        let text = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
        assert!(
            text.contains("river:"),
            "the bad option was reported: {text}"
        );
    }
    let after = std::fs::read_to_string(&state_file).unwrap();
    assert!(
        after.contains(r#""starts":0"#) || after.contains(r#""starts": 0"#),
        "{after}"
    );
    assert_eq!(
        std::fs::read(&install).unwrap(),
        b"the swapped-in binary",
        "nothing was rolled back"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
