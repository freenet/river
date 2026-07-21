//! Behavioural tests for `riverctl identity whoami` (freenet/river#438).
//!
//! These run the REAL binary with `--node-url` pointed at a closed port, which
//! is the only way to prove the thing the feature actually promises: that a
//! bridge can learn its own member ID with the node down, before any message
//! has arrived.
//!
//! The unit tests in `cli/src/` cover the derivation and the JSON payload
//! shape; the wiring pins in `commands/identity.rs` scrape `main.rs`. Neither
//! can catch the regression that matters most here — someone routing whoami
//! back through `ApiClient` (which opens a WebSocket eagerly), or dropping the
//! `else` so the short-circuit falls through into client construction. Both
//! leave the source scrapes green and fail here immediately.
//!
//! Unlike `message_flow.rs`, these need no Freenet node, so they run in normal
//! CI rather than being `#[ignore]`d.

use assert_cmd::cargo::cargo_bin_cmd;
use tempfile::TempDir;

/// A node URL that cannot connect: port 1 is never a Freenet node. Any code
/// path that tries to reach the node fails fast rather than hanging.
const DEAD_NODE_URL: &str = "ws://127.0.0.1:1/v1/contract/command?encodingProtocol=native";

fn whoami(config_dir: &TempDir, args: &[&str]) -> std::process::Output {
    let mut cmd = cargo_bin_cmd!("riverctl");
    cmd.args(["--config-dir", config_dir.path().to_str().unwrap()])
        .args(["--node-url", DEAD_NODE_URL])
        // Keep the once/day crates.io nudge out of a hermetic test.
        .arg("--no-version-check")
        .args(["identity", "whoami"])
        .args(args)
        // A stale RIVER_SIGNING_KEY* in the developer's environment would
        // otherwise silently change which identity is reported.
        .env_remove("RIVER_SIGNING_KEY")
        .env_remove("RIVER_SIGNING_KEY_FILE")
        .env_remove("RIVER_CONFIG_DIR");
    cmd.output().expect("riverctl binary runs")
}

/// With an inline `--signing-key`, whoami needs no local storage at all — and
/// must still succeed with the node unreachable. This is the bridge/bot case:
/// `message send --signing-key` works without local storage, so whoami must
/// too, or the identity it reports cannot be checked against those messages.
#[test]
fn whoami_with_inline_key_succeeds_with_no_node_and_no_storage() {
    let dir = TempDir::new().unwrap();
    // Base64 of 32 bytes, the encoding `message send --signing-key` accepts.
    let key_b64 = base64_32([7u8; 32]);
    let room = bs58_room_key();

    let out = whoami(
        &dir,
        &["--format", "json", &room, "--signing-key", &key_b64],
    );
    assert!(
        out.status.success(),
        "whoami must succeed offline with an inline key.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stdout is JSON");
    assert!(
        json.is_object(),
        "a single named room must emit ONE object, not an array — the \
         documented `| jq -r .member_id` depends on it. Got: {json}"
    );
    assert_eq!(json["room"], room);
    assert_eq!(
        json["signing_key_source"], "inline",
        "an inline key must be disclosed as such, so a bridge can tell which \
         of the three override mechanisms won"
    );
    assert!(
        json["member_id"].as_str().is_some_and(|s| !s.is_empty()),
        "member_id must be present: it is the whole point"
    );
    // Nickname is unknowable without the room's state.
    assert_eq!(json["nickname"], serde_json::Value::Null);
}

/// The same key always yields the same member ID — the property a bridge
/// relies on when it re-runs whoami between restarts.
#[test]
fn whoami_inline_key_is_deterministic() {
    let dir = TempDir::new().unwrap();
    let key_b64 = base64_32([11u8; 32]);
    let room = bs58_room_key();
    let args = ["--format", "json", &room, "--signing-key", &key_b64];

    let first = whoami(&dir, &args);
    let second = whoami(&dir, &args);
    assert!(first.status.success() && second.status.success());

    let a: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    let b: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(a["member_id"], b["member_id"]);
    assert_ne!(a["member_id"], serde_json::Value::Null);
}

/// A different key must yield a different member ID — otherwise the override
/// plumbing is silently ignoring the key and reporting something else.
#[test]
fn whoami_distinct_keys_yield_distinct_member_ids() {
    let dir = TempDir::new().unwrap();
    let room = bs58_room_key();

    let one = whoami(
        &dir,
        &[
            "--format",
            "json",
            &room,
            "--signing-key",
            &base64_32([1u8; 32]),
        ],
    );
    let two = whoami(
        &dir,
        &[
            "--format",
            "json",
            &room,
            "--signing-key",
            &base64_32([2u8; 32]),
        ],
    );
    let a: serde_json::Value = serde_json::from_slice(&one.stdout).unwrap();
    let b: serde_json::Value = serde_json::from_slice(&two.stdout).unwrap();
    assert_ne!(a["member_id"], b["member_id"]);
}

/// `RIVER_SIGNING_KEY` must work exactly like `--signing-key`: it is the form
/// an automation bridge actually uses (exported in a profile or unit file),
/// and it is what `message send` reads. If whoami ignored it, it would report
/// an identity matching none of that bridge's own messages.
#[test]
fn whoami_reads_river_signing_key_env() {
    let dir = TempDir::new().unwrap();
    let key_b64 = base64_32([23u8; 32]);
    let room = bs58_room_key();

    let mut cmd = cargo_bin_cmd!("riverctl");
    let out = cmd
        .args(["--config-dir", dir.path().to_str().unwrap()])
        .args(["--node-url", DEAD_NODE_URL])
        .arg("--no-version-check")
        .args(["identity", "whoami", "--format", "json", &room])
        .env("RIVER_SIGNING_KEY", &key_b64)
        .env_remove("RIVER_SIGNING_KEY_FILE")
        .env_remove("RIVER_CONFIG_DIR")
        .output()
        .expect("riverctl binary runs");
    assert!(
        out.status.success(),
        "RIVER_SIGNING_KEY must be honoured.\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let from_env: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(from_env["signing_key_source"], "inline");

    // Identical to passing the same key on the command line.
    let from_flag = whoami(
        &dir,
        &["--format", "json", &room, "--signing-key", &key_b64],
    );
    let from_flag: serde_json::Value = serde_json::from_slice(&from_flag.stdout).unwrap();
    assert_eq!(from_env["member_id"], from_flag["member_id"]);
}

/// With no rooms and no key, whoami still exits 0 offline and emits an empty
/// JSON array — a bridge enumerating rooms shouldn't have to special-case a
/// crash on first run.
#[test]
fn whoami_all_rooms_empty_storage_is_empty_array_offline() {
    let dir = TempDir::new().unwrap();
    let out = whoami(&dir, &["--format", "json"]);
    assert!(
        out.status.success(),
        "whoami over an empty store must succeed offline.\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json, serde_json::json!([]));
}

/// An unknown room with no inline key is a user error, not a crash: exit
/// non-zero with a message naming the room and the ways forward. It must fail
/// on the LOOKUP, not on a connection attempt.
#[test]
fn whoami_unknown_room_errors_without_contacting_node() {
    let dir = TempDir::new().unwrap();
    let room = bs58_room_key();
    let out = whoami(&dir, &[&room]);
    assert!(!out.status.success(), "unknown room must exit non-zero");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not found in local storage"),
        "error should explain the room is not stored locally, got: {stderr}"
    );
    assert!(
        !stderr.contains("WebSocket") && !stderr.contains("Connection refused"),
        "whoami must not attempt a node connection — it is a local lookup. \
         Got a connection error instead: {stderr}"
    );
}

/// A malformed inline key is rejected with a clear message rather than
/// silently reporting some other identity.
#[test]
fn whoami_rejects_malformed_inline_key() {
    let dir = TempDir::new().unwrap();
    let room = bs58_room_key();

    let bad_b64 = whoami(&dir, &[&room, "--signing-key", "not-valid-base64!!"]);
    assert!(!bad_b64.status.success());
    assert!(String::from_utf8_lossy(&bad_b64.stderr).contains("base64"));

    // Valid base64, wrong length.
    let short = whoami(&dir, &[&room, "--signing-key", &base64_of(&[9u8; 16])]);
    assert!(!short.status.success());
    assert!(String::from_utf8_lossy(&short.stderr).contains("32 bytes"));
}

fn base64_32(bytes: [u8; 32]) -> String {
    base64_of(&bytes)
}

fn base64_of(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// A syntactically valid base58 room key (32 bytes) that is not in storage.
fn bs58_room_key() -> String {
    // A valid Ed25519 verifying key so parsing succeeds and the failure under
    // test is "not in storage", not "malformed key".
    let sk = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);
    bs58::encode(sk.verifying_key().as_bytes()).into_string()
}
