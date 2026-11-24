//! The per-device record of deletions: what it keeps across a restart, how it is written, and
//! what bounds it. The race it exists for is tested in `session::own_purge_tests`.

use std::fs;
use std::path::PathBuf;

use ed25519_dalek::{Signer as _, SigningKey};
use river_core::own_purges::OwnPurges;
use river_core::room_state::ChatRoomStateV1;
use river_core::room_state::direct_messages::PurgeToken;
use river_core::room_state::member::{AuthorizedMember, Member, MemberId};

use super::*;
use crate::rooms::room_tests::Fixture;

fn dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("river-purge-record-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn owner(seed: u8) -> VerifyingKey {
    SigningKey::from_bytes(&[seed; 32]).verifying_key()
}

fn id(seed: u8) -> MemberId {
    MemberId::from(&owner(seed))
}

fn t(n: u8) -> PurgeToken {
    PurgeToken([n; 16])
}

#[test]
fn a_recorded_deletion_survives_a_restart() {
    let dir = dir("restart");
    let mut record = PurgeRecord::open(&dir);
    record.record(&owner(1), id(2), &[t(1), t(2)]).unwrap();
    record.record(&owner(1), id(2), &[t(2), t(3)]).unwrap();
    record.record(&owner(5), id(2), &[t(9)]).unwrap();
    drop(record);

    let reloaded = PurgeRecord::open(&dir);
    let own = reloaded.for_room(&owner(1)).expect("the room's record");
    assert_eq!(
        own.purged,
        vec![(id(2), t(1)), (id(2), t(2)), (id(2), t(3))],
        "each token once, sorted"
    );
    assert!(own.healed_at.is_empty());
    assert_eq!(
        reloaded.for_room(&owner(5)).unwrap().purged,
        vec![(id(2), t(9))]
    );
    assert!(reloaded.for_room(&owner(6)).is_none());
}

#[test]
fn the_file_is_riverctls_record_serialized_per_room_and_owner_only() {
    let dir = dir("shape");
    let mut record = PurgeRecord::open(&dir);
    record.record(&owner(1), id(2), &[t(1)]).unwrap();
    let text = fs::read_to_string(dir.join(FILE_NAME)).unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    let key = bs58::encode(owner(1).as_bytes()).into_string();
    // The per-room value is exactly river-core's `OwnPurges`, as riverctl stores it.
    let own: OwnPurges = serde_json::from_value(json["rooms"][&key].clone()).unwrap();
    assert_eq!(own.purged, vec![(id(2), t(1))]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(dir.join(FILE_NAME))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    // No temporary file is left behind.
    let leftovers: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn a_second_river_over_the_same_directory_loses_nothing() {
    let dir = dir("two-processes");
    let mut first = PurgeRecord::open(&dir);
    let mut second = PurgeRecord::open(&dir);
    first.record(&owner(1), id(2), &[t(1)]).unwrap();
    // `second` read the file before `first` wrote: its change is made on what the file holds.
    second.record(&owner(1), id(2), &[t(2)]).unwrap();
    let both = PurgeRecord::open(&dir);
    assert_eq!(
        both.for_room(&owner(1)).unwrap().purged,
        vec![(id(2), t(1)), (id(2), t(2))]
    );
}

fn bad_files(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(&format!("{FILE_NAME}.bad"))
        })
        .collect();
    v.sort();
    v
}

#[test]
fn a_file_that_is_not_a_record_is_moved_aside_and_a_second_one_does_not_replace_the_first() {
    let dir = dir("corrupt");
    fs::write(dir.join(FILE_NAME), b"not json at all").unwrap();
    let mut record = PurgeRecord::open(&dir);
    assert!(record.for_room(&owner(1)).is_none());
    record.record(&owner(1), id(2), &[t(1)]).unwrap();
    let first = bad_files(&dir);
    assert_eq!(first.len(), 1);
    assert_eq!(fs::read(&first[0]).unwrap(), b"not json at all");
    assert!(PurgeRecord::open(&dir).for_room(&owner(1)).is_some());

    // Corrupted again while running: what this run knows is written back, not an empty record,
    // and the second bad file has a name of its own.
    fs::write(dir.join(FILE_NAME), b"{{ second").unwrap();
    record.record(&owner(1), id(2), &[t(2)]).unwrap();
    let both = bad_files(&dir);
    assert_eq!(both.len(), 2);
    assert!(
        both.iter()
            .any(|p| fs::read(p).unwrap() == b"not json at all")
    );
    assert!(both.iter().any(|p| fs::read(p).unwrap() == b"{{ second"));
    assert_eq!(
        PurgeRecord::open(&dir).for_room(&owner(1)).unwrap().purged,
        vec![(id(2), t(1)), (id(2), t(2))]
    );
}

#[test]
fn a_file_that_cannot_be_read_at_all_refuses_changes_and_is_not_replaced() {
    let dir = dir("unreadable");
    // A directory where the file should be: reading it fails whoever runs this.
    fs::create_dir_all(dir.join(FILE_NAME)).unwrap();
    let mut record = PurgeRecord::open(&dir);
    assert!(record.for_room(&owner(1)).is_none());
    assert!(record.record(&owner(1), id(2), &[t(1)]).is_err());
    assert!(record.for_room(&owner(1)).is_none(), "nothing was recorded");
    assert!(dir.join(FILE_NAME).is_dir());
}

#[test]
fn a_record_that_cannot_be_locked_or_written_is_an_error_and_changes_nothing() {
    let dir = dir("unwritable");
    let mut record = PurgeRecord::open(&dir);
    record.record(&owner(1), id(2), &[t(1)]).unwrap();
    // The directory is gone and a file stands in its place: no lock, no write, for anyone.
    fs::remove_dir_all(&dir).unwrap();
    fs::write(&dir, b"x").unwrap();
    assert!(record.record(&owner(1), id(2), &[t(2)]).is_err());
    assert_eq!(
        record.for_room(&owner(1)).unwrap().purged,
        vec![(id(2), t(1))],
        "memory is as the disk was"
    );
}

#[test]
fn old_temporary_files_are_removed_at_start_and_recent_ones_left() {
    let dir = dir("tmp");
    let old = dir.join(format!(".{FILE_NAME}.1.0.tmp"));
    let recent = dir.join(format!(".{FILE_NAME}.2.0.tmp"));
    let other = dir.join("notes.tmp");
    for p in [&old, &recent, &other] {
        fs::write(p, b"x").unwrap();
    }
    let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(long_ago)
        .unwrap();
    let _ = PurgeRecord::open(&dir);
    assert!(!old.exists());
    assert!(recent.exists(), "a write in progress is left alone");
    assert!(other.exists());
}

#[test]
fn leaving_a_room_removes_its_entry_and_only_its_entry() {
    let dir = dir("forget");
    let mut record = PurgeRecord::open(&dir);
    record.record(&owner(1), id(2), &[t(1)]).unwrap();
    record.record(&owner(5), id(2), &[t(2)]).unwrap();
    record.forget(&owner(1)).unwrap();
    // Forgetting a room that has no entry is not an error and writes nothing.
    record.forget(&owner(1)).unwrap();
    let reloaded = PurgeRecord::open(&dir);
    assert!(reloaded.for_room(&owner(1)).is_none());
    assert!(reloaded.for_room(&owner(5)).is_some());
    assert!(
        !fs::read_to_string(dir.join(FILE_NAME))
            .unwrap()
            .contains(&bs58::encode(owner(1).as_bytes()).into_string())
    );
}

#[test]
fn the_tokens_of_a_refused_deletion_are_taken_back_and_an_empty_room_entry_goes() {
    let dir = dir("unrecord");
    let mut record = PurgeRecord::open(&dir);
    record.record(&owner(1), id(2), &[t(1), t(2)]).unwrap();
    record.unrecord(&owner(1), id(2), &[t(1)]).unwrap();
    assert_eq!(
        PurgeRecord::open(&dir).for_room(&owner(1)).unwrap().purged,
        vec![(id(2), t(2))]
    );
    // Another identity's tokens are not touched.
    record.unrecord(&owner(1), id(3), &[t(2)]).unwrap();
    assert!(record.for_room(&owner(1)).is_some());
    record.unrecord(&owner(1), id(2), &[t(2)]).unwrap();
    assert!(PurgeRecord::open(&dir).for_room(&owner(1)).is_none());
}

/// A room whose members are the owner and `members`, with its configuration signed.
fn room_with(f: &Fixture, members: &[&SigningKey]) -> ChatRoomStateV1 {
    let mut state = f.private_state();
    state.members.members.clear();
    for who in members {
        state.members.members.push(AuthorizedMember::new(
            Member {
                owner_member_id: f.owner_id(),
                invited_by: f.owner_id(),
                member_vk: who.verifying_key(),
            },
            &f.owner,
        ));
    }
    state
}

#[test]
fn the_record_is_bounded_by_the_rooms_members() {
    let dir = dir("prune");
    let f = Fixture::new();
    let owner_vk = f.owner.verifying_key();
    let gone = SigningKey::from_bytes(&[77; 32]);
    let gone_id = MemberId::from(&gone.verifying_key());
    let mut record = PurgeRecord::open(&dir);
    record.record(&owner_vk, f.me_id(), &[t(1)]).unwrap();
    record.record(&owner_vk, f.owner_id(), &[t(2)]).unwrap();
    record.record(&owner_vk, gone_id, &[t(3), t(4)]).unwrap();

    // The room as it is with the user, and not `gone`, a member: `gone`'s entries go, the
    // user's and the owner's stay.
    let state = room_with(&f, &[&f.me]);
    record.prune(&owner_vk, &state).unwrap();
    let after = PurgeRecord::open(&dir);
    assert_eq!(after.for_room(&owner_vk).unwrap().purged, {
        let mut expected = vec![(f.me_id(), t(1)), (f.owner_id(), t(2))];
        expected.sort();
        expected
    });

    // A state whose configuration the owner did not sign prunes nothing.
    let mut forged = room_with(&f, &[]);
    forged.configuration.signature = SigningKey::from_bytes(&[8; 32]).sign(b"x");
    record.prune(&owner_vk, &forged).unwrap();
    assert_eq!(
        PurgeRecord::open(&dir)
            .for_room(&owner_vk)
            .unwrap()
            .purged
            .len(),
        2
    );
}

#[test]
fn without_a_directory_the_record_is_kept_for_the_run_only() {
    let mut record = PurgeRecord::default();
    record.record(&owner(1), id(2), &[t(1)]).unwrap();
    assert_eq!(record.for_room(&owner(1)).unwrap().purged.len(), 1);
    record.forget(&owner(1)).unwrap();
    assert!(record.for_room(&owner(1)).is_none());
}

#[test]
fn recording_says_which_tokens_it_newly_added() {
    let dir = dir("added");
    let mut record = PurgeRecord::open(&dir);
    assert_eq!(
        record
            .record(&owner(1), id(2), &[t(2), t(1), t(1)])
            .unwrap(),
        vec![t(1), t(2)]
    );
    assert_eq!(
        record.record(&owner(1), id(2), &[t(2), t(3)]).unwrap(),
        vec![t(3)]
    );
    assert!(record.record(&owner(1), id(2), &[t(3)]).unwrap().is_empty());
    // Another recipient's token is its own.
    assert_eq!(
        record.record(&owner(1), id(7), &[t(3)]).unwrap(),
        vec![t(3)]
    );
}

/// The lock cannot be taken (a directory stands where its file should be) while the record
/// itself reads and writes: the change goes ahead unlocked, as the grant token's store does.
#[test]
fn a_lock_that_cannot_be_taken_does_not_stop_the_record() {
    let dir = dir("lock-fails");
    fs::create_dir_all(dir.join(format!(".{FILE_NAME}.lock"))).unwrap();
    let mut record = PurgeRecord::open(&dir);
    record.record(&owner(1), id(2), &[t(1)]).unwrap();
    assert_eq!(
        PurgeRecord::open(&dir).for_room(&owner(1)).unwrap().purged,
        vec![(id(2), t(1))]
    );
}

/// A lock another River holds is waited for a short while, then the change goes ahead.
#[test]
fn a_lock_that_stays_held_is_waited_for_only_a_short_while() {
    let dir = dir("lock-held");
    let held = lock_file(&dir.join(format!(".{FILE_NAME}.lock"))).unwrap();
    held.lock().unwrap();
    let mut record = PurgeRecord::open(&dir);
    let start = std::time::Instant::now();
    record.record(&owner(1), id(2), &[t(1)]).unwrap();
    assert!(start.elapsed() < std::time::Duration::from_secs(10));
    assert!(PurgeRecord::open(&dir).for_room(&owner(1)).is_some());
    drop(held);
}

/// Without the lock, a change another River made between this one's read and its write is kept:
/// the file is read again before the write and the change made again on what is there.
#[test]
fn without_the_lock_a_change_made_in_between_is_kept() {
    let dir = dir("unlocked-race");
    fs::create_dir_all(dir.join(format!(".{FILE_NAME}.lock"))).unwrap();
    let mut record = PurgeRecord::open(&dir);
    let mut first = true;
    let (mine, key) = (id(2), bs58::encode(owner(1).as_bytes()).into_string());
    record
        .change(|book| {
            if std::mem::take(&mut first) {
                // Another River writes while this one is between its read and its write.
                PurgeRecord::open(&dir)
                    .record(&owner(5), id(2), &[t(9)])
                    .unwrap();
            }
            book.rooms
                .entry(key.clone())
                .or_default()
                .record(mine, [t(1)]);
            true
        })
        .unwrap();
    let after = PurgeRecord::open(&dir);
    assert!(after.for_room(&owner(1)).is_some());
    assert!(
        after.for_room(&owner(5)).is_some(),
        "the other River's change survived"
    );
}
