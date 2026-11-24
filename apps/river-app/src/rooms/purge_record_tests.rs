//! The per-device record of deletions: what it keeps across a restart, how it is written, and
//! what bounds it. The race it exists for is tested in `session::own_purge_tests`.

use std::fs;
use std::path::PathBuf;

use ed25519_dalek::{Signer as _, SigningKey};
use river_core::own_purges::OwnPurges;
use river_core::room_state::direct_messages::PurgeToken;
use river_core::room_state::member::{AuthorizedMember, Member, MemberId};
use river_core::room_state::ChatRoomStateV1;

use super::*;
use crate::rooms::room_tests::Fixture;

fn dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("river-purge-record-{}-{name}", std::process::id()));
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
        let mode = fs::metadata(dir.join(FILE_NAME)).unwrap().permissions().mode();
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

#[test]
fn a_file_that_is_not_a_record_is_moved_aside_not_overwritten() {
    let dir = dir("corrupt");
    fs::write(dir.join(FILE_NAME), b"not json at all").unwrap();
    let mut record = PurgeRecord::open(&dir);
    assert!(record.for_room(&owner(1)).is_none());
    record.record(&owner(1), id(2), &[t(1)]).unwrap();
    assert_eq!(
        fs::read(dir.join(format!("{FILE_NAME}.bad"))).unwrap(),
        b"not json at all"
    );
    assert!(PurgeRecord::open(&dir).for_room(&owner(1)).is_some());
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
    assert!(!fs::read_to_string(dir.join(FILE_NAME))
        .unwrap()
        .contains(&bs58::encode(owner(1).as_bytes()).into_string()));
}

#[test]
fn a_dismissed_deletion_is_taken_back_and_an_empty_room_entry_goes() {
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
    assert_eq!(
        after.for_room(&owner_vk).unwrap().purged,
        {
            let mut expected = vec![(f.me_id(), t(1)), (f.owner_id(), t(2))];
            expected.sort();
            expected
        }
    );

    // A state whose configuration the owner did not sign prunes nothing.
    let mut forged = room_with(&f, &[]);
    forged.configuration.signature = SigningKey::from_bytes(&[8; 32]).sign(b"x");
    record.prune(&owner_vk, &forged).unwrap();
    assert_eq!(
        PurgeRecord::open(&dir).for_room(&owner_vk).unwrap().purged.len(),
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

#[cfg(unix)]
#[test]
fn a_record_that_cannot_be_written_is_an_error_and_changes_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let dir = dir("readonly");
    let mut record = PurgeRecord::open(&dir);
    record.record(&owner(1), id(2), &[t(1)]).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
    let writable = fs::File::create(dir.join("probe")).is_ok();
    let result = record.record(&owner(1), id(2), &[t(2)]);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    if writable {
        // Running as a user the permission does not bind (root): nothing to check.
        return;
    }
    assert!(result.is_err());
    assert_eq!(
        record.for_room(&owner(1)).unwrap().purged,
        vec![(id(2), t(1))],
        "memory is as the disk is"
    );
}
