//! River's self-update: find a newer signed build, download it, verify it, swap it in, start
//! it, and put the old one back if the new one does not come up.
//!
//! # Trust
//!
//! The only thing trusted is [`pubkey`], River's update-signing public key, compiled in. The
//! server (any host, any redirect, a hijacked DNS name) can only offer bytes that either
//! verify against that key or are refused. In order, before the installed binary is touched:
//!
//! 0. The key must not be the placeholder: every entry point refuses it ([`manifest::verify_and_parse`]
//!    and [`Updater::check`]), so an unconfigured build cannot update whatever the server says.
//! 1. The manifest's ed25519 signature ([`manifest::verify_and_parse`]), then its rules: channel,
//!    expiry, sequence replay, version rollback ([`manifest::evaluate`]).
//! 2. The artifact's exact size and SHA-256, both from the signed manifest, while it downloads
//!    into River's own data directory ([`Updater::download`]).
//! 3. At apply time, the stored manifest's signature and rules again, and the stored artifact's
//!    hash again ([`Updater::apply`]): a file left on disk by an earlier run is not trusted
//!    for having been checked then.
//! 4. The unpacked binary must run and report the manifest's version ([`platform::Installer::prepare`]).
//!
//! The private key never touches the app or this repository. `examples/update_tool.rs`
//! generates a pair and signs manifests; the key is Ian's.
//!
//! # Swap and rollback
//!
//! [`Updater::apply`] keeps a copy of the running binary in `<data>/update/previous/`, then renames
//! the new binary over it. The old process keeps running from the old inode. Then
//! [`Updater::launch`] starts the new binary with a handshake file path in its environment and waits
//! for it to appear. The new binary writes it from [`Updater::confirm_started`], once its
//! window's event loop is running. If the new binary exits, or says nothing within
//! [`HANDSHAKE_TIMEOUT`], the old process (still running) restores the previous binary and
//! reports the failure; if it does confirm, the old process exits. The new binary confirms
//! only after its first successful node connection or [`CONFIRM_AFTER`] of running, whichever
//! comes first (see `main.rs`), so a binary that crashes once it is drawing or connecting is
//! not kept. The timeout is long (two minutes) because a slow machine can take a while to open
//! a window, and killing a good binary is worse than waiting.
//!
//! The apply lock is held from the swap until the launch has finished, and the swap refuses to
//! run if the installed file is no longer the one this River started as (another River or a
//! package manager changed it), so a kept "previous" copy is always what it says.
//!
//! The swap is recorded as "probation" in the state file until the new binary confirms. A
//! new binary that is started by hand (no launcher) and fails to confirm
//! [`MAX_UNCONFIRMED_STARTS`] times in a row rolls itself back on its next start; a start is
//! counted only after the command line has been accepted, and a clean exit resets the count, so
//! a good binary that is run with a bad option, or closed at once, is never rolled back. If the
//! rollback itself fails, nothing is started in its place: the failure is printed and left as a
//! notice for the next start, and the probation record is kept. What this
//! cannot rescue: a new binary that cannot even load (a missing library) and is started by hand;
//! the preflight `--version` run before the swap exists to catch that case first, and the kept
//! copy of the old binary is in `previous/` for a manual restore.

pub mod host;
#[cfg(target_os = "linux")]
mod linux;
pub mod manifest;
mod net;
mod platform;
mod state;
#[cfg(all(test, target_os = "linux"))]
mod testserver;

use std::ffi::OsString;
use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::release::{Version, parse_version};
use manifest::{Offer, Refusal, Trust, Verdict};
pub use manifest::{PLACEHOLDER_PUBKEY, is_placeholder};
pub use platform::{Installer, Support};
use state::{Probation, Staged};

/// Where the manifest lives (the signature is at the same address plus `.sig`). The freenet/web
/// repository serves `hugo-site/static/river/update/stable/` there through GitHub Pages: HTTPS,
/// no redirect, cached for about ten minutes, and a deploy publishes the manifest and its
/// signature together. Overridden by `RIVER_UPDATE_MANIFEST_URL` in debug and `test-hooks`
/// builds only.
pub const MANIFEST_URL: &str = "https://freenet.org/river/update/stable/manifest.json";
pub const CHANNEL: &str = "stable";

/// The key this build trusts: `RIVER_UPDATE_PUBKEY_HEX` at build time (64 hex digits, checked
/// by `build.rs`, which fails the build if it is set and malformed), else the placeholder,
/// which nothing verifies against.
pub fn pubkey() -> [u8; 32] {
    option_env!("RIVER_UPDATE_PUBKEY_HEX")
        .and_then(|hex| <[u8; 32]>::try_from(::hex::decode(hex).ok()?).ok())
        .unwrap_or(PLACEHOLDER_PUBKEY)
}

/// The floors this build starts with: the oldest version and sequence it will accept from a
/// manifest, from `RIVER_UPDATE_MIN_VERSION` and `RIVER_UPDATE_MIN_SEQUENCE` at build time
/// (checked by `build.rs`; both default to nothing).
pub fn build_floor() -> Trust {
    Trust {
        highest_sequence: option_env!("RIVER_UPDATE_MIN_SEQUENCE")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        highest_version: option_env!("RIVER_UPDATE_MIN_VERSION")
            .and_then(|v| parse_version(v).ok()),
    }
}

/// How long the new binary has to confirm it started. Long on purpose: see the module comment.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(120);
/// The new binary confirms after this long running if it has not connected to the node first.
pub const CONFIRM_AFTER: Duration = Duration::from_secs(20);
/// A run at least this long that ends normally is not a failed start.
pub const CLEAN_EXIT_AFTER: Duration = Duration::from_secs(5);
/// A new binary started by hand that did not confirm this many times is rolled back.
pub const MAX_UNCONFIRMED_STARTS: u32 = 2;
/// The environment variable that carries the handshake file path to the new binary.
pub const HANDSHAKE_ENV: &str = "RIVER_UPDATE_HANDSHAKE";
/// How long an apply lock is honoured before it is taken to be left by a crash.
const LOCK_STALE: Duration = Duration::from_secs(10 * 60);
/// Leftovers of an interrupted update older than this are deleted.
const LEFTOVER_AGE: Duration = Duration::from_secs(60 * 60);

pub struct Config {
    pub manifest_url: String,
    pub pubkey: [u8; 32],
    pub data_dir: PathBuf,
    pub running: Version,
    pub channel: String,
    /// Allow `http://127.0.0.1` URLs (tests and `test-hooks` builds only).
    pub allow_loopback_http: bool,
    pub clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    pub handshake_timeout: Duration,
    /// The lowest sequence and version a manifest may carry, before anything is remembered.
    pub floor: Trust,
    /// How long to wait before fetching the manifest again when its signature does not verify
    /// (a CDN can serve a new manifest beside an old signature for a moment).
    pub retry_delay: Duration,
}

impl Config {
    // Unix seconds for update checks; nothing is signed.
    #[allow(clippy::disallowed_methods)]
    pub fn system_clock() -> Arc<dyn Fn() -> u64 + Send + Sync> {
        Arc::new(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckResult {
    UpToDate,
    Available(Offer),
}

/// A manifest that verified, kept for the download that follows the check.
struct Checked {
    manifest: Vec<u8>,
    signature: Vec<u8>,
    sequence: u64,
    offer: Offer,
}

/// A swap that has been made and not yet confirmed. It holds the apply lock until it is
/// dropped, so no second River swaps while this one is launching.
#[derive(Debug)]
pub struct Applied {
    pub version: Version,
    pub install_path: PathBuf,
    /// What to put back if the new version does not start.
    probation: Probation,
    _lock: Lock,
}

pub struct Updater {
    cfg: Config,
    net: net::Net,
    installer: Arc<dyn Installer>,
    checked: Mutex<Option<Checked>>,
    /// The SHA-256 of the installed binary as this River first saw it.
    install_sha: Arc<Mutex<Option<String>>>,
}

fn remember(slot: &Mutex<Option<String>>, installer: &dyn Installer) {
    let mut sha = slot.lock().unwrap();
    if sha.is_none()
        && let Support::Replaceable { install_path } = installer.support()
        && let Ok(file) = fs::File::open(install_path)
        && let Ok((sum, _)) = hash_file(file)
    {
        *sha = Some(hex::encode(sum));
    }
}

fn io(what: &str, e: std::io::Error) -> String {
    format!("{what}: {e}")
}

impl Updater {
    pub fn new(cfg: Config, installer: Arc<dyn Installer>) -> Updater {
        let net = net::Net::new(cfg.allow_loopback_http);
        Updater {
            cfg,
            net,
            installer,
            checked: Mutex::new(None),
            install_sha: Arc::new(Mutex::new(None)),
        }
    }

    pub fn running(&self) -> Version {
        self.cfg.running
    }

    pub fn support(&self) -> Support {
        self.installer.support()
    }

    fn dir(&self) -> PathBuf {
        self.cfg.data_dir.join("update")
    }

    fn now(&self) -> u64 {
        (self.cfg.clock)()
    }

    /// The two floors: what this install has accepted, or what the build requires, whichever
    /// is higher.
    fn trust(&self, st: &state::State) -> Trust {
        let seen = Trust {
            highest_sequence: st.highest_sequence,
            highest_version: st
                .highest_version
                .as_deref()
                .and_then(|v| parse_version(v).ok()),
        };
        Trust {
            highest_sequence: seen.highest_sequence.max(self.cfg.floor.highest_sequence),
            highest_version: seen.highest_version.max(self.cfg.floor.highest_version),
        }
    }

    fn platform(&self) -> Result<&'static str, String> {
        self.installer
            .platform_key()
            .ok_or_else(|| "River has no update for this platform".to_string())
    }

    fn require_key(&self) -> Result<(), String> {
        if is_placeholder(&self.cfg.pubkey) {
            return Err("this build of River has no update key, so it cannot update itself".into());
        }
        Ok(())
    }

    /// Remember what the installed binary is, the first time. It is compared at swap time, and
    /// a swap is refused while it is unknown.
    pub fn remember_install(&self) {
        remember(&self.install_sha, self.installer.as_ref());
    }

    /// Record the installed binary's hash on a thread, at start-up, so the swap guard has
    /// something to compare with however early an update is applied.
    pub fn remember_install_in_background(&self) {
        let (sha, installer) = (self.install_sha.clone(), self.installer.clone());
        std::thread::spawn(move || remember(&sha, installer.as_ref()));
    }

    /// Delete what an interrupted update left behind, once it is old enough to be nobody's.
    pub fn sweep(&self) {
        let old = |p: &Path| {
            fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > LEFTOVER_AGE)
        };
        let clean = |dir: &Path, prefixes: &[&str]| {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if prefixes.iter().any(|p| name.starts_with(p)) && old(&entry.path()) {
                    let _ = fs::remove_file(entry.path());
                }
            }
        };
        clean(&self.dir(), &["handshake-"]);
        // Only the temporary files a save leaves (`update-state.json.<pid>.<n>.tmp`), never the
        // `.bak` kept for a damaged file or a live lock.
        if let Ok(entries) = fs::read_dir(&self.cfg.data_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with("update-state.json.")
                    && name.ends_with(".tmp")
                    && old(&entry.path())
                {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
        if let Support::Replaceable { install_path } = self.installer.support()
            && let Some(dir) = install_path.parent()
        {
            clean(
                dir,
                &[
                    ".river-update-",
                    ".river-restore-",
                    ".river-copy-",
                    ".river-write-test-",
                ],
            );
        }
    }

    fn fetch_verified(&self) -> Result<(Vec<u8>, Vec<u8>, manifest::Manifest), String> {
        let url = &self.cfg.manifest_url;
        let fetch = || -> Result<(Vec<u8>, Vec<u8>), String> {
            let manifest = self.net.get_small(url, manifest::MAX_MANIFEST_BYTES)?;
            let signature = self
                .net
                .get_small(&format!("{url}.sig"), manifest::SIGNATURE_BYTES as u64)?;
            Ok((manifest, signature))
        };
        let (mut manifest, mut signature) = fetch()?;
        let mut verified = manifest::verify_and_parse(&manifest, &signature, &self.cfg.pubkey);
        if verified == Err(Refusal::BadSignature) {
            // A CDN can serve the new manifest next to the old signature for a short while:
            // fetch both once more before calling it bad.
            std::thread::sleep(self.cfg.retry_delay);
            (manifest, signature) = fetch()?;
            verified = manifest::verify_and_parse(&manifest, &signature, &self.cfg.pubkey);
        }
        let parsed = verified.map_err(|r| r.to_string())?;
        Ok((manifest, signature, parsed))
    }

    /// Fetch and verify the manifest and say whether it offers something newer.
    pub fn check(&self) -> Result<CheckResult, String> {
        self.require_key()?;
        state::try_load(&self.cfg.data_dir)?;
        self.sweep();
        self.remember_install();
        let (manifest, signature, parsed) = self.fetch_verified()?;
        let platform = self.platform()?;
        let now = self.now();
        let verdict = state::modify(&self.cfg.data_dir, |st| {
            let trust = self.trust(st);
            let verdict = manifest::evaluate(
                &parsed,
                self.cfg.running,
                &self.cfg.channel,
                platform,
                now,
                trust,
            )
            .map_err(|r| r.to_string())?;
            // Only a manifest that passed everything moves the floor.
            let trust = trust.accepting(&parsed);
            st.highest_sequence = trust.highest_sequence;
            st.highest_version = trust.highest_version.map(|v| v.to_string());
            st.last_checked = Some(now);
            Ok::<_, String>(verdict)
        })??;
        match verdict {
            Verdict::UpToDate => {
                *self.checked.lock().unwrap() = None;
                Ok(CheckResult::UpToDate)
            }
            Verdict::Available(offer) => {
                *self.checked.lock().unwrap() = Some(Checked {
                    manifest,
                    signature,
                    sequence: parsed.sequence,
                    offer: offer.clone(),
                });
                Ok(CheckResult::Available(offer))
            }
        }
    }

    fn require_replaceable(&self) -> Result<PathBuf, String> {
        match self.installer.support() {
            Support::Replaceable { install_path } => Ok(install_path),
            Support::External(why) | Support::Unsupported(why) => Err(why),
        }
    }

    /// Delete the staged update and forget it.
    fn discard_staged(&self) {
        let _ = fs::remove_dir_all(self.dir().join("staged"));
        let _ = state::modify(&self.cfg.data_dir, |st| st.staged = None);
    }

    /// Whether the staged update's verified manifest is still the one on offer: the same
    /// sequence and artifact hash, and it would still pass every rule today. Otherwise it is
    /// stale (re-signed, expired, superseded) and must not be reused.
    fn staged_is_current(&self, st: &state::State, offer: &Offer, sequence: u64) -> bool {
        let Some(staged) = &st.staged else {
            return false;
        };
        let (Ok(manifest), Ok(signature)) =
            (fs::read(&staged.manifest), fs::read(&staged.signature))
        else {
            return false;
        };
        let Ok(parsed) = manifest::verify_and_parse(&manifest, &signature, &self.cfg.pubkey) else {
            return false;
        };
        let Ok(platform) = self.platform() else {
            return false;
        };
        parsed.sequence == sequence
            && staged.artifact.exists()
            && matches!(
                manifest::evaluate(&parsed, self.cfg.running, &self.cfg.channel, platform, self.now(), self.trust(st)),
                Ok(Verdict::Available(o)) if o.artifact.sha256 == offer.artifact.sha256 && o.version == offer.version
            )
    }

    /// The version already downloaded and verified by an earlier run, if it is still usable.
    pub fn staged_version(&self) -> Option<Version> {
        let st = state::load(&self.cfg.data_dir);
        let staged = st.staged.as_ref()?;
        let v = parse_version(&staged.version).ok()?;
        let parsed = {
            let manifest = fs::read(&staged.manifest).ok()?;
            let signature = fs::read(&staged.signature).ok()?;
            manifest::verify_and_parse(&manifest, &signature, &self.cfg.pubkey).ok()?
        };
        let verdict = manifest::evaluate(
            &parsed,
            self.cfg.running,
            &self.cfg.channel,
            self.platform().ok()?,
            self.now(),
            self.trust(&st),
        );
        (staged.artifact.exists()
            && v > self.cfg.running
            && matches!(verdict, Ok(Verdict::Available(_))))
        .then_some(v)
    }

    /// Download the artifact the last [`check`](Updater::check) offered. On success it is on
    /// disk, its size and SHA-256 matching the signed manifest.
    pub fn download(&self, progress: &mut dyn FnMut(u64, u64)) -> Result<Version, String> {
        self.require_key()?;
        self.require_replaceable()?;
        // Not while another River is applying what is staged: discarding it would pull the
        // files out from under that swap.
        let _lock = Lock::take(&self.dir().join("apply.lock"))?;
        let st = state::try_load(&self.cfg.data_dir)?;
        self.sweep();
        let (offer, manifest_bytes, signature, sequence) = {
            let checked = self.checked.lock().unwrap();
            let c = checked.as_ref().ok_or("check for updates first")?;
            (
                c.offer.clone(),
                c.manifest.clone(),
                c.signature.clone(),
                c.sequence,
            )
        };
        if self.staged_is_current(&st, &offer, sequence) {
            return Ok(offer.version);
        }
        let dir = self.dir();
        let staged_dir = dir.join("staged");
        // Only ever one staged update; a stale one is replaced.
        self.discard_staged();
        fs::create_dir_all(&staged_dir).map_err(|e| io("couldn't create the update folder", e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
        }
        let artifact_path = staged_dir.join(format!("river-{}.artifact", offer.version));
        let part = staged_dir.join("download.part");
        let result = (|| -> Result<(), String> {
            let mut file =
                fs::File::create(&part).map_err(|e| io("couldn't save the update", e))?;
            let sum = self.net.download(
                &offer.artifact.url,
                offer.artifact.size,
                net::ARTIFACT_DEADLINE,
                &mut file,
                &mut |got, total| progress(got, total.unwrap_or(offer.artifact.size)),
            )?;
            file.sync_all()
                .map_err(|e| io("couldn't save the update", e))?;
            if hex::encode(sum) != offer.artifact.sha256 {
                return Err(
                    "the download does not match its signed checksum, so it was discarded".into(),
                );
            }
            // Close it before renaming: a file with an open handle cannot be renamed on Windows.
            drop(file);
            state::retry_denied(|| fs::rename(&part, &artifact_path))
                .map_err(|e| io("couldn't save the update", e))
        })();
        if let Err(e) = result {
            let _ = fs::remove_dir_all(&staged_dir);
            return Err(e);
        }
        let manifest_path = staged_dir.join("manifest.json");
        let signature_path = staged_dir.join("manifest.json.sig");
        fs::write(&manifest_path, &manifest_bytes)
            .map_err(|e| io("couldn't save the update", e))?;
        fs::write(&signature_path, &signature).map_err(|e| io("couldn't save the update", e))?;
        state::modify(&self.cfg.data_dir, |st| {
            st.staged = Some(Staged {
                version: offer.version.to_string(),
                artifact: artifact_path,
                manifest: manifest_path,
                signature: signature_path,
            })
        })?;
        Ok(offer.version)
    }

    /// Re-verify the staged update from disk, unpack it, and swap it in. After this the new
    /// binary is installed and the old one keeps running until [`launch`](Updater::launch).
    /// A staged update that fails any check is discarded, so the next check starts clean.
    pub fn apply(&self) -> Result<Applied, String> {
        self.require_key()?;
        let install_path = self.require_replaceable()?;
        let lock = Lock::take(&self.dir().join("apply.lock"))?;
        let mut st = state::try_load(&self.cfg.data_dir)?;
        if let Some(p) = st.probation.clone() {
            // A swap that was never confirmed. If this River is the one that was replaced (the
            // launch failed and so did the rollback), try the rollback again now; otherwise
            // the swapped-in binary is still on probation and nothing more is applied.
            if p.from == self.cfg.running.to_string() {
                let (message, restored) =
                    self.restore_probation(&p, "the earlier restore did not finish");
                if !restored {
                    return Err(message);
                }
                st = state::try_load(&self.cfg.data_dir)?;
            } else {
                return Err("the last update has not been confirmed yet".into());
            }
        }
        match self.apply_locked(&st, &install_path) {
            Ok((version, probation)) => Ok(Applied {
                version,
                install_path,
                probation,
                _lock: lock,
            }),
            Err(e) => {
                self.discard_staged();
                Err(e)
            }
        }
    }

    fn apply_locked(
        &self,
        st: &state::State,
        install_path: &Path,
    ) -> Result<(Version, Probation), String> {
        let staged = st.staged.clone().ok_or("there is no downloaded update")?;
        let read = |p: &Path| fs::read(p).map_err(|e| io("couldn't read the downloaded update", e));
        let manifest_bytes = read(&staged.manifest)?;
        let signature = read(&staged.signature)?;
        let parsed = manifest::verify_and_parse(&manifest_bytes, &signature, &self.cfg.pubkey)
            .map_err(|r| r.to_string())?;
        let verdict = manifest::evaluate(
            &parsed,
            self.cfg.running,
            &self.cfg.channel,
            self.platform()?,
            self.now(),
            self.trust(st),
        )
        .map_err(|r| r.to_string())?;
        let Verdict::Available(offer) = verdict else {
            return Err("the downloaded update is no longer newer than this River".into());
        };
        if offer.version.to_string() != staged.version {
            return Err("the downloaded update is not the one its manifest names".into());
        }
        // Hash and unpack from the one open file, so nothing can swap it between the two.
        let mut file = fs::File::open(&staged.artifact)
            .map_err(|e| io("couldn't read the downloaded update", e))?;
        let (sum, len) = hash_file(
            file.try_clone()
                .map_err(|e| io("couldn't read the downloaded update", e))?,
        )
        .map_err(|e| io("couldn't read the downloaded update", e))?;
        if len != offer.artifact.size || hex::encode(sum) != offer.artifact.sha256 {
            return Err("the downloaded update does not match its signed checksum".into());
        }
        file.seek(SeekFrom::Start(0))
            .map_err(|e| io("couldn't read the downloaded update", e))?;
        let prepared = self.installer.prepare(file, offer.version, install_path)?;
        let previous_dir = self.dir().join("previous");
        let previous = previous_dir.join(format!("river-{}", self.cfg.running));
        let _ = fs::remove_dir_all(&previous_dir);
        let Some(expected) = self.install_sha.lock().unwrap().clone() else {
            let _ = fs::remove_file(&prepared);
            return Err(
                "River has not finished noting what is installed; try again in a moment".into(),
            );
        };
        let previous_sha256 =
            match self
                .installer
                .swap(&prepared, install_path, &previous, Some(&expected))
            {
                Ok(sha) => sha,
                Err(e) => {
                    let _ = fs::remove_file(&prepared);
                    let _ = fs::remove_dir_all(&previous_dir);
                    return Err(e);
                }
            };
        let probation = Probation {
            from: self.cfg.running.to_string(),
            to: offer.version.to_string(),
            install_path: install_path.to_path_buf(),
            previous,
            previous_sha256,
            starts: 0,
        };
        let recorded = state::modify(&self.cfg.data_dir, |s| {
            s.staged = None;
            s.probation = Some(probation.clone());
        });
        if let Err(e) = recorded {
            // Without the record there is no rollback. Put the old binary back now.
            let _ = self.installer.restore(
                &probation.previous,
                &probation.previous_sha256,
                &probation.install_path,
            );
            return Err(e);
        }
        let _ = fs::remove_dir_all(self.dir().join("staged"));
        Ok((offer.version, probation))
    }

    /// Start the swapped-in binary and wait for it to confirm. If it does not, restore the
    /// previous binary and say so. On `Ok` the caller should exit: the new binary is running.
    // A unique handshake file name; nothing is signed.
    #[allow(clippy::disallowed_methods)]
    pub fn launch(&self, applied: &Applied, args: &[OsString]) -> Result<(), String> {
        let marker = self.dir().join(format!(
            "handshake-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let _ = fs::remove_file(&marker);
        let mut child = match self.installer.spawn(
            &applied.install_path,
            args,
            &[(HANDSHAKE_ENV, marker.to_string_lossy().into_owned())],
        ) {
            Ok(c) => c,
            Err(e) => {
                return Err(self
                    .restore_probation(
                        &applied.probation,
                        &format!("the new River would not start: {e}"),
                    )
                    .0);
            }
        };
        let deadline = std::time::Instant::now() + self.cfg.handshake_timeout;
        let failure = loop {
            if fs::read_to_string(&marker).is_ok_and(|s| s.trim() == applied.version.to_string()) {
                let _ = fs::remove_file(&marker);
                return Ok(());
            }
            match child.try_wait() {
                Ok(Some(status)) => break format!("the new River exited at once ({status})"),
                Ok(None) => {}
                Err(e) => break format!("couldn't watch the new River: {e}"),
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                break "the new River did not start in time".to_string();
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let _ = fs::remove_file(&marker);
        Err(self.restore_probation(&applied.probation, &failure).0)
    }

    /// Put the previous binary back, described by `p`. Returns what to tell the user and
    /// whether it worked. The restore is done first and outside the state lock, so a damaged
    /// state file or a lock left by a crashed River cannot stop it; the record is updated
    /// afterwards. When the restore failed the probation record stays, so it is not forgotten.
    fn restore_probation(&self, p: &Probation, why: &str) -> (String, bool) {
        let outcome = self
            .installer
            .restore(&p.previous, &p.previous_sha256, &p.install_path);
        let (message, restored) = match &outcome {
            Ok(()) => (
                format!(
                    "River {} did not start ({why}), so River went back to {}.",
                    p.to, p.from
                ),
                true,
            ),
            Err(e) => (
                format!(
                    "River {} did not start ({why}), and the previous version could not be \
                     restored: {e}. A copy is at {}.",
                    p.to,
                    p.previous.display()
                ),
                false,
            ),
        };
        // Best effort: the binary is already back (or not), whatever the notes say.
        report_unrecorded(
            "the rollback",
            state::modify(&self.cfg.data_dir, |st| {
                if restored && st.probation.as_ref().is_some_and(|q| q.to == p.to) {
                    st.probation = None;
                }
                st.notice = Some(message.clone());
            }),
        );
        (message, restored)
    }

    /// Call as early as possible once the command line has been accepted, with whether a
    /// launcher is waiting for this process (the handshake variable is set). A binary that was
    /// swapped in and has not yet confirmed counts the start, and after
    /// [`MAX_UNCONFIRMED_STARTS`] it rolls itself back (returns the previous binary's path for
    /// the caller to start in its place). If the rollback fails it starts nothing else: the
    /// failure is printed and kept as a notice.
    pub fn on_start(&self, supervised: bool) -> Option<PathBuf> {
        let st = state::load(&self.cfg.data_dir);
        let p = st.probation.clone()?;
        if p.to != self.cfg.running.to_string() {
            // Not the swapped-in version. Drop the record only if this IS the installed
            // binary (the install was changed since: a manual restore, a later version). A
            // different copy started by hand, from `previous/` say, says nothing about the
            // swap that is on probation.
            if matches!(
                self.installer.support(),
                Support::Replaceable { install_path } if install_path == p.install_path
            ) {
                let _ = state::modify(&self.cfg.data_dir, |s| s.probation = None);
            }
            return None;
        }
        if supervised {
            return None;
        }
        let starts = p.starts + 1;
        if starts > MAX_UNCONFIRMED_STARTS {
            let (message, restored) = self.restore_probation(&p, "it kept failing to start");
            if restored {
                return Some(p.install_path);
            }
            river_app::diag!("River: {message}");
            return None;
        }
        report_unrecorded(
            "this start",
            state::modify(&self.cfg.data_dir, |s| {
                if let Some(p) = s.probation.as_mut() {
                    p.starts = starts;
                }
            }),
        );
        None
    }

    /// The window closed normally after `ran`. A start that lasted at least
    /// [`CLEAN_EXIT_AFTER`] was not a failure, so the count is reset; a run that ends sooner
    /// (a binary whose window opens and returns at once) stays counted.
    pub fn clean_exit(&self, ran: Duration) {
        if ran < CLEAN_EXIT_AFTER {
            return;
        }
        let _ = state::modify(&self.cfg.data_dir, |s| {
            if let Some(p) = s.probation.as_mut()
                && p.to == self.cfg.running.to_string()
            {
                p.starts = 0;
            }
        });
    }

    /// Write the marker a waiting launcher looks for, and nothing else: probation is left to a
    /// start that really ran (see [`confirm_started`](Self::confirm_started)).
    pub fn signal_started(&self, handshake: Option<&Path>) {
        if let Some(path) = handshake
            && path.parent() == Some(self.dir().as_path())
        {
            let _ = fs::File::create(path)
                .and_then(|mut f| f.write_all(self.cfg.running.to_string().as_bytes()));
        }
    }

    /// The new binary is up: tell the launcher, and end probation.
    pub fn confirm_started(&self, handshake: Option<&Path>) {
        self.signal_started(handshake);
        report_unrecorded(
            "the confirmed start",
            state::modify(&self.cfg.data_dir, |st| {
                if st
                    .probation
                    .as_ref()
                    .is_some_and(|p| p.to == self.cfg.running.to_string())
                {
                    st.probation = None;
                }
            }),
        );
    }

    /// A message left by an automatic rollback, once.
    pub fn take_notice(&self) -> Option<String> {
        state::modify(&self.cfg.data_dir, |st| st.notice.take())
            .ok()
            .flatten()
    }
}

/// Say, without any of the notes' content, that a change to the update notes was not saved
/// (they were busy or unreadable). The caller carries on, as before; this only makes a start
/// that was not counted, or a swap that was not confirmed, visible.
fn report_unrecorded<T>(what: &str, result: Result<T, String>) {
    if let Err(e) = result {
        river_app::diag!("River: the update notes did not record {what}: {e}");
    }
}

fn hash_file(mut file: fs::File) -> std::io::Result<([u8; 32], u64)> {
    use std::io::Read;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut len = 0;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok((hasher.finalize().into(), len));
        }
        len += n as u64;
        hasher.update(&buf[..n]);
    }
}

/// A lock file that excludes a second River doing the same swap.
#[derive(Debug)]
struct Lock(PathBuf);

impl Lock {
    fn take(path: &Path) -> Result<Lock, String> {
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        for _ in 0..2 {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(_) => return Ok(Lock(path.to_path_buf())),
                // "Permission denied" is what Windows says for a lock file that was deleted a
                // moment ago and has not gone yet: the same as being held.
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
                    ) =>
                {
                    let stale = fs::metadata(path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age > LOCK_STALE);
                    if !stale {
                        return Err("another River is updating right now".into());
                    }
                    let _ = fs::remove_file(path);
                }
                Err(e) => return Err(io("couldn't lock the update", e)),
            }
        }
        Err("another River is updating right now".into())
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

// The end-to-end tests run real shell scripts through the Linux installer.
#[cfg(all(test, target_os = "linux"))]
mod tests;
