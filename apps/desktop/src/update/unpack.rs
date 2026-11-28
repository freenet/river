//! Helpers the Windows and macOS installers share: the version check run on a new binary
//! before it is swapped in, and a copy that hashes what it writes.
//!
//! The Linux installer has its own copies of these ([`super::linux`]), left as they were so this
//! change does not alter Linux behaviour.

use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::release::Version;

/// The most an unpacked binary may be. A zip bomb would otherwise fill the disk.
pub const MAX_UNPACKED: u64 = 512 * 1024 * 1024;
/// How long the new binary gets to answer `--version` before the swap.
const PREFLIGHT_DEADLINE: Duration = Duration::from_secs(20);

/// Run `binary --version` and require `river <version>`.
pub fn preflight(binary: &Path, version: Version) -> Result<(), String> {
    let mut child = Command::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("the new River does not run on this computer: {e}"))?;
    let deadline = Instant::now() + PREFLIGHT_DEADLINE;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("the new River did not answer a version check in time".into());
            }
        }
    };
    let mut said = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.by_ref().take(256).read_to_string(&mut said);
    }
    if !status.success() || said.trim() != format!("river {version}") {
        return Err(format!(
            "the new River did not report version {version} (it said {:?})",
            said.trim()
        ));
    }
    Ok(())
}

/// Copy the file `from` to `to` through a temporary file in `to`'s directory, so `to` is either
/// absent, its old contents, or the whole new file. Returns the SHA-256 (hex) of what was
/// written.
pub fn copy_hashing(from: &Path, to: &Path) -> std::io::Result<String> {
    let dir = to
        .parent()
        .ok_or_else(|| std::io::Error::other("no directory"))?;
    fs::create_dir_all(dir)?;
    let mut src = fs::File::open(from)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".river-copy-")
        .tempfile_in(dir)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        tmp.write_all(&buf[..n])?;
    }
    tmp.as_file().sync_all()?;
    tmp.persist(to).map_err(|e| e.error)?;
    Ok(hex::encode(hasher.finalize()))
}

/// A real program for the installer tests to unpack, run and swap: `--version` prints
/// `river <version>`, `--sleep <secs>` stays running, `--mark` writes the same line to the file
/// named by [`MARK_ENV`]. Built with `rustc` once per version per test run, because the
/// installers must be tested on a real executable of the platform, not a script.
#[cfg(test)]
pub mod testexe {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::{Mutex, OnceLock};

    pub const MARK_ENV: &str = "RIVER_TEST_MARK";

    /// What the program does when it is started with no arguments, the way an updater starts
    /// River.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub enum Starts {
        /// Writes the handshake file the updater named, as `confirm_started` does, then exits.
        Confirms,
        /// Exits at once with status 3, as a build that crashes on start would.
        Crashes,
    }

    /// The program for `version` that confirms its start.
    pub fn build(version: &str) -> PathBuf {
        build_as(version, Starts::Confirms)
    }

    pub fn build_as(version: &str, starts: Starts) -> PathBuf {
        type Built = (tempfile::TempDir, HashMap<(String, Starts), PathBuf>);
        static BUILT: OnceLock<Mutex<Built>> = OnceLock::new();
        let built =
            BUILT.get_or_init(|| Mutex::new((tempfile::tempdir().unwrap(), HashMap::new())));
        let mut built = built.lock().unwrap();
        let id = (version.to_string(), starts);
        if let Some(p) = built.1.get(&id) {
            return p.clone();
        }
        let dir = built.0.path().join(format!("{version}-{starts:?}"));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("main.rs");
        let on_start = match starts {
            Starts::Confirms => format!(
                r#"if let Some(h) = std::env::var_os("{handshake}") {{
            std::fs::write(h, "{version}").unwrap();
        }}
        std::thread::sleep(std::time::Duration::from_millis(500));"#,
                handshake = super::super::HANDSHAKE_ENV
            ),
            Starts::Crashes => "std::process::exit(3)".to_string(),
        };
        std::fs::write(
            &src,
            format!(
                r#"// The release river.exe uses the GUI subsystem (no console), so this does too: the
// updater must read its `--version` through a pipe, which is what a release can offer.
#![cfg_attr(windows, windows_subsystem = "windows")]
fn main() {{
    let line = "river {version}";
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {{
        Some("--version") => println!("{{line}}"),
        Some("--sleep") => std::thread::sleep(std::time::Duration::from_secs(
            args[2].parse().unwrap(),
        )),
        Some("--mark") => std::fs::write(std::env::var("{MARK_ENV}").unwrap(), line).unwrap(),
        None => {{
            {on_start}
        }}
        _ => std::process::exit(2),
    }}
}}
"#
            ),
        )
        .unwrap();
        let out = dir.join(format!("river{}", std::env::consts::EXE_SUFFIX));
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let status = Command::new(rustc)
            .arg("-O")
            .arg("-o")
            .arg(&out)
            .arg(&src)
            .status()
            .expect("rustc runs");
        assert!(status.success(), "building the test program");
        built.1.insert(id, out.clone());
        out
    }
}
