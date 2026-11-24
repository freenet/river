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
use ed25519_dalek::SigningKey;

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
    panic!("the devices kept signing envelopes: {} taken", node.took - before);
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
    assert_eq!(kept.len(), 1, "the room kept one of the two, not their union");
    assert_eq!(node.version(me), 1);

    for d in &mut devices {
        d.hear(&node);
    }
    // The device whose token the winner omits.
    let lost: Vec<usize> = (0..2)
        .filter(|&i| {
            let mine = tokens[if i == 0 { first_deletes } else { 1 - first_deletes }];
            !kept.contains(&mine)
        })
        .collect();
    assert_eq!(lost.len(), 1);

    let signed = settle(&mut devices, &mut node);
    assert_eq!(signed, 1, "the loser signs the union once: {:?}", node.versions);
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
    assert_ne!(a, b, "the device that loses changes with which one deleted what");
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

/// A deletion the room refused is shown as failed with Retry and Discard. It is not signed
/// again behind the user's back while it is shown, and once dismissed it is not recorded.
#[test]
fn a_refused_deletion_is_not_signed_again_and_a_dismissed_one_is_unrecorded() {
    let f = Fixture::new();
    let msgs = messages(&f, 1);
    let token = msgs[0].purge_token();
    let dir = dir("refused");
    let mut device = device(&dir, &msgs);
    let node = Node::of(&device);
    let owner = device.rooms.list[0].stored.owner_vk;
    device.delete(&[token]);
    let mut changed = false;
    device
        .rooms
        .outbox
        .next_update(&device.rooms.list[0], Instant::now(), &mut changed)
        .expect("sent");
    let id = device.rooms.list[0].contract_id();
    device.rooms.update_answered(&id, Err("refused".into()));
    let row = device.rooms.outbox.purge_rows(&owner).remove(0);
    assert!(row.pending.failed.is_some());
    assert!(PurgeRecord::open(&dir).for_room(&owner).is_some());

    // The room does not hold it and the record does, but the user was told it failed.
    device.hear(&node);
    assert!(device.rooms.outbox.purge_rows(&owner).len() == 1);
    assert!(!device.rooms.outbox.is_empty());
    let mut changed = false;
    assert!(
        device
            .rooms
            .outbox
            .next_update(&device.rooms.list[0], Instant::now(), &mut changed)
            .is_none(),
        "a failed entry is not sent and no second one was queued"
    );

    // Dismissed: no longer a deletion to sign.
    let room = device.rooms.list[0].clone();
    assert!(device.rooms.outbox.command(
        &room,
        Action::Discard {
            entry: row.pending.entry
        }
    ));
    assert!(device.rooms.outbox.is_empty());
    assert!(PurgeRecord::open(&dir).for_room(&owner).is_none());
    device.hear(&node);
    assert!(device.rooms.outbox.is_empty());
}

/// A record that cannot be saved stops the deletion rather than sending it unrecorded.
#[cfg(unix)]
#[test]
fn a_deletion_whose_record_cannot_be_saved_is_not_sent() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let msgs = messages(&f, 1);
    let token = msgs[0].purge_token();
    let dir = dir("unwritable");
    let mut device = device(&dir, &msgs);
    let node = Node::of(&device);
    device.delete(&[token]);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let writable = std::fs::File::create(dir.join("probe")).is_ok();
    let mut changed = false;
    let sent = device
        .rooms
        .outbox
        .next_update(&device.rooms.list[0], Instant::now(), &mut changed);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    if writable {
        // Running as a user the permission does not bind: nothing to check.
        return;
    }
    assert!(sent.is_none());
    assert!(changed);
    let owner = device.rooms.list[0].stored.owner_vk;
    let failed = device.rooms.outbox.purge_rows(&owner).remove(0).pending.failed;
    assert!(failed.unwrap().contains("not sent"));
    assert!(node.purged(device.me()).is_empty());
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
    record
        .record(&owner, me, &[msgs[0].purge_token()])
        .unwrap();
    device.hear(&node);
    let on_disk = PurgeRecord::open(&dir);
    let own = on_disk.for_room(&owner).unwrap();
    assert!(own.purged.iter().all(|(r, _)| *r != gone), "{own:?}");
    assert_eq!(own.purged.len(), 1);
}
