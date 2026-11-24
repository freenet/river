//! A deletion lost to a same-version race on another device is signed again as the union, once
//! per winning version (freenet/river#945). Devices are real `Rooms` of one identity over one
//! room, each with its own config directory (its own record file); the node is the room
//! contract applying what the devices send, and tells each device the state it holds.

use std::path::{Path, PathBuf};
use std::time::Instant;

use freenet_scaffold::ComposableState;
use freenet_stdlib::prelude::State;
use river_core::room_state::direct_messages::{
    AuthorizedDirectMessage, DirectMessagesDelta, PurgeToken, compose_direct_message,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};

use super::*;
use crate::rooms::outbox::Action;
use crate::rooms::purge_record::PurgeRecord;
use crate::rooms::room_tests::Fixture;
use ed25519_dalek::{Signer as _, SigningKey};

fn dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("river-own-purges-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `n` messages from the room owner to the user, composed once so every device holds the same
/// bytes (a message is sealed with fresh randomness each time it is composed).
fn messages(f: &Fixture, n: u64) -> Vec<AuthorizedDirectMessage> {
    (0..n)
        .map(|i| {
            let ts = 1_700_000_000 + i;
            compose_direct_message(
                &f.owner,
                &f.me.verifying_key(),
                &f.owner.verifying_key(),
                ts,
                ts,
                format!("owner says {i}").as_bytes(),
            )
            .unwrap()
        })
        .collect()
}

/// The room contract: the state it holds and how many updates it took.
struct Node {
    state: ChatRoomStateV1,
    params: ChatRoomParametersV1,
    took: usize,
    versions: Vec<u64>,
}

impl Node {
    fn of(device: &Device) -> Node {
        let state = device.rooms.list[0].stored.room_state.clone();
        let params = ChatRoomParametersV1 {
            owner: device.rooms.list[0].stored.owner_vk,
        };
        Node {
            state,
            params,
            took: 0,
            versions: Vec::new(),
        }
    }

    /// The contract applies an update: a higher-ranked envelope replaces the held one, a lower
    /// one is dropped without an error.
    fn take(&mut self, delta: &ChatRoomStateV1Delta) {
        for env in &delta.direct_messages.as_ref().unwrap().advanced_purges {
            self.versions.push(env.state.version);
        }
        let before = self.state.clone();
        self.state
            .apply_delta(&before, &self.params, &Some(delta.clone()))
            .expect("the contract takes it");
        self.took += 1;
    }

    fn purged(&self, who: MemberId) -> Vec<PurgeToken> {
        let mut t = self
            .state
            .direct_messages
            .purges
            .iter()
            .find(|p| p.recipient_id == who)
            .map(|p| p.state.purged.clone())
            .unwrap_or_default();
        t.sort();
        t
    }

    fn version(&self, who: MemberId) -> u64 {
        self.state
            .direct_messages
            .purges
            .iter()
            .find(|p| p.recipient_id == who)
            .map_or(0, |p| p.state.version)
    }
}

/// One device of the user: the room as `Rooms` holds it, and a record in its own directory.
struct Device {
    rooms: Rooms,
    key: String,
    peer: String,
}

fn device(dir: &Path, msgs: &[AuthorizedDirectMessage]) -> Device {
    let f = Fixture::new();
    let mut rooms = Rooms::default();
    let mut room = LiveRoom::new(f.stored(f.private_state()));
    for m in msgs {
        room.apply_delta(ChatRoomStateV1Delta {
            direct_messages: Some(DirectMessagesDelta {
                new_messages: vec![m.clone()],
                advanced_purges: Vec::new(),
            }),
            ..Default::default()
        })
        .unwrap();
    }
    let key = room.key();
    rooms.network_seen.insert(room.stored.owner_vk);
    rooms.list.push(room);
    rooms.loaded = true;
    rooms.complete = true;
    // As `run` does at the start of a session.
    rooms.outbox.keep_purge_record_in(Some(dir));
    Device {
        rooms,
        key,
        peer: crate::rooms::dm::peer_key(f.owner_id()),
    }
}

impl Device {
    fn me(&self) -> MemberId {
        Fixture::new().me_id()
    }

    fn delete(&mut self, tokens: &[PurgeToken]) {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Command {
            room: self.key.clone(),
            action: Action::PurgeDm {
                peer: crate::rooms::dm::peer_from_key(&self.peer).unwrap(),
                tokens: tokens.to_vec(),
                ticket: 1,
            },
        })
        .unwrap();
        self.rooms.take_commands(&rx);
        let answers: Vec<_> = std::mem::take(&mut self.rooms.dm_purge_results)
            .into_iter()
            .map(|r| r.result)
            .collect();
        assert_eq!(answers, vec![Ok(())]);
    }

    /// Send what the outbox has, if anything: the node takes it and answers Ok. True when
    /// something was sent.
    fn send(&mut self, node: &mut Node) -> bool {
        let mut changed = false;
        let Some(delta) =
            self.rooms
                .outbox
                .next_update(&self.rooms.list[0], Instant::now(), &mut changed)
        else {
            return false;
        };
        node.take(&delta);
        let id = self.rooms.list[0].contract_id();
        self.rooms.update_answered(&id, Ok(()));
        true
    }

    /// The node tells the device the state it holds, and the session's tick runs.
    fn hear(&mut self, node: &Node) {
        let id = self.rooms.list[0].contract_id();
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&node.state, &mut bytes).unwrap();
        self.rooms
            .apply_update(&id, &UpdateData::State(State::from(bytes)));
        self.rooms.own_purges_tick();
    }

    fn holds(&self) -> Vec<PurgeToken> {
        let mut t = self.rooms.list[0]
            .stored
            .room_state
            .direct_messages
            .purges
            .iter()
            .find(|p| p.recipient_id == self.me())
            .map(|p| p.state.purged.clone())
            .unwrap_or_default();
        t.sort();
        t
    }
}

/// Rounds of every device sending what it has and then hearing the node, until a round sends
/// nothing. How many updates the node took. Fails if it does not go quiet.
fn settle(devices: &mut [Device], node: &mut Node) -> usize {
    let before = node.took;
    // Each device first hears the node's state (the update that follows its own send).
    for d in devices.iter_mut() {
        d.hear(node);
    }
    for _ in 0..12 {
        let mut sent = false;
        for d in devices.iter_mut() {
            sent |= d.send(node);
        }
        for d in devices.iter_mut() {
            d.hear(node);
        }
        if !sent {
            return node.took - before;
        }
    }
    panic!(
        "the devices kept signing envelopes: {} taken",
        node.took - before
    );
}

fn sorted(mut t: Vec<PurgeToken>) -> Vec<PurgeToken> {
    t.sort();
    t
}

/// Both devices delete a different message at version 1. The digest decides which envelope
/// the room keeps; the device that lost signs the union, once, and both end with both deleted.
/// Returns which device lost (0 or 1).
fn the_race(msgs: &[AuthorizedDirectMessage], first_deletes: usize, name: &str) -> usize {
    let tokens: Vec<PurgeToken> = msgs.iter().map(|m| m.purge_token()).collect();
    let mut devices = [
        device(&dir(&format!("{name}-a")), msgs),
        device(&dir(&format!("{name}-b")), msgs),
    ];
    let mut node = Node::of(&devices[0]);
    let me = devices[0].me();

    devices[0].delete(&[tokens[first_deletes]]);
    devices[1].delete(&[tokens[1 - first_deletes]]);
    // Both go out before either hears the other: two envelopes at version 1.
    assert!(devices[0].send(&mut node));
    assert!(devices[1].send(&mut node));
    assert_eq!(node.versions, vec![1, 1]);
    let kept = node.purged(me);
    assert_eq!(
        kept.len(),
        1,
        "the room kept one of the two, not their union"
    );
    assert_eq!(node.version(me), 1);

    for d in &mut devices {
        d.hear(&node);
    }
    // The device whose token the winner omits.
    let lost: Vec<usize> = (0..2)
        .filter(|&i| {
            let mine = tokens[if i == 0 {
                first_deletes
            } else {
                1 - first_deletes
            }];
            !kept.contains(&mine)
        })
        .collect();
    assert_eq!(lost.len(), 1);

    let signed = settle(&mut devices, &mut node);
    assert_eq!(
        signed, 1,
        "the loser signs the union once: {:?}",
        node.versions
    );
    assert_eq!(node.versions, vec![1, 1, 2]);
    let both = sorted(tokens.clone());
    assert_eq!(node.purged(me), both);
    assert_eq!(node.version(me), 2);
    for d in &devices {
        assert_eq!(d.holds(), both, "every device holds both tombstones");
        assert!(
            d.rooms.list[0]
                .stored
                .room_state
                .direct_messages
                .messages
                .is_empty(),
            "both messages are gone"
        );
        assert!(d.rooms.outbox.is_empty());
    }
    // And it stays quiet.
    assert_eq!(settle(&mut devices, &mut node), 0);
    lost[0]
}

#[test]
fn the_losing_device_signs_the_union_once_in_both_orders() {
    // One set of messages (a message is sealed with fresh randomness each time it is composed,
    // so its token, and which envelope has the greater digest, differs between runs).
    let msgs = messages(&Fixture::new(), 2);
    let a = the_race(&msgs, 0, "race-0");
    let b = the_race(&msgs, 1, "race-1");
    // The same two envelopes with the devices swapped: the winner is decided by the signature
    // digest of the envelope, so the other device loses. Both orders were exercised.
    assert_ne!(
        a, b,
        "the device that loses changes with which one deleted what"
    );
}

/// Three devices each delete a different message at version 1 and the room keeps one envelope.
/// The two that lost sign at the same version, one wins, and the other signs once more: a
/// bounded number of envelopes, all three deleted, then quiet.
#[test]
fn three_devices_that_all_hold_records_converge_in_a_bounded_number_of_envelopes() {
    let f = Fixture::new();
    let msgs = messages(&f, 3);
    let tokens: Vec<PurgeToken> = msgs.iter().map(|m| m.purge_token()).collect();
    let mut devices: Vec<Device> = (0..3)
        .map(|i| device(&dir(&format!("storm-{i}")), &msgs))
        .collect();
    let mut node = Node::of(&devices[0]);
    let me = devices[0].me();
    for (i, d) in devices.iter_mut().enumerate() {
        d.delete(&[tokens[i]]);
        assert!(d.send(&mut node));
    }
    assert_eq!(node.purged(me).len(), 1);
    let signed = settle(&mut devices, &mut node);
    // Two losers sign at version 2 (one wins), the loser of that signs at version 3.
    assert!(signed <= 3, "{signed} envelopes: {:?}", node.versions);
    assert!(signed >= 1);
    assert_eq!(node.purged(me), sorted(tokens.clone()));
    for d in &devices {
        assert_eq!(d.holds(), sorted(tokens.clone()));
    }
    assert_eq!(settle(&mut devices, &mut node), 0, "{:?}", node.versions);
}

/// The record is written before the deletion is sent, so a crash between the two loses
/// nothing: after the restart the room does not hold the token and the record does.
#[test]
fn a_crash_between_the_record_and_the_send_still_deletes_the_message() {
    let f = Fixture::new();
    let msgs = messages(&f, 2);
    let tokens: Vec<PurgeToken> = msgs.iter().map(|m| m.purge_token()).collect();
    let dir = dir("crash");
    let mut first = device(&dir, &msgs);
    let mut node = Node::of(&first);
    let me = first.me();
    first.delete(&[tokens[0]]);
    // Queued, not yet sent: nothing is recorded (a deletion that never went out, and may be
    // refused, is not one this device is to sign behind the user's back).
    assert!(
        PurgeRecord::open(&dir)
            .for_room(&first.rooms.list[0].stored.owner_vk)
            .is_none()
    );
    // It goes out: the record is on disk first. The process dies before the node takes it.
    let mut changed = false;
    let delta = first
        .rooms
        .outbox
        .next_update(&first.rooms.list[0], Instant::now(), &mut changed)
        .expect("it is sent");
    assert_eq!(delta.direct_messages.unwrap().advanced_purges.len(), 1);
    let on_disk = PurgeRecord::open(&dir);
    assert_eq!(
        on_disk
            .for_room(&first.rooms.list[0].stored.owner_vk)
            .unwrap()
            .purged,
        vec![(me, tokens[0])]
    );
    drop(first);

    // Restarted: a new outbox that holds nothing, the record read from the file, and the room
    // as the node holds it, without the deletion.
    let mut second = device(&dir, &msgs);
    assert!(second.rooms.outbox.is_empty());
    assert!(node.purged(me).is_empty());

    // Not before the room has been read from the network.
    let owner = second.rooms.list[0].stored.owner_vk;
    second.rooms.network_seen.remove(&owner);
    second.rooms.own_purges_tick();
    assert!(second.rooms.outbox.is_empty());
    second.rooms.network_seen.insert(owner);

    second.hear(&node);
    assert!(second.send(&mut node), "the deletion is signed again");
    assert_eq!(node.purged(me), vec![tokens[0]]);
    assert_eq!(node.versions, vec![1]);
    assert_eq!(settle(std::slice::from_mut(&mut second), &mut node), 0);
}

#[test]
fn nothing_is_signed_again_when_the_room_holds_everything_recorded() {
    let f = Fixture::new();
    let msgs = messages(&f, 2);
    let tokens: Vec<PurgeToken> = msgs.iter().map(|m| m.purge_token()).collect();
    let mut device = device(&dir("quiet"), &msgs);
    let mut node = Node::of(&device);
    let me = device.me();
    device.delete(&[tokens[0]]);
    assert!(device.send(&mut node));
    device.delete(&[tokens[1]]);
    assert!(device.send(&mut node));
    assert_eq!(node.versions, vec![1, 2]);
    assert_eq!(settle(std::slice::from_mut(&mut device), &mut node), 0);
    assert_eq!(node.purged(me), sorted(tokens));
}

/// The record loses recipients that are no longer members once the room has been read from the
/// network, and keeps the user's own.
#[test]
fn the_record_is_pruned_to_the_rooms_members() {
    let f = Fixture::new();
    let msgs = messages(&f, 1);
    let dir = dir("prune");
    let mut device = device(&dir, &msgs);
    let node = Node::of(&device);
    let owner = device.rooms.list[0].stored.owner_vk;
    let gone = MemberId::from(&SigningKey::from_bytes(&[99; 32]).verifying_key());
    let me = device.me();
    let record = device.rooms.outbox.purge_record_mut();
    record.record(&owner, gone, &[PurgeToken([7; 16])]).unwrap();
    record.record(&owner, me, &[msgs[0].purge_token()]).unwrap();
    device.hear(&node);
    let on_disk = PurgeRecord::open(&dir);
    let own = on_disk.for_room(&owner).unwrap();
    assert!(own.purged.iter().all(|(r, _)| *r != gone), "{own:?}");
    assert_eq!(own.purged.len(), 1);
}

// Storage that fails whether or not the process may write (a regular file where the directory
// should be, so every path under it is "not a directory"; running as root changes nothing).

fn break_storage(dir: &Path) {
    std::fs::remove_dir_all(dir).unwrap();
    std::fs::write(dir, b"not a directory").unwrap();
}

fn mend_storage(dir: &Path) {
    std::fs::remove_file(dir).unwrap();
    std::fs::create_dir_all(dir).unwrap();
}

/// The first send of `device`'s queued deletion, which the node refuses.
fn sent_and_refused(device: &mut Device) {
    let mut changed = false;
    device
        .rooms
        .outbox
        .next_update(&device.rooms.list[0], Instant::now(), &mut changed)
        .expect("sent");
    let id = device.rooms.list[0].contract_id();
    device.rooms.update_answered(&id, Err("refused".into()));
}

fn owner_of(d: &Device) -> VerifyingKey {
    d.rooms.list[0].stored.owner_vk
}

/// The node refused a deletion that went out once: it was never taken, so it is no longer one
/// to sign again, at the moment of the refusal, and dismissing it changes nothing.
#[test]
fn a_deletion_the_node_refused_is_taken_out_of_the_record_when_refused() {
    let f = Fixture::new();
    let msgs = messages(&f, 1);
    let dir = dir("refused");
    let mut device = device(&dir, &msgs);
    let node = Node::of(&device);
    let owner = owner_of(&device);
    device.delete(&[msgs[0].purge_token()]);
    // Recorded as it goes out, and taken back as it is refused.
    sent_and_refused(&mut device);
    assert!(PurgeRecord::open(&dir).for_room(&owner).is_none());
    let row = device.rooms.outbox.purge_rows(&owner).remove(0);
    assert!(row.pending.failed.is_some());
    // Nothing signs it behind the user's back, now or after a restart.
    device.hear(&node);
    assert_eq!(device.rooms.outbox.purge_rows(&owner).len(), 1);
    let mut restarted = self::device(&dir, &msgs);
    restarted.hear(&node);
    assert!(restarted.rooms.outbox.is_empty());
    // Dismissing it does not touch the record.
    let room = device.rooms.list[0].clone();
    assert!(device.rooms.outbox.command(
        &room,
        Action::Discard {
            entry: row.pending.entry
        }
    ));
    assert!(PurgeRecord::open(&dir).for_room(&owner).is_none());
}

/// A deletion that went out twice may have been taken the first time: a refusal of the second
/// leaves it recorded, and dismissing it does not unrecord it. It is signed again.
#[test]
fn a_deletion_sent_twice_stays_recorded_when_refused_and_when_dismissed() {
    let f = Fixture::new();
    let msgs = messages(&f, 1);
    let token = msgs[0].purge_token();
    let dir = dir("sent-twice");
    let mut device = device(&dir, &msgs);
    let mut node = Node::of(&device);
    let owner = owner_of(&device);
    let me = device.me();
    device.delete(&[token]);
    let mut changed = false;
    device
        .rooms
        .outbox
        .next_update(&device.rooms.list[0], Instant::now(), &mut changed)
        .expect("sent");
    // The connection ended while it was owed an answer: it goes again.
    device.rooms.outbox.requeue();
    sent_and_refused(&mut device);
    assert!(PurgeRecord::open(&dir).for_room(&owner).is_some());
    let row = device.rooms.outbox.purge_rows(&owner).remove(0);
    let room = device.rooms.list[0].clone();
    assert!(device.rooms.outbox.command(
        &room,
        Action::Discard {
            entry: row.pending.entry
        }
    ));
    assert!(PurgeRecord::open(&dir).for_room(&owner).is_some());
    // The room does not hold it and the record does: it is signed again.
    assert_eq!(settle(std::slice::from_mut(&mut device), &mut node), 1);
    assert_eq!(node.purged(me), vec![token]);
}

/// A deletion stopped after it went out (River moved on, or the identity changed) is not one
/// the node refused: it stays recorded.
#[test]
fn a_sent_deletion_that_is_failed_without_an_answer_stays_recorded() {
    let f = Fixture::new();
    let msgs = messages(&f, 1);
    let dir = dir("failed-queued");
    let mut device = device(&dir, &msgs);
    let owner = owner_of(&device);
    device.delete(&[msgs[0].purge_token()]);
    let mut changed = false;
    device
        .rooms
        .outbox
        .next_update(&device.rooms.list[0], Instant::now(), &mut changed)
        .expect("sent");
    device.rooms.outbox.requeue();
    assert!(device.rooms.outbox.fail_queued("River has moved on"));
    assert!(PurgeRecord::open(&dir).for_room(&owner).is_some());
    // The failed entry stays shown and holds nothing up: what was sent is signed again.
    let node = Node::of(&device);
    device.hear(&node);
    assert!(device.rooms.outbox.heal_in_progress(&owner));
}

/// The node refused it and the record could not be taken back (the storage failed): the tokens
/// stay out of re-signing for the run, and the entry says why.
#[test]
fn a_record_that_cannot_be_taken_back_keeps_the_deletion_out_of_re_signing() {
    let f = Fixture::new();
    let msgs = messages(&f, 1);
    let dir = dir("unrecord-fails");
    let mut device = device(&dir, &msgs);
    let node = Node::of(&device);
    let owner = owner_of(&device);
    device.delete(&[msgs[0].purge_token()]);
    let mut changed = false;
    device
        .rooms
        .outbox
        .next_update(&device.rooms.list[0], Instant::now(), &mut changed)
        .expect("sent");
    break_storage(&dir);
    let id = device.rooms.list[0].contract_id();
    device.rooms.update_answered(&id, Err("refused".into()));
    let failed = device
        .rooms
        .outbox
        .purge_rows(&owner)
        .remove(0)
        .pending
        .failed;
    assert!(failed.unwrap().contains("could not update its record"));
    // Still in memory, but not signed.
    assert!(
        device
            .rooms
            .outbox
            .purge_record()
            .for_room(&owner)
            .is_some()
    );
    device.hear(&node);
    assert_eq!(device.rooms.outbox.purge_rows(&owner).len(), 1);
    assert!(!device.rooms.outbox.heal_in_progress(&owner));
    // Still owed while the storage fails; taken back by the next tick that is due once it
    // works, not left for a restart to sign.
    device.rooms.purge_retry_at = None;
    device.rooms.own_purges_tick();
    assert!(
        device.rooms.purge_retry_at.is_some(),
        "waits before trying again"
    );
    mend_storage(&dir);
    PurgeRecord::open(&dir)
        .record(&owner, device.me(), &[msgs[0].purge_token()])
        .unwrap();
    device.rooms.purge_retry_at = None;
    device.rooms.own_purges_tick();
    assert!(device.rooms.purge_retry_at.is_none());
    assert!(PurgeRecord::open(&dir).for_room(&owner).is_none());
    assert!(!device.rooms.outbox.retry_takebacks());
}

/// A deletion whose record cannot be saved is not sent.
#[test]
fn a_deletion_whose_record_cannot_be_saved_is_not_sent() {
    let f = Fixture::new();
    let msgs = messages(&f, 1);
    let dir = dir("unwritable");
    let mut device = device(&dir, &msgs);
    let node = Node::of(&device);
    device.delete(&[msgs[0].purge_token()]);
    break_storage(&dir);
    let mut changed = false;
    let sent = device
        .rooms
        .outbox
        .next_update(&device.rooms.list[0], Instant::now(), &mut changed);
    assert!(sent.is_none());
    assert!(changed);
    let owner = owner_of(&device);
    let failed = device
        .rooms
        .outbox
        .purge_rows(&owner)
        .remove(0)
        .pending
        .failed;
    assert!(failed.unwrap().contains("not sent"));
    assert!(node.purged(device.me()).is_empty());
}

/// Two devices raced; returns them, the node, the index of the one that lost, and the tokens.
fn raced(name: &str) -> (Vec<Device>, Node, usize, Vec<PurgeToken>) {
    let f = Fixture::new();
    let msgs = messages(&f, 2);
    let tokens: Vec<PurgeToken> = msgs.iter().map(|m| m.purge_token()).collect();
    let mut devices: Vec<Device> = (0..2)
        .map(|i| device(&dir(&format!("{name}-{i}")), &msgs))
        .collect();
    let mut node = Node::of(&devices[0]);
    for (i, d) in devices.iter_mut().enumerate() {
        d.delete(&[tokens[i]]);
        assert!(d.send(&mut node));
    }
    let kept = node.purged(devices[0].me());
    let loser = usize::from(kept.contains(&tokens[0]));
    (devices, node, loser, tokens)
}

/// A re-signed envelope the node refuses is dropped without a trace on screen, holds nothing
/// up, is not tried again for the same winning version, and the next winning envelope that
/// omits a recorded token derives a new one.
#[test]
fn a_failed_re_signed_envelope_is_dropped_and_the_next_winning_version_derives_a_new_one() {
    let (mut devices, mut node, loser, tokens) = raced("failed-heal");
    let owner = owner_of(&devices[loser]);
    let me = devices[loser].me();
    devices[loser].hear(&node);
    assert!(devices[loser].rooms.outbox.heal_in_progress(&owner));
    // It is background work: no row in the thread, no purge pending.
    assert!(devices[loser].rooms.outbox.purge_rows(&owner).is_empty());
    assert!(!devices[loser].rooms.outbox.purge_pending(&owner));
    sent_and_refused(&mut devices[loser]);
    devices[loser].hear(&node);
    assert!(devices[loser].rooms.outbox.is_empty(), "dropped");
    // Same winning version: not tried again.
    devices[loser].hear(&node);
    assert!(devices[loser].rooms.outbox.is_empty());
    // Nothing it depends on changed, so the tick does not even look again.
    devices[loser].rooms.healed_run.clear();
    devices[loser].hear(&node);
    assert!(devices[loser].rooms.outbox.is_empty());

    // Another device's envelope, one version higher, omits the loser's token again.
    let held = node.state.direct_messages.purges[0].clone();
    let next = river_core::room_state::direct_messages::advance_recipient_purges(
        &Fixture::new().me,
        &owner,
        Some(&held),
        [PurgeToken([9; 16])],
    )
    .unwrap();
    assert!(!next.state.purged.contains(&tokens[loser]));
    node.take(&ChatRoomStateV1Delta {
        direct_messages: Some(DirectMessagesDelta {
            new_messages: Vec::new(),
            advanced_purges: vec![next],
        }),
        ..Default::default()
    });
    devices[loser].hear(&node);
    assert!(devices[loser].rooms.outbox.heal_in_progress(&owner));
    assert!(devices[loser].send(&mut node));
    assert!(node.purged(me).contains(&tokens[loser]));
}

/// A heal that is waiting when River has moved on fails and is dropped; none is derived while
/// writes are stopped.
#[test]
fn nothing_is_re_signed_once_river_has_moved_on_and_a_waiting_envelope_is_dropped() {
    let (mut devices, node, loser, _) = raced("moved-on");
    let owner = owner_of(&devices[loser]);
    devices[loser].hear(&node);
    assert!(devices[loser].rooms.outbox.heal_in_progress(&owner));
    // What the session does when its check says River has moved on.
    devices[loser].rooms.update_required = true;
    assert!(
        devices[loser]
            .rooms
            .outbox
            .fail_queued("River has moved on")
    );
    devices[loser].rooms.own_purges_tick();
    assert!(devices[loser].rooms.outbox.is_empty());
    // And none is derived while it holds.
    devices[loser].rooms.purge_tick_seen.clear();
    devices[loser].rooms.healed_run.clear();
    devices[loser].rooms.own_purges_tick();
    assert!(devices[loser].rooms.outbox.is_empty());
}

/// When the room's envelope already holds the most tokens an envelope can, nothing is signed
/// and the winning version is not used up.
#[test]
fn a_full_envelope_is_not_signed_over_and_does_not_use_up_the_version() {
    use river_core::room_state::direct_messages::{
        MAX_PURGED_TOMBSTONES_PER_RECIPIENT, advance_recipient_purges,
    };
    let f = Fixture::new();
    let msgs = messages(&f, 1);
    let mut device = device(&dir("cap"), &msgs);
    let owner = owner_of(&device);
    let me = device.me();
    let many: Vec<PurgeToken> = (0..MAX_PURGED_TOMBSTONES_PER_RECIPIENT as u32)
        .map(|n| {
            let mut t = [0u8; 16];
            t[..4].copy_from_slice(&(n + 1000).to_le_bytes());
            PurgeToken(t)
        })
        .collect();
    let full = advance_recipient_purges(&f.me, &owner, None, many).unwrap();
    device.rooms.list[0]
        .apply_delta(ChatRoomStateV1Delta {
            direct_messages: Some(DirectMessagesDelta {
                new_messages: Vec::new(),
                advanced_purges: vec![full],
            }),
            ..Default::default()
        })
        .unwrap();
    device
        .rooms
        .outbox
        .purge_record_mut()
        .record(&owner, me, &[PurgeToken([5; 16])])
        .unwrap();
    device.rooms.own_purges_tick();
    assert!(device.rooms.outbox.is_empty());
    assert!(!device.rooms.healed_run.contains_key(&owner));
}

/// A record that could not be removed when a leave ended is removed when the storage works
/// again.
#[test]
fn a_failed_removal_of_a_left_rooms_record_is_retried() {
    let f = Fixture::new();
    let msgs = messages(&f, 1);
    let dir = dir("forget");
    let mut device = device(&dir, &msgs);
    let owner = owner_of(&device);
    device
        .rooms
        .outbox
        .purge_record_mut()
        .record(&owner, MemberId::from(&owner), &[PurgeToken([4; 16])])
        .unwrap();
    device.rooms.purge_forget_owed.insert(owner);
    break_storage(&dir);
    device.rooms.own_purges_tick();
    assert!(device.rooms.purge_forget_owed.contains(&owner));
    mend_storage(&dir);
    device.rooms.purge_retry_at = None;
    device.rooms.own_purges_tick();
    assert!(device.rooms.purge_forget_owed.is_empty());
    assert!(PurgeRecord::open(&dir).for_room(&owner).is_none());
}

/// Trying a refused deletion again: an owed take-back is no longer owed, and a take-back that
/// worked means the deletion is recorded again as it goes out.
#[test]
fn trying_a_refused_deletion_again_records_it_again_and_clears_what_was_owed() {
    let f = Fixture::new();
    let msgs = messages(&f, 1);
    let token = msgs[0].purge_token();
    let dir = dir("retry");
    let mut device = device(&dir, &msgs);
    let owner = owner_of(&device);
    device.delete(&[token]);
    sent_and_refused(&mut device);
    assert!(PurgeRecord::open(&dir).for_room(&owner).is_none());
    let entry = device
        .rooms
        .outbox
        .purge_rows(&owner)
        .remove(0)
        .pending
        .entry;
    let room = device.rooms.list[0].clone();
    assert!(device.rooms.outbox.command(&room, Action::Retry { entry }));
    let mut changed = false;
    device
        .rooms
        .outbox
        .next_update(&device.rooms.list[0], Instant::now(), &mut changed)
        .expect("sent again");
    assert_eq!(
        PurgeRecord::open(&dir).for_room(&owner).unwrap().purged,
        vec![(device.me(), token)],
        "recorded again as it went out"
    );

    // Refused with the storage failing: a take-back is owed; trying again clears that.
    let id = device.rooms.list[0].contract_id();
    break_storage(&dir);
    device.rooms.update_answered(&id, Err("refused".into()));
    assert!(device.rooms.outbox.retry_takebacks(), "owed");
    assert!(device.rooms.outbox.command(&room, Action::Retry { entry }));
    // Still broken, and yet nothing is owed any more: that try is the deletion's again.
    assert!(!device.rooms.outbox.retry_takebacks(), "no longer owed");
    mend_storage(&dir);
    // The tokens are this try's again: when it is refused too, they are taken back.
    PurgeRecord::open(&dir)
        .record(&owner, device.me(), &[token])
        .unwrap();
    let mut changed = false;
    device
        .rooms
        .outbox
        .next_update(&device.rooms.list[0], Instant::now(), &mut changed)
        .expect("sent again");
    device
        .rooms
        .update_answered(&id, Err("refused again".into()));
    assert!(
        PurgeRecord::open(&dir).for_room(&owner).is_none(),
        "the second refusal took the tokens back"
    );
}

/// A deletion asked for again while the take-back of an earlier try of the same token is owed:
/// the take-back does not undo it.
#[test]
fn an_owed_take_back_does_not_undo_a_later_deletion_of_the_same_token() {
    let f = Fixture::new();
    let msgs = messages(&f, 1);
    let token = msgs[0].purge_token();
    let dir = dir("owed-then-again");
    let mut device = device(&dir, &msgs);
    let owner = owner_of(&device);
    let me = device.me();
    device.delete(&[token]);
    let mut changed = false;
    device
        .rooms
        .outbox
        .next_update(&device.rooms.list[0], Instant::now(), &mut changed)
        .expect("sent");
    break_storage(&dir);
    let id = device.rooms.list[0].contract_id();
    device.rooms.update_answered(&id, Err("refused".into()));
    assert!(device.rooms.outbox.retry_takebacks(), "owed");
    // The storage works again; the user asks again.
    mend_storage(&dir);
    device.delete(&[token]);
    let mut changed = false;
    device
        .rooms
        .outbox
        .next_update(&device.rooms.list[0], Instant::now(), &mut changed)
        .expect("sent again");
    // Nothing is owed any more, and the deletion is recorded.
    assert!(!device.rooms.outbox.retry_takebacks());
    assert_eq!(
        PurgeRecord::open(&dir).for_room(&owner).unwrap().purged,
        vec![(me, token)]
    );
    // And as this deletion's own, it is taken back if the node refuses it.
    device.rooms.update_answered(&id, Err("refused".into()));
    assert!(PurgeRecord::open(&dir).for_room(&owner).is_none());
}

/// Who the user is is read by the tick: when the identity changes to one that has a recorded
/// token, with nothing else changed, the tick looks again.
#[test]
fn a_change_of_identity_makes_the_tick_look_again() {
    let f = Fixture::new();
    let msgs = messages(&f, 1);
    let mut device = device(&dir("identity"), &msgs);
    let owner = owner_of(&device);
    let other = SigningKey::from_bytes(&[95; 32]);
    let other_id = MemberId::from(&other.verifying_key());
    device.rooms.list[0].stored.room_state.members.members.push(
        river_core::room_state::member::AuthorizedMember::new(
            river_core::room_state::member::Member {
                owner_member_id: f.owner_id(),
                invited_by: f.owner_id(),
                member_vk: other.verifying_key(),
            },
            &f.owner,
        ),
    );
    device
        .rooms
        .outbox
        .purge_record_mut()
        .record(&owner, other_id, &[PurgeToken([6; 16])])
        .unwrap();
    // The user is the first identity, which has nothing recorded.
    device.rooms.own_purges_tick();
    assert!(device.rooms.outbox.is_empty());
    // Replaced by the second, which is a member too; neither holds an envelope.
    device.rooms.list[0].stored.self_sk = Some(other);
    device.rooms.own_purges_tick();
    assert!(device.rooms.outbox.heal_in_progress(&owner));
}

/// Entry E1 was taken by the node (and shown as not kept); the same deletion asked for again
/// is E2, which the node refuses: E2 added nothing to the record, so it takes nothing back.
#[test]
fn a_refused_second_deletion_does_not_take_back_what_an_earlier_one_recorded() {
    let f = Fixture::new();
    let msgs = messages(&f, 1);
    let token = msgs[0].purge_token();
    let dir = dir("e1-e2");
    let mut device = device(&dir, &msgs);
    let owner = owner_of(&device);
    let me = device.me();
    // E1, as it was recorded when it went out and the node took it.
    device
        .rooms
        .outbox
        .purge_record_mut()
        .record(&owner, me, &[token])
        .unwrap();
    device.delete(&[token]);
    sent_and_refused(&mut device);
    assert_eq!(
        PurgeRecord::open(&dir).for_room(&owner).unwrap().purged,
        vec![(me, token)]
    );
}

/// A re-signed envelope the node took but the room did not keep is dropped like a failed one.
#[test]
fn a_re_signed_envelope_the_room_did_not_keep_is_dropped() {
    let (mut devices, mut node, loser, tokens) = raced("unconfirmed-heal");
    let owner = owner_of(&devices[loser]);
    devices[loser].hear(&node);
    let d = &mut devices[loser];
    let mut changed = false;
    d.rooms
        .outbox
        .next_update(&d.rooms.list[0], Instant::now(), &mut changed)
        .expect("sent");
    // Meanwhile a higher-ranked envelope reached this device: two versions above the winner,
    // so it outranks our re-signed one whatever the signature digests are.
    let held = node.state.direct_messages.purges[0].clone();
    let sk = Fixture::new().me;
    let two = river_core::room_state::direct_messages::advance_recipient_purges(
        &sk,
        &owner,
        Some(&held),
        [PurgeToken([8; 16])],
    )
    .unwrap();
    let next = river_core::room_state::direct_messages::advance_recipient_purges(
        &sk,
        &owner,
        Some(&two),
        [PurgeToken([9; 16])],
    )
    .unwrap();
    assert!(!next.state.purged.contains(&tokens[loser]));
    node.take(&ChatRoomStateV1Delta {
        direct_messages: Some(DirectMessagesDelta {
            new_messages: Vec::new(),
            advanced_purges: vec![next],
        }),
        ..Default::default()
    });
    let id = devices[loser].rooms.list[0].contract_id();
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(&node.state, &mut bytes).unwrap();
    devices[loser]
        .rooms
        .apply_update(&id, &UpdateData::State(State::from(bytes)));
    devices[loser].rooms.update_answered(&id, Ok(()));
    assert!(!devices[loser].rooms.outbox.is_empty(), "shown as not kept");
    // The winning version is one this run has signed against already, so the tick derives
    // nothing new and only the dropping shows.
    let me = devices[loser].me();
    devices[loser].rooms.healed_run.insert(owner, vec![(me, 3)]);
    devices[loser].rooms.own_purges_tick();
    assert!(devices[loser].rooms.outbox.is_empty(), "dropped");
}

/// A deletion that was sent, then stopped without an answer, says it may still be signed.
#[test]
fn a_sent_deletion_that_was_stopped_says_it_may_still_be_signed() {
    let f = Fixture::new();
    let msgs = messages(&f, 1);
    let mut device = device(&dir("sent-note"), &msgs);
    let owner = owner_of(&device);
    device.delete(&[msgs[0].purge_token()]);
    let mut changed = false;
    device
        .rooms
        .outbox
        .next_update(&device.rooms.list[0], Instant::now(), &mut changed)
        .expect("sent");
    device.rooms.outbox.requeue();
    assert!(device.rooms.outbox.fail_queued("River has moved on."));
    let why = device
        .rooms
        .outbox
        .purge_rows(&owner)
        .remove(0)
        .pending
        .failed;
    assert!(why.unwrap().contains("may still sign it again"));
}

// What the tick reads: each input of the fingerprint, changed on its own, makes it look again.

/// A recipient the room lists is kept; when the room stops listing them, the tick looks again
/// (the record, the envelopes and the configuration are as they were) and drops their entry.
#[test]
fn a_change_of_members_makes_the_tick_look_again() {
    let f = Fixture::new();
    let msgs = messages(&f, 1);
    let dir = dir("members");
    let mut device = device(&dir, &msgs);
    let owner = owner_of(&device);
    let other = SigningKey::from_bytes(&[96; 32]);
    let other_id = MemberId::from(&other.verifying_key());
    let listed = river_core::room_state::member::AuthorizedMember::new(
        river_core::room_state::member::Member {
            owner_member_id: f.owner_id(),
            invited_by: f.owner_id(),
            member_vk: other.verifying_key(),
        },
        &f.owner,
    );
    device.rooms.list[0]
        .stored
        .room_state
        .members
        .members
        .push(listed);
    device
        .rooms
        .outbox
        .purge_record_mut()
        .record(&owner, other_id, &[PurgeToken([6; 16])])
        .unwrap();
    device.rooms.own_purges_tick();
    assert!(
        PurgeRecord::open(&dir).for_room(&owner).is_some(),
        "listed: kept"
    );
    device.rooms.list[0]
        .stored
        .room_state
        .members
        .members
        .retain(|m| m.member.id() != other_id);
    device.rooms.own_purges_tick();
    assert!(
        PurgeRecord::open(&dir).for_room(&owner).is_none(),
        "not listed: dropped"
    );
}

/// Pruning needs a configuration the owner signed: when it becomes valid the tick looks again.
#[test]
fn a_change_of_configuration_makes_the_tick_look_again() {
    let f = Fixture::new();
    let msgs = messages(&f, 1);
    let dir = dir("config");
    let mut device = device(&dir, &msgs);
    let owner = owner_of(&device);
    let gone = MemberId::from(&SigningKey::from_bytes(&[98; 32]).verifying_key());
    device
        .rooms
        .outbox
        .purge_record_mut()
        .record(&owner, gone, &[PurgeToken([8; 16])])
        .unwrap();
    let good = device.rooms.list[0]
        .stored
        .room_state
        .configuration
        .signature;
    device.rooms.list[0]
        .stored
        .room_state
        .configuration
        .signature = SigningKey::from_bytes(&[8; 32]).sign(b"not the owner");
    device.rooms.own_purges_tick();
    assert!(
        PurgeRecord::open(&dir).for_room(&owner).is_some(),
        "forged: nothing pruned"
    );
    device.rooms.list[0]
        .stored
        .room_state
        .configuration
        .signature = good;
    device.rooms.own_purges_tick();
    assert!(
        PurgeRecord::open(&dir).for_room(&owner).is_none(),
        "valid: pruned"
    );
}

/// The record changes (a recipient that is not a member is added) with the room as it was: the
/// tick looks again and prunes it.
#[test]
fn a_change_of_the_record_makes_the_tick_look_again() {
    let f = Fixture::new();
    let msgs = messages(&f, 1);
    let dir = dir("rev");
    let mut device = device(&dir, &msgs);
    let owner = owner_of(&device);
    // Recorded as the owner, whom the room always lists and whose tokens nothing re-signs.
    device
        .rooms
        .outbox
        .purge_record_mut()
        .record(&owner, MemberId::from(&owner), &[PurgeToken([1; 16])])
        .unwrap();
    device.rooms.own_purges_tick();
    let gone = MemberId::from(&SigningKey::from_bytes(&[97; 32]).verifying_key());
    device
        .rooms
        .outbox
        .purge_record_mut()
        .record(&owner, gone, &[PurgeToken([2; 16])])
        .unwrap();
    device.rooms.own_purges_tick();
    let own = PurgeRecord::open(&dir);
    assert!(
        own.for_room(&owner)
            .unwrap()
            .purged
            .iter()
            .all(|(r, _)| *r != gone)
    );
}

/// The storage fails for a while with the record file intact (moved out of the way and back):
/// a take-back owed meanwhile, then the same token deleted again. The token is already on disk,
/// so the new deletion adds nothing; it must still own the token, so that its own refusal takes
/// it back.
#[test]
fn a_token_taken_over_from_an_owed_take_back_is_taken_back_with_its_new_deletion() {
    let f = Fixture::new();
    let msgs = messages(&f, 1);
    let token = msgs[0].purge_token();
    let dir = dir("taken-over");
    let parked = dir.with_extension("parked");
    let _ = std::fs::remove_dir_all(&parked);
    let mut device = device(&dir, &msgs);
    let owner = owner_of(&device);
    device.delete(&[token]);
    let mut changed = false;
    device
        .rooms
        .outbox
        .next_update(&device.rooms.list[0], Instant::now(), &mut changed)
        .expect("sent");
    // The directory is out of the way: the refusal cannot be taken back.
    std::fs::rename(&dir, &parked).unwrap();
    std::fs::write(&dir, b"x").unwrap();
    let id = device.rooms.list[0].contract_id();
    device.rooms.update_answered(&id, Err("refused".into()));
    assert!(device.rooms.outbox.retry_takebacks(), "owed");
    // And back, with the token still recorded on disk.
    std::fs::remove_file(&dir).unwrap();
    std::fs::rename(&parked, &dir).unwrap();
    assert!(PurgeRecord::open(&dir).for_room(&owner).is_some());
    device.delete(&[token]);
    let mut changed = false;
    device
        .rooms
        .outbox
        .next_update(&device.rooms.list[0], Instant::now(), &mut changed)
        .expect("sent again");
    device
        .rooms
        .update_answered(&id, Err("refused again".into()));
    assert!(
        PurgeRecord::open(&dir).for_room(&owner).is_none(),
        "the deletion that took the token over took it back"
    );
}
