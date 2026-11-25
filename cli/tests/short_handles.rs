// Test code stamps arbitrary times; the rule is for what production signs (freenet/river#971).
#![allow(clippy::disallowed_methods)]
//! `riverctl room short-link` / `room resolve-handle` (freenet/river#973).
//!
//! The publish -> resolve round trip needs a Freenet node, so it runs only when
//! `RIVERCTL_TEST_NODE_URL` names one (a LOCAL, disposable node, never a shared
//! one: the test PUTs a resolver entry for a throwaway key). Without it the test
//! prints a skip line and passes, so CI needs no node. Example:
//!
//! ```text
//! freenet local --ws-api-port 7611 --config-dir D/cfg --data-dir D/data --log-dir D/log &
//! RIVERCTL_TEST_NODE_URL='ws://127.0.0.1:7611/v1/contract/command?encodingProtocol=native' \
//!   cargo test -p riverctl --test short_handles -- --nocapture
//! ```

use assert_cmd::cargo::cargo_bin_cmd;
use ed25519_dalek::SigningKey;
use serde_json::Value;
use tempfile::TempDir;

const DEAD_NODE_URL: &str = "ws://127.0.0.1:1/v1/contract/command?encodingProtocol=native";

fn run(node_url: &str, config_dir: &TempDir, args: &[&str]) -> std::process::Output {
    cargo_bin_cmd!("riverctl")
        .args(["--config-dir", config_dir.path().to_str().unwrap()])
        .args(["--node-url", node_url])
        .arg("--no-version-check")
        .args(["--format", "json"])
        .args(args)
        .env_remove("RIVER_SIGNING_KEY")
        .env_remove("RIVER_SIGNING_KEY_FILE")
        .env_remove("RIVER_CONFIG_DIR")
        .output()
        .expect("riverctl binary runs")
}

fn json(out: &std::process::Output) -> Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let start = stdout.find('{').unwrap_or_else(|| {
        panic!(
            "no JSON in stdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    serde_json::from_str(&stdout[start..]).expect("stdout is JSON")
}

#[test]
fn help_describes_the_commands_without_a_node() {
    let dir = TempDir::new().unwrap();
    for (sub, needle) in [("short-link", "--publish"), ("resolve-handle", "<HANDLE>")] {
        let out = run(DEAD_NODE_URL, &dir, &["room", sub, "--help"]);
        assert!(out.status.success(), "{sub} --help failed");
        assert!(String::from_utf8_lossy(&out.stdout).contains(needle));
    }
}

#[test]
fn publish_then_resolve_round_trip() {
    let Ok(node_url) = std::env::var("RIVERCTL_TEST_NODE_URL") else {
        eprintln!("SKIP publish_then_resolve_round_trip: RIVERCTL_TEST_NODE_URL is not set");
        return;
    };
    let dir = TempDir::new().unwrap();
    // A fresh throwaway key per run, so a re-run never finds an entry from an earlier one.
    let sk = SigningKey::from_bytes(&rand::random::<[u8; 32]>());
    let owner = bs58::encode(sk.verifying_key().as_bytes()).into_string();

    let before = json(&run(&node_url, &dir, &["room", "short-link", &owner]));
    assert_eq!(before["entry_exists"], Value::Bool(false), "{before}");
    let handle = before["handle"].as_str().unwrap().to_string();
    assert!(
        before["short_link"]
            .as_str()
            .unwrap()
            .ends_with(&format!("?room={handle}")),
        "{before}"
    );

    let published = run(
        &node_url,
        &dir,
        &["room", "short-link", &owner, "--publish"],
    );
    assert!(published.status.success(), "{published:?}");
    assert_eq!(json(&published)["published"], Value::Bool(true));

    // Idempotent: a second publish finds the entry and sends nothing.
    let again = json(&run(
        &node_url,
        &dir,
        &["room", "short-link", &owner, "--publish"],
    ));
    assert_eq!(again["published"], Value::Bool(false), "{again}");

    let resolved = json(&run(&node_url, &dir, &["room", "resolve-handle", &handle]));
    assert_eq!(
        resolved["room_id"],
        Value::String(owner.clone()),
        "{resolved}"
    );

    // A full key is not a handle.
    let out = run(&node_url, &dir, &["room", "resolve-handle", &owner]);
    assert!(!out.status.success());

    // A handle nobody published: a clear failure, not a wrong key.
    let missing = bs58::encode([9u8; 16]).into_string();
    let out = run(&node_url, &dir, &["room", "resolve-handle", &missing]);
    assert!(!out.status.success());
}
