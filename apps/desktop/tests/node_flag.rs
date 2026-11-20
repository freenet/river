//! The `--node` flag and `FREENET_NODE`, through the real binary (`--node-check` reports what the
//! arguments name and exits, without a window).

use std::process::{Command, Output};

fn river(args: &[&str], env: &[(&str, &str)]) -> Output {
    let home = std::env::temp_dir().join(format!("river-node-flag-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_river"));
    cmd.args(args)
        .env_remove("FREENET_NODE")
        // An empty home: no saved node address from whoever runs the test.
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("APPDATA", home.join("appdata"))
        .env("LOCALAPPDATA", home.join("localappdata"));
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("river runs")
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).replace("\r\n", "\n")
}

#[test]
fn a_good_flag_is_reported_normalised() {
    let out = river(&["--node-check", "--node", "LocalHost:40302"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout).trim(), "node localhost:40302");
}

#[test]
fn a_bad_flag_exits_2_and_says_why() {
    for bad in ["a@b", "http://x:7509/", "host:99999", "two words:1"] {
        let out = river(&["--node-check", "--node", bad], &[]);
        assert_eq!(out.status.code(), Some(2), "{bad}");
        let err = text(&out.stderr);
        assert!(
            err.contains("--node") && err.contains("isn't a node address"),
            "{err}"
        );
        assert!(text(&out.stdout).is_empty());
    }
}

#[test]
fn the_variable_is_used_and_the_flag_beats_it() {
    let out = river(&["--node-check"], &[("FREENET_NODE", "127.0.0.1:40303")]);
    assert_eq!(text(&out.stdout).trim(), "node 127.0.0.1:40303");
    let out = river(
        &["--node-check", "--node=127.0.0.1:40302"],
        &[("FREENET_NODE", "127.0.0.1:40303")],
    );
    assert_eq!(text(&out.stdout).trim(), "node 127.0.0.1:40302");
    let out = river(&["--node-check"], &[("FREENET_NODE", "a@b")]);
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains("FREENET_NODE"));
}

#[test]
fn with_nothing_given_the_node_is_this_computers() {
    let out = river(&["--node-check"], &[]);
    assert_eq!(text(&out.stdout).trim(), "node 127.0.0.1:7509");
}
