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
use std::time::{Duration, Instant};

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

/// How long a caller waits behind a holder that makes no progress before it reports "busy".
const LOCK_PATIENCE: Duration = Duration::from_secs(5);
/// A bound on the whole wait, so a caller always ends even when other writers never stop.
const LOCK_LIMIT: Duration = Duration::from_secs(60);
/// How often a waiter looks at the lock again (plus up to the same again, at random).
const LOCK_POLL: Duration = Duration::from_millis(6);

/// How long [`modify`] waits for the lock.
#[derive(Clone, Copy)]
struct Wait {
    patience: Duration,
    poll: Duration,
}

/// Load, change and save the state while holding the lock, so two Rivers (or two threads)
/// cannot overwrite each other's change. A damaged file is an error, as in [`try_load`].
///
/// The lock is an operating-system advisory lock on `update-state.json.lock`, a file that is
/// created once and never deleted. The system drops the lock when its holder exits or crashes,
/// so there is no stale lock to detect and remove, and nothing a guard can delete that belongs
/// to the next holder. (An earlier scheme created and deleted the lock file; a waiter that took
/// a lock for stale could delete a new holder's lock, and two writers then overlapped and one
/// change was lost, which for the highest accepted sequence lowers the anti-rollback floor.)
/// Locks conflict between two handles in one process as well as between processes, on Unix
/// (`flock`) and on Windows (`LockFileEx`).
pub fn modify<T>(data_dir: &Path, change: impl FnOnce(&mut State) -> T) -> Result<T, String> {
    modify_waiting(
        data_dir,
        Wait {
            patience: LOCK_PATIENCE,
            poll: LOCK_POLL,
        },
        change,
    )
}

fn lock_path(data_dir: &Path) -> PathBuf {
    data_dir.join(format!("{STATE_FILE}.lock"))
}

fn busy(data_dir: &Path) -> String {
    format!(
        "the update notes are busy: another River, or an update, may be using them. Wait a \
         moment and try again; if no River is running, delete {}",
        lock_path(data_dir).display()
    )
}

/// A number that differs between calls and processes, for spreading out waiters.
fn jitter_nanos() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}

/// [`modify`] with the wait given. The patience is how long the lock may stay with a holder
/// that does nothing; it starts again whenever the state file's modification time changes
/// (another writer saved), so waiting behind a queue of working writers is not "busy" however
/// long the queue is (an fsync and a rename on a Windows disk with a scanner can take tens of
/// milliseconds, and the lock is not handed out in order). A holder that is stuck ends the wait
/// as "busy", and nothing is changed.
fn modify_waiting<T>(
    data_dir: &Path,
    wait: Wait,
    change: impl FnOnce(&mut State) -> T,
) -> Result<T, String> {
    fs::create_dir_all(data_dir)
        .map_err(|e| format!("couldn't create {}: {e}", data_dir.display()))?;
    let lock_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path(data_dir))
        .map_err(|e| format!("couldn't open the update notes lock: {e}"))?;
    let state_file = path(data_dir);
    let modified = || fs::metadata(&state_file).and_then(|m| m.modified()).ok();
    let started = Instant::now();
    let mut deadline = started + wait.patience;
    let mut seen = modified();
    let _guard = loop {
        match lock_file.try_lock() {
            Ok(()) => break LockGuard(&lock_file),
            Err(fs::TryLockError::WouldBlock) => {
                let now = Instant::now();
                let latest = modified();
                if latest != seen {
                    seen = latest;
                    deadline = now + wait.patience;
                }
                if now >= deadline || now >= started + LOCK_LIMIT {
                    return Err(busy(data_dir));
                }
                let poll = wait.poll.as_nanos() as u64;
                std::thread::sleep(Duration::from_nanos(poll + jitter_nanos() % poll.max(1)));
            }
            Err(fs::TryLockError::Error(e)) => {
                return Err(format!("couldn't lock the update notes: {e}"));
            }
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

/// Holds the lock until dropped. It lets go of the lock and leaves the file where it is.
struct LockGuard<'a>(&'a fs::File);

impl Drop for LockGuard<'_> {
    fn drop(&mut self) {
        let _ = self.0.unlock();
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

    const FAST: Wait = Wait {
        patience: Duration::from_secs(30),
        poll: Duration::from_millis(2),
    };

    /// Handles to the lock file in one process conflict, which is what makes the thread
    /// tests above contend as processes do.
    #[test]
    fn two_handles_in_one_process_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let open = || fs::File::create(lock_path(dir.path())).unwrap();
        let (a, b) = (open(), open());
        a.try_lock().unwrap();
        assert!(matches!(b.try_lock(), Err(fs::TryLockError::WouldBlock)));
        a.unlock().unwrap();
        b.try_lock().unwrap();
    }

    #[test]
    fn a_holder_that_does_nothing_is_reported_busy() {
        let dir = tempfile::tempdir().unwrap();
        let holder = fs::File::create(lock_path(dir.path())).unwrap();
        holder.try_lock().unwrap();
        let wait = Wait {
            patience: Duration::from_millis(100),
            poll: Duration::from_millis(2),
        };
        let r = modify_waiting(dir.path(), wait, |s| s.highest_sequence = 9);
        let e = r.unwrap_err();
        assert!(e.starts_with("the update notes are busy"), "{e}");
        assert!(e.contains(STATE_FILE), "it says where the lock is: {e}");
        assert!(!path(dir.path()).exists(), "nothing was written");
    }

    /// A holder that keeps saving is making progress, so the waiter's patience restarts. The
    /// holder here outlasts the patience several times over, and sets the state file's time
    /// itself so the test does not depend on the disk's clock granularity.
    #[test]
    fn a_holder_that_keeps_saving_is_waited_for() {
        let dir = tempfile::tempdir().unwrap();
        let holder = fs::File::create(lock_path(dir.path())).unwrap();
        holder.try_lock().unwrap();
        let state = path(dir.path());
        fs::write(&state, b"{}").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let h = {
            let state = state.clone();
            std::thread::spawn(move || {
                let f = fs::OpenOptions::new().write(true).open(&state).unwrap();
                let base = std::time::SystemTime::now();
                for i in 1..=12u64 {
                    tx.send(()).unwrap();
                    f.set_modified(base + Duration::from_secs(i)).unwrap();
                    std::thread::sleep(Duration::from_millis(100));
                }
                holder.unlock().unwrap();
            })
        };
        rx.recv().unwrap();
        let wait = Wait {
            patience: Duration::from_millis(1000),
            poll: Duration::from_millis(5),
        };
        // About 1.2 s of holding against 1 s of patience: it passes only because it restarts.
        modify_waiting(dir.path(), wait, |s| s.highest_sequence = 5).unwrap();
        h.join().unwrap();
        assert_eq!(load(dir.path()).highest_sequence, 5);
    }

    /// The test binary run again as a child process, for the tests below. It does nothing
    /// unless the parent set the environment.
    #[test]
    fn child_process() {
        let (Ok(dir), Ok(mode)) = (
            std::env::var("RIVER_STATE_TEST_DIR"),
            std::env::var("RIVER_STATE_TEST_MODE"),
        ) else {
            return;
        };
        let dir = PathBuf::from(dir);
        match mode.as_str() {
            "count" => {
                while !dir.join("go").exists() {
                    std::thread::sleep(Duration::from_millis(2));
                }
                for _ in 0..25 {
                    modify_waiting(&dir, FAST, |s| s.highest_sequence += 1).unwrap();
                }
            }
            "hold" => {
                let f = fs::File::create(lock_path(&dir)).unwrap();
                f.try_lock().unwrap();
                fs::write(dir.join("held"), b"").unwrap();
                std::thread::sleep(Duration::from_secs(600));
            }
            other => panic!("unknown mode {other}"),
        }
    }

    fn child(dir: &Path, mode: &str) -> std::process::Child {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "update::state::tests::child_process",
                "--nocapture",
            ])
            .env("RIVER_STATE_TEST_DIR", dir)
            .env("RIVER_STATE_TEST_MODE", mode)
            .spawn()
            .unwrap()
    }

    /// Four processes and four threads, released together, each add 25: none is lost.
    #[test]
    fn processes_and_threads_together_lose_no_change() {
        let dir = tempfile::tempdir().unwrap();
        let mut children: Vec<_> = (0..4).map(|_| child(dir.path(), "count")).collect();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(5));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let (d, b) = (dir.path().to_path_buf(), barrier.clone());
                std::thread::spawn(move || {
                    b.wait();
                    for _ in 0..25 {
                        modify_waiting(&d, FAST, |s| s.highest_sequence += 1).unwrap();
                    }
                })
            })
            .collect();
        barrier.wait();
        fs::write(dir.path().join("go"), b"").unwrap();
        for t in threads {
            t.join().unwrap();
        }
        for c in &mut children {
            assert!(c.wait().unwrap().success());
        }
        assert_eq!(load(dir.path()).highest_sequence, 200);
    }

    /// A holder that dies without letting go does not block anyone: no 30 s stale rule, no
    /// manual clean-up. While it is alive, another process is refused.
    #[test]
    fn a_killed_holder_does_not_block_the_next_writer() {
        let dir = tempfile::tempdir().unwrap();
        let mut holder = child(dir.path(), "hold");
        while !dir.path().join("held").exists() {
            std::thread::sleep(Duration::from_millis(5));
        }
        let wait = Wait {
            patience: Duration::from_millis(100),
            poll: Duration::from_millis(2),
        };
        assert!(modify_waiting(dir.path(), wait, |_| ()).is_err());
        holder.kill().unwrap();
        holder.wait().unwrap();
        let began = Instant::now();
        modify_waiting(dir.path(), FAST, |s| s.highest_sequence = 3).unwrap();
        assert!(began.elapsed() < Duration::from_secs(5));
        assert_eq!(load(dir.path()).highest_sequence, 3);
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
