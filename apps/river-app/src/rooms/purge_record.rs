//! This device's record of the direct-message deletions it signed ("Delete their messages"),
//! kept in a file of its own so a deletion that loses a race with another device is signed again
//! (freenet/river#945).
//!
//! The room keeps one purge envelope per recipient and the higher-ranked one wins (the higher
//! version, then the greater signature digest). Two devices signed in as the same identity can
//! each sign one at the same version; the loser's tokens stop being tombstoned and the deleted
//! messages come back. river-core's [`OwnPurges`] is the record the web UI and `riverctl` use to
//! notice that and sign the union again ([`OwnPurges::heal`], built with
//! `advance_recipient_purges` so every client signs byte-identical envelopes).
//!
//! **Where it lives.** One JSON file in the app's config directory, `own-purges.json`: a map
//! from the room owner's key (base58, as `riverctl` keys its rooms) to that room's [`OwnPurges`]
//! serialized as `riverctl` serializes it. It is per device. It is deliberately NOT written into
//! the chat delegate's room slot, where the web UI keeps `RoomData.own_purges`: that would be
//! this app's first field-preserving rewrite of a slot the web UI owns. The web UI and this app
//! need not share a record to converge. [`OwnPurges::heal`] takes the union with the winning
//! envelope, so a web UI tab and this app that sign as one identity each re-sign what the other's
//! envelope omits, and both end holding both sets.
//!
//! **When it is written.** [`PurgeRecord::record`] runs before a deletion is first sent, and
//! returns only after the new file is on disk (written to a temporary file, synced, renamed over
//! the old one, directory synced). A crash between the record and the send leaves a recorded
//! token the room does not hold, which the next heal signs. The record is bounded: a room's
//! entries go when its recipient is no longer a member ([`OwnPurges::prune`], as `riverctl`
//! does), and the whole entry goes when the user leaves the room ([`PurgeRecord::forget`]).
//! A deletion can only be queued while it fits the room's 1000-token cap, so what is recorded
//! for one recipient is bounded the same way.
//!
//! **`healed_at` is not advanced on disk.** The guard that signs at most once per winning
//! version is kept for the run only ([`super::session`]). Writing it when the re-signed envelope
//! is queued would lose the heal on a crash before it is sent; writing it when the envelope
//! lands would need the outbox to say so. Without it a restart can sign once more for a winning
//! version that still omits a recorded token, which converges (the new envelope lists it) and
//! is bounded by the number of restarts.
//!
//! Nothing here logs a token or a key, and the errors it returns name neither.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use ed25519_dalek::VerifyingKey;
use river_core::own_purges::OwnPurges;
use river_core::room_state::ChatRoomStateV1;
use river_core::room_state::direct_messages::PurgeToken;
use river_core::room_state::member::MemberId;
use serde::{Deserialize, Serialize};

use super::token::{create_private_dir, lock_file, private_file, sync_dir};

const FILE_NAME: &str = "own-purges.json";

/// What the file holds.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Book {
    /// By room owner key, base58.
    #[serde(default)]
    rooms: BTreeMap<String, OwnPurges>,
}

fn room_key(owner: &VerifyingKey) -> String {
    bs58::encode(owner.as_bytes()).into_string()
}

/// The record, read from the file at start and kept in memory; every change re-reads the file
/// under a lock, so a second River over the same directory (an update handover) loses nothing.
/// Without a directory it is kept in memory only (tests, and a shell with no config dir).
#[derive(Default)]
pub struct PurgeRecord {
    path: Option<PathBuf>,
    book: Book,
    /// Counts the changes this record has seen, so a caller can tell cheaply whether it moved.
    rev: u64,
}

impl std::fmt::Debug for PurgeRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PurgeRecord")
            .field("rooms", &self.book.rooms.len())
            .finish()
    }
}

impl PurgeRecord {
    /// The record kept in `dir`, the app's config directory. A file that cannot be read as a
    /// record is moved to `own-purges.json.bad-<time>-<pid>-<n>` (never overwritten unseen) and the record starts
    /// empty; a file that cannot be read at all leaves the record empty, and a change is
    /// refused until it can be read (a deletion is then refused rather than sent unrecorded).
    pub fn open(dir: &Path) -> Self {
        let path = dir.join(FILE_NAME);
        remove_stale_temporaries(dir);
        let book = match read_book(&path) {
            Ok((book, _)) => book,
            Err(e) => {
                crate::diag!(
                    "River: the record of your deletions was not read: {}",
                    crate::diag::scrub(&e)
                );
                Book::default()
            }
        };
        PurgeRecord {
            path: Some(path),
            book,
            rev: 0,
        }
    }

    /// A number that changes whenever the record does.
    pub fn rev(&self) -> u64 {
        self.rev
    }

    /// Whether this is the record kept in `dir`.
    pub fn is_in(&self, dir: &Path) -> bool {
        self.path.as_deref() == Some(dir.join(FILE_NAME).as_path())
    }

    /// The record of `owner`'s room, if there is one.
    pub fn for_room(&self, owner: &VerifyingKey) -> Option<&OwnPurges> {
        self.book.rooms.get(&room_key(owner))
    }

    /// Record that the user deleted `tokens`, signed as `me`, in `owner`'s room. Returns after
    /// the record is on disk; on an error nothing was recorded.
    pub fn record(
        &mut self,
        owner: &VerifyingKey,
        me: MemberId,
        tokens: &[PurgeToken],
    ) -> Result<(), String> {
        let key = room_key(owner);
        let tokens = tokens.to_vec();
        self.change(move |book| {
            book.rooms
                .entry(key)
                .or_default()
                .record(me, tokens.iter().copied());
            true
        })
    }

    /// Take back a record of `tokens` for a deletion the user gave up (it was refused, and they
    /// dismissed it).
    pub fn unrecord(
        &mut self,
        owner: &VerifyingKey,
        me: MemberId,
        tokens: &[PurgeToken],
    ) -> Result<(), String> {
        let key = room_key(owner);
        let tokens = tokens.to_vec();
        self.change(move |book| {
            let Some(own) = book.rooms.get_mut(&key) else {
                return false;
            };
            let before = own.purged.len();
            own.purged
                .retain(|(r, t)| !(*r == me && tokens.contains(t)));
            let changed = before != own.purged.len();
            if own.purged.is_empty() && own.healed_at.is_empty() {
                book.rooms.remove(&key);
            }
            changed
        })
    }

    /// Drop the entries of recipients that are no longer members of the room in `state`
    /// ([`OwnPurges::prune`]: a state whose configuration the owner did not sign prunes nothing).
    pub fn prune(&mut self, owner: &VerifyingKey, state: &ChatRoomStateV1) -> Result<(), String> {
        let key = room_key(owner);
        // Looked at first, so the common case writes nothing.
        let mut probe = match self.book.rooms.get(&key) {
            Some(own) => own.clone(),
            None => return Ok(()),
        };
        if !probe.prune(owner, state) {
            return Ok(());
        }
        let owner = *owner;
        let state = state.clone();
        self.change(move |book| {
            let Some(own) = book.rooms.get_mut(&key) else {
                return false;
            };
            let changed = own.prune(&owner, &state);
            if own.purged.is_empty() && own.healed_at.is_empty() {
                book.rooms.remove(&key);
            }
            changed
        })
    }

    /// Remove every room not in `keep` (rooms the user is no longer in, found at start and after
    /// a read of the stored rooms; an entry whose key does not read as a room goes too). One
    /// write, none when nothing goes.
    pub fn retain_rooms(
        &mut self,
        keep: &std::collections::HashSet<VerifyingKey>,
    ) -> Result<(), String> {
        let keep: std::collections::HashSet<String> = keep.iter().map(room_key).collect();
        if self.book.rooms.keys().all(|k| keep.contains(k)) {
            return Ok(());
        }
        self.change(move |book| {
            let before = book.rooms.len();
            book.rooms.retain(|k, _| keep.contains(k));
            before != book.rooms.len()
        })
    }

    /// Remove `owner`'s room from the record (the user left it).
    pub fn forget(&mut self, owner: &VerifyingKey) -> Result<(), String> {
        let key = room_key(owner);
        if !self.book.rooms.contains_key(&key) {
            return Ok(());
        }
        self.change(move |book| book.rooms.remove(&key).is_some())
    }

    /// Apply `edit` to the file's current content and keep the result, in memory and on disk.
    /// `edit` says whether it changed anything; nothing is written when it did not. When the
    /// write fails the memory is left as it was.
    fn change(&mut self, edit: impl FnOnce(&mut Book) -> bool) -> Result<(), String> {
        let Some(path) = self.path.clone() else {
            edit(&mut self.book);
            self.rev += 1;
            return Ok(());
        };
        let dir = path.parent().ok_or("bad record path")?.to_owned();
        create_private_dir(&dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
        // Held across the read and the write, so two Rivers cannot lose each other's change.
        let lock = lock_file(&dir.join(format!(".{FILE_NAME}.lock")))
            .map_err(|e| format!("couldn't lock the record of your deletions: {e}"))?;
        lock.lock()
            .map_err(|e| format!("couldn't lock the record of your deletions: {e}"))?;
        let (mut book, corrupt) = read_book(&path)?;
        if corrupt {
            // The file was not a record and was moved aside: what this run knows is kept and
            // written back, not replaced by an empty record.
            book = self.book.clone();
        }
        if !edit(&mut book) && !corrupt {
            // Nothing to write; what the file holds is what is known.
            if self.book != book {
                self.rev += 1;
            }
            self.book = book;
            return Ok(());
        }
        write_book(&path, &book)?;
        self.book = book;
        self.rev += 1;
        Ok(())
    }
}

/// The record in `path` and whether the file was not a record: empty when there is no file, and
/// empty after a file that is not a record is moved aside (to a name of its own, so an earlier
/// one is never overwritten). An error only for a file that cannot be read.
fn read_book(path: &Path) -> Result<(Book, bool), String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Book::default(), false)),
        Err(e) => return Err(format!("couldn't read {}: {e}", path.display())),
    };
    match serde_json::from_slice::<Book>(&bytes) {
        Ok(book) => Ok((book, false)),
        Err(_) => {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let bad =
                path.with_file_name(format!("{FILE_NAME}.bad-{at}-{}-{n}", std::process::id()));
            // Said without the content, which is tokens.
            crate::diag!("River: the record of your deletions was unreadable; moved aside");
            fs::rename(path, &bad)
                .map_err(|e| format!("couldn't move {} aside: {e}", path.display()))?;
            Ok((Book::default(), true))
        }
    }
}

/// Remove temporary files an earlier run left (it died between creating and renaming one). Only
/// those not touched for a minute, so another River's write in progress is left alone.
fn remove_stale_temporaries(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let prefix = format!(".{FILE_NAME}.");
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with(&prefix) && name.ends_with(".tmp")) {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age.as_secs() >= 60);
        if old {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Write `book` whole or not at all: to a temporary file of its own, synced, then renamed over
/// the record, then the directory synced.
fn write_book(path: &Path, book: &Book) -> Result<(), String> {
    let dir = path.parent().ok_or("bad record path")?;
    let err = |e: std::io::Error| format!("couldn't save the record of your deletions: {e}");
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{FILE_NAME}.{}.{n}.tmp", std::process::id()));
    let bytes = serde_json::to_vec(book).map_err(|e| format!("couldn't encode the record: {e}"))?;
    let mut file = private_file(&tmp).map_err(err)?;
    let written = file.write_all(&bytes).and_then(|()| file.sync_all());
    drop(file);
    if let Err(e) = written.and_then(|()| fs::rename(&tmp, path)) {
        let _ = fs::remove_file(&tmp);
        return Err(err(e));
    }
    sync_dir(dir);
    Ok(())
}

#[cfg(test)]
#[path = "purge_record_tests.rs"]
mod tests;
