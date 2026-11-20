//! What the updater remembers between runs, in `update-state.json` in River's data directory.
//!
//! - The highest manifest sequence and version it has accepted (so an older signed manifest
//!   cannot be replayed to it; see [`super::manifest`]).
//! - A downloaded, verified artifact waiting to be applied.
//! - A swap in probation: the new binary has been installed but has not yet said it started
//!   (see [`super::Updater::on_start`]).
//!
//! A missing or unreadable file is an empty state, never an error: the updater must not stop
//! working because its notes were lost. Writes go to a temporary file and are renamed into
//! place, so a crash leaves the old state or the new one.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub const STATE_FILE: &str = "update-state.json";

/// A downloaded artifact whose size and SHA-256 matched its signed manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Staged {
    pub version: String,
    /// The artifact file (an archive holding the new binary).
    pub artifact: PathBuf,
    /// The signed manifest and signature it came from, re-verified before applying.
    pub manifest: PathBuf,
    pub signature: PathBuf,
}

/// A swap that has not yet been confirmed by the new binary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Probation {
    pub from: String,
    pub to: String,
    pub install_path: PathBuf,
    /// A copy of the binary that was replaced.
    pub previous: PathBuf,
    pub previous_sha256: String,
    /// How many times the new binary has started without confirming. A binary that is started
    /// again without ever confirming is rolled back by itself.
    pub starts: u32,
}

/// The layout of the file this build writes. A file from a newer build (after a downgrade or a
/// rollback) is read for what this build understands and never written back, so it is not
/// damaged by being read and the newer build finds it as it left it.
pub const SCHEMA: u32 = 1;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub schema: u32,
    pub highest_sequence: u64,
    pub highest_version: Option<String>,
    pub staged: Option<Staged>,
    pub probation: Option<Probation>,
    /// Unix seconds of the last successful check.
    pub last_checked: Option<u64>,
    /// What an automatic rollback wants the user to be told, once.
    pub notice: Option<String>,
}

impl State {
    /// Written by a newer build: not saved over.
    pub fn is_newer(&self) -> bool {
        self.schema > SCHEMA
    }
}

pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join(STATE_FILE)
}

/// The state, or an empty one when the file is missing or unreadable. For paths where an
/// empty state is harmless (a start-up check, a notice); updating uses [`try_load`].
pub fn load(data_dir: &Path) -> State {
    try_load(data_dir).unwrap_or_default()
}

/// The state. A missing file is an empty state; a file that exists but cannot be read or
/// parsed is an error, and is kept as `update-state.json.bak`. An updater that treated it as
/// empty would forget the highest sequence and version it has accepted and take an older signed
/// manifest, so it fails closed instead: the person is told, and updating stays off until the
/// file is removed.
/// Run `op` again, a few times and briefly, while it fails with "permission denied". On Windows
/// that is what a file that is open elsewhere, or a lock file being deleted at that moment,
/// answers (an access violation or a sharing violation) where Unix would just succeed; it
/// passes in milliseconds. On Unix it is a real permission error and is returned after the
/// same short wait.
pub(super) fn retry_denied<T>(mut op: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let mut tries = 0;
    loop {
        match op() {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && tries < 100 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(5));
            }
            other => return other,
        }
    }
}

pub fn try_load(data_dir: &Path) -> Result<State, String> {
    let file = path(data_dir);
    let bytes = match retry_denied(|| fs::read(&file)) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(State::default()),
        Err(e) => return Err(format!("couldn't read {}: {e}", file.display())),
    };
    serde_json::from_slice(&bytes).map_err(|_| {
        let bak = data_dir.join(format!("{STATE_FILE}.bak"));
        let _ = fs::copy(&file, &bak);
        format!(
            "River's update notes ({}) are damaged, so River will not update itself until they \
             are removed. A copy was kept as {}.",
            file.display(),
            bak.display()
        )
    })
}

/// How long a caller waits behind one holder that makes no progress before it reports "busy".
const LOCK_PATIENCE: Duration = Duration::from_secs(5);
/// A bound on the whole wait, so a caller always ends even when other writers never stop.
const LOCK_LIMIT: Duration = Duration::from_secs(60);

/// Load, change and save the state while holding a lock file, so two Rivers (or two threads)
/// cannot overwrite each other's change. A damaged file is an error, as in [`try_load`].
pub fn modify<T>(data_dir: &Path, change: impl FnOnce(&mut State) -> T) -> Result<T, String> {
    modify_within(data_dir, LOCK_PATIENCE, change)
}

/// What a waiter can see of the lock without touching it: when the lock file and the state file
/// were last changed. It differs from one look to the next whenever another writer takes the
/// lock, saves, or lets go.
fn lock_progress(lock: &Path, state: &Path) -> [Option<std::time::SystemTime>; 2] {
    let modified = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).ok();
    [modified(lock), modified(state)]
}

/// [`modify`] with the patience given. The patience is how long the lock may stay with one
/// holder that does nothing; it starts again whenever the lock changes hands or the state is
/// saved, so waiting in a queue of working writers is not "busy" however long the queue is
/// (an fsync and a rename on a Windows disk with a scanner can take tens of milliseconds, and
/// the lock is not handed out in order). Only a holder that is stuck, or a lock left behind
/// by a crash, ends the wait.
fn modify_within<T>(
    data_dir: &Path,
    patience: Duration,
    change: impl FnOnce(&mut State) -> T,
) -> Result<T, String> {
    fs::create_dir_all(data_dir)
        .map_err(|e| format!("couldn't create {}: {e}", data_dir.display()))?;
    let lock = data_dir.join(format!("{STATE_FILE}.lock"));
    let state_file = path(data_dir);
    let started = std::time::Instant::now();
    let mut deadline = started + patience;
    let mut seen = lock_progress(&lock, &state_file);
    let mut waits = 0u32;
    let _guard = loop {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
        {
            Ok(_) => break LockGuard(lock.clone()),
            // Windows answers "permission denied" for a lock file that another thread has just
            // deleted and that is not gone yet, so that is "busy" too.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                let stale = fs::metadata(&lock)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > Duration::from_secs(30));
                let now = std::time::Instant::now();
                let progress = lock_progress(&lock, &state_file);
                if progress != seen {
                    seen = progress;
                    deadline = now + patience;
                }
                if stale {
                    let _ = fs::remove_file(&lock);
                } else if now >= deadline || now >= started + LOCK_LIMIT {
                    return Err("the update notes are busy".into());
                } else {
                    // A little jitter, so waiters do not look at the lock in step.
                    waits = waits.wrapping_add(1);
                    let jitter =
                        u64::from(now.elapsed().subsec_nanos() ^ waits.wrapping_mul(7919)) % 8;
                    std::thread::sleep(Duration::from_millis(6 + jitter));
                }
            }
            Err(e) => return Err(format!("couldn't lock the update notes: {e}")),
        }
    };
    let mut state = try_load(data_dir)?;
    let out = change(&mut state);
    if !state.is_newer() {
        state.schema = SCHEMA;
        save(data_dir, &state)?;
    }
    Ok(out)
}

struct LockGuard(PathBuf);

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub fn save(data_dir: &Path, state: &State) -> Result<(), String> {
    fs::create_dir_all(data_dir)
        .map_err(|e| format!("couldn't create {}: {e}", data_dir.display()))?;
    let bytes = serde_json::to_vec_pretty(state).map_err(|e| e.to_string())?;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let tmp = data_dir.join(format!(
        "{STATE_FILE}.{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let write = || -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        // Replacing a file that another handle has open can be refused on Windows; it is
        // retried briefly. Nothing here holds a handle on `tmp` or the target across it.
        drop(f);
        retry_denied(|| fs::rename(&tmp, path(data_dir)))
    };
    write().map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("couldn't save the update state: {e}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_and_garbage_files_are_empty_state() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()), State::default());
        fs::write(path(dir.path()), b"not json").unwrap();
        assert_eq!(load(dir.path()), State::default());
    }

    #[test]
    fn a_damaged_file_fails_closed_and_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(path(dir.path()), b"not json").unwrap();
        let err = try_load(dir.path()).unwrap_err();
        assert!(err.contains("damaged"), "{err}");
        assert_eq!(
            fs::read(dir.path().join("update-state.json.bak")).unwrap(),
            b"not json"
        );
        assert!(
            modify(dir.path(), |_| ()).is_err(),
            "no change is saved over it"
        );
        assert_eq!(fs::read(path(dir.path())).unwrap(), b"not json");
    }

    #[test]
    fn denied_operations_are_retried_briefly_and_then_reported() {
        let mut calls = 0;
        let r = retry_denied(|| {
            calls += 1;
            if calls < 4 {
                Err(std::io::ErrorKind::PermissionDenied.into())
            } else {
                Ok(calls)
            }
        });
        assert_eq!(r.unwrap(), 4);
        // Other errors are not retried.
        let mut calls = 0;
        let r: std::io::Result<()> = retry_denied(|| {
            calls += 1;
            Err(std::io::ErrorKind::NotFound.into())
        });
        assert!(r.is_err());
        assert_eq!(calls, 1);
    }

    #[test]
    fn concurrent_changes_are_not_lost() {
        let dir = tempfile::tempdir().unwrap();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let d = dir.path().to_path_buf();
                std::thread::spawn(move || {
                    for _ in 0..10 {
                        modify(&d, |s| s.highest_sequence += 1).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(load(dir.path()).highest_sequence, 80);
    }

    #[test]
    fn a_long_queue_of_working_writers_is_not_busy() {
        // Each change holds the lock for 60 ms and the patience is 150 ms, so a writer that
        // is last of six waits about 300 ms. Waiting behind writers that keep finishing is not
        // "busy"; before the patience restarted on progress, the last ones gave up.
        let dir = tempfile::tempdir().unwrap();
        let threads: Vec<_> = (0..6)
            .map(|_| {
                let d = dir.path().to_path_buf();
                std::thread::spawn(move || {
                    for _ in 0..3 {
                        modify_within(&d, Duration::from_millis(150), |s| {
                            std::thread::sleep(Duration::from_millis(60));
                            s.highest_sequence += 1;
                        })
                        .unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(load(dir.path()).highest_sequence, 18);
    }

    #[test]
    fn a_holder_that_does_nothing_is_reported_busy() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(format!("{STATE_FILE}.lock")), b"").unwrap();
        let began = std::time::Instant::now();
        let r = modify_within(dir.path(), Duration::from_millis(200), |_| ());
        assert_eq!(r.unwrap_err(), "the update notes are busy");
        assert!(began.elapsed() < Duration::from_secs(5));
        assert!(!path(dir.path()).exists(), "nothing was written");
    }

    #[test]
    fn state_round_trips_and_tolerates_missing_fields() {
        let dir = tempfile::tempdir().unwrap();
        let s = State {
            highest_sequence: 7,
            highest_version: Some("0.2.0".into()),
            probation: Some(Probation {
                from: "0.1.0".into(),
                to: "0.2.0".into(),
                install_path: "/x/river".into(),
                previous: "/y/river".into(),
                previous_sha256: "ab".into(),
                starts: 1,
            }),
            ..Default::default()
        };
        save(dir.path(), &s).unwrap();
        assert_eq!(load(dir.path()), s);
        fs::write(path(dir.path()), br#"{"highest_sequence":3}"#).unwrap();
        assert_eq!(load(dir.path()).highest_sequence, 3);
    }
}
