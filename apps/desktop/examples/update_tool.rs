// Test code stamps arbitrary times; the rule is for what production signs (freenet/river#971).
#![allow(clippy::disallowed_methods)]
//! The release-signing tool for River's self-update. Not shipped in the app.
//!
//! ```text
//! update_tool keygen <private-key-file>
//!     Make a new signing key. Writes the private key (hex, mode 0600) to the file and prints
//!     the public key. Refuses to overwrite a file.
//!
//! update_tool sign --key <private-key-file> --version 0.2.0 --out <dir> \
//!       --artifact linux-x86_64=https://host/River-0.2.0-linux-x86_64.tar.gz=./River-0.2.0-linux-x86_64.tar.gz \
//!       [--artifact ...] [--sequence N] [--released-at UNIX] [--expires-in-days 30] \
//!       [--min-supported 0.0.0] [--channel stable] [--notes TEXT]
//!     Hash each artifact, write <dir>/manifest.json and <dir>/manifest.json.sig.
//!     --sequence defaults to the current unix time, which rises with every signing.
//!
//! update_tool verify --pubkey <hex> --manifest <file> --sig <file>
//!     Check a manifest the way the app does.
//! ```
//!
//! # Where the key lives, and rotating it
//!
//! The public half is committed in `apps/desktop/update-pubkey.hex` (64 lowercase hex digits and
//! a newline). The release workflow reads it into `RIVER_UPDATE_PUBKEY_HEX` for the build and
//! checks the built binary against it. The repository variable of that name is no longer used.
//! The private half stays in `~/.freenet-signing/` and 1Password, never in the repository.
//!
//! A new key is a new release, because the key is compiled into each build: run
//! `update_tool keygen <file>`, commit the printed public key to `update-pubkey.hex` in a
//! reviewed PR, then cut a release. Installed versions keep trusting the old key until they see
//! a signed update-key record naming the new one (`scripts/sign-update-key-record.sh`,
//! `apps/desktop/update-authority.toml`): publish that record BEFORE the old key is retired, and
//! pin it into the release. An install that cannot reach its node keeps the key it has.
//!
//! # Publishing a release
//!
//! 1. Sign: `update_tool sign ... --out <dir>` (with `--expires-in-days 30`, the default).
//! 2. Copy `manifest.json` and `manifest.json.sig` into `hugo-site/static/river/update/stable/`
//!    of the freenet/web repository and merge it. freenet.org is GitHub Pages: a deploy
//!    publishes both files together, and caches serve the old pair for up to about ten minutes.
//! 3. Sign again before `expires_at`, even with no new release, or clients stop seeing updates
//!    ("the update manifest has expired"). A monthly re-sign keeps the 30-day window open.
//!
//! Keep the private key off any machine that builds untrusted code, and out of CI logs.

#![allow(dead_code)]

#[path = "../src/update/manifest.rs"]
mod manifest;
#[path = "../src/release.rs"]
mod release;

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::process::exit;
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};

use manifest::{Artifact, FORMAT, Manifest, signed_bytes, verify_and_parse};

fn die(msg: &str) -> ! {
    eprintln!("update_tool: {msg}");
    exit(1);
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("keygen") => keygen(
            args.next()
                .unwrap_or_else(|| die("keygen needs a file name")),
        ),
        Some("sign") => sign(args.collect()),
        Some("verify") => verify(args.collect()),
        _ => die("usage: update_tool keygen|sign|verify (see the top of examples/update_tool.rs)"),
    }
}

fn keygen(path: String) {
    let mut seed = [0u8; 32];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut seed))
        .unwrap_or_else(|e| die(&format!("no randomness: {e}")));
    let key = SigningKey::from_bytes(&seed);
    let mut file = fs::OpenOptions::new();
    file.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        file.mode(0o600);
    }
    use std::io::Write;
    file.open(&path)
        .and_then(|mut f| writeln!(f, "{}", hex::encode(seed)))
        .unwrap_or_else(|e| die(&format!("couldn't write {path}: {e}")));
    println!("{}", hex::encode(key.verifying_key().to_bytes()));
}

fn load_key(path: &str) -> SigningKey {
    let text =
        fs::read_to_string(path).unwrap_or_else(|e| die(&format!("couldn't read {path}: {e}")));
    let seed: [u8; 32] = hex::decode(text.trim())
        .ok()
        .and_then(|b| b.try_into().ok())
        .unwrap_or_else(|| die("the key file does not hold 64 hex digits"));
    SigningKey::from_bytes(&seed)
}

/// `--name value` pairs, with `--artifact` allowed more than once.
fn options(args: Vec<String>) -> (BTreeMap<String, String>, Vec<String>) {
    let (mut single, mut artifacts) = (BTreeMap::new(), Vec::new());
    let mut it = args.into_iter();
    while let Some(flag) = it.next() {
        let name = flag
            .strip_prefix("--")
            .unwrap_or_else(|| die(&format!("unexpected {flag}")));
        let value = it
            .next()
            .unwrap_or_else(|| die(&format!("--{name} needs a value")));
        if name == "artifact" {
            artifacts.push(value);
        } else {
            single.insert(name.to_string(), value);
        }
    }
    (single, artifacts)
}

fn sign(args: Vec<String>) {
    let (o, artifacts) = options(args);
    let get = |k: &str| {
        o.get(k)
            .cloned()
            .unwrap_or_else(|| die(&format!("--{k} is required")))
    };
    let key = load_key(&get("key"));
    let version = get("version");
    let released_at = o.get("released-at").map_or_else(now, |v| {
        v.parse().unwrap_or_else(|_| die("bad --released-at"))
    });
    let days: u64 = o.get("expires-in-days").map_or(30, |v| {
        v.parse().unwrap_or_else(|_| die("bad --expires-in-days"))
    });
    let mut map = BTreeMap::new();
    for spec in artifacts {
        let parts: Vec<&str> = spec.splitn(3, '=').collect();
        let [platform, url, path] = parts[..] else {
            die("--artifact is PLATFORM=URL=PATH")
        };
        let bytes = fs::read(path).unwrap_or_else(|e| die(&format!("couldn't read {path}: {e}")));
        map.insert(
            platform.to_string(),
            Artifact {
                url: url.to_string(),
                sha256: hex::encode(Sha256::digest(&bytes)),
                size: bytes.len() as u64,
            },
        );
    }
    let manifest = Manifest {
        format: FORMAT,
        channel: o.get("channel").cloned().unwrap_or_else(|| "stable".into()),
        min_supported: o
            .get("min-supported")
            .cloned()
            .unwrap_or_else(|| "0.0.0".into()),
        version,
        sequence: o
            .get("sequence")
            .map_or_else(now, |v| v.parse().unwrap_or_else(|_| die("bad --sequence"))),
        released_at,
        expires_at: days
            .checked_mul(86_400)
            .and_then(|d| released_at.checked_add(d))
            .unwrap_or_else(|| die("--expires-in-days is too large")),
        notes: o.get("notes").cloned().unwrap_or_default(),
        artifacts: map,
    };
    let bytes = manifest.canonical_bytes();
    let signature = key.sign(&signed_bytes(&bytes)).to_bytes();
    // The same checks the app makes, so a manifest it would refuse is never written.
    if let Err(e) = verify_and_parse(&bytes, &signature, &key.verifying_key().to_bytes()) {
        die(&format!("the manifest is not valid: {e}"));
    }
    let out = get("out");
    fs::create_dir_all(&out).unwrap_or_else(|e| die(&format!("couldn't create {out}: {e}")));
    fs::write(format!("{out}/manifest.json"), &bytes).unwrap_or_else(|e| die(&e.to_string()));
    fs::write(format!("{out}/manifest.json.sig"), signature)
        .unwrap_or_else(|e| die(&e.to_string()));
    println!(
        "signed river {} (sequence {})",
        manifest.version, manifest.sequence
    );
}

fn verify(args: Vec<String>) {
    let (o, _) = options(args);
    let get = |k: &str| {
        o.get(k)
            .cloned()
            .unwrap_or_else(|| die(&format!("--{k} is required")))
    };
    let key: [u8; 32] = hex::decode(get("pubkey"))
        .ok()
        .and_then(|b| b.try_into().ok())
        .unwrap_or_else(|| die("bad --pubkey"));
    let manifest = fs::read(get("manifest")).unwrap_or_else(|e| die(&e.to_string()));
    let sig = fs::read(get("sig")).unwrap_or_else(|e| die(&e.to_string()));
    match verify_and_parse(&manifest, &sig, &key) {
        Ok(m) => println!(
            "valid: river {} (sequence {}, {} artifacts)",
            m.version,
            m.sequence,
            m.artifacts.len()
        ),
        Err(e) => die(&format!("invalid: {e}")),
    }
}
