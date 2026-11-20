//! Archiving a thread on screen, driven as a person drives it: the rail's row action, the Undo
//! line (on a clock the test passes in), the fold-out, the thread header's action, and what a
//! newer message from the peer does. The rules are `rooms::dm_archive`'s own tests.

use std::time::{Duration, Instant};

use rune::{Key, KeyEvent, Runtime};

use super::dm_archive::{ARCHIVED_LINE, UNDO_FOR};
use super::dm_model::{DmArchived, DmSnapshot};
use super::dm_view_tests::*;
use super::{LinkStatus, Source};
use crate::RiverApp;
use crate::rooms::outbox::Action;

/// The app reading rooms from a node, with the conversation given (the peer's newest message is
/// at `T0 + 3600`, the user's reply between).
fn node_app() -> (Runtime<RiverApp>, Fixture) {
    let mut rt = app();
    rt.update(|s, _| s.chat.source = Source::Node(LinkStatus::Live));
    let (f, seed) = conversation_after_seed();
    give(&mut rt, seed);
    give(&mut rt, f.snapshot(true));
    (rt, f)
}

fn archive_key() -> String {
    format!("dm-archive-{ROOM}-{}", peer_key())
}

fn on_rail(rt: &Runtime<RiverApp>) -> bool {
    rt.find(&row_key()).is_some()
}

fn commands(rt: &Runtime<RiverApp>) -> Vec<Action> {
    rt.state
        .chat
        .queued_commands()
        .iter()
        .map(|c| c.action.clone())
        .collect()
}

fn archived_at(ts: u64) -> DmArchived {
    DmArchived {
        room: ROOM.to_owned(),
        peer_key: peer_key(),
        hidden_at_ts: ts,
    }
}

/// The newest time the peer sent in the fixture's conversation.
const PEER_NEWEST: u64 = T0 + 3600;

fn tab_to(rt: &mut Runtime<RiverApp>, key: &str) {
    for _ in 0..80 {
        if rt
            .elements()
            .iter()
            .any(|e| e.focused && e.key.as_deref() == Some(key))
        {
            return;
        }
        rt.key(KeyEvent {
            key: Key::Tab,
            mods: Default::default(),
        });
        rt.frame(0.05);
    }
    let f: Vec<_> = rt
        .elements()
        .into_iter()
        .filter(|e| e.focused)
        .map(|e| (e.key, e.path))
        .collect();
    panic!("Tab never reached {key}; focused: {f:?}");
}

fn space(rt: &mut Runtime<RiverApp>) {
    rt.key(KeyEvent {
        key: Key::Text(" ".into()),
        mods: Default::default(),
    });
    rt.frame(0.1);
    rt.frame(0.2);
}

fn enter(rt: &mut Runtime<RiverApp>) {
    rt.key(KeyEvent {
        key: Key::Enter,
        mods: Default::default(),
    });
    rt.frame(0.1);
    rt.frame(0.2);
}

#[test]
fn the_row_has_an_archive_button_that_takes_the_thread_off_the_rail_without_opening_it() {
    let (mut rt, _f) = node_app();
    assert!(on_rail(&rt));
    assert!(rt.find(&archive_key()).is_some(), "a button on the row");
    assert!(
        rt.find("dm-archived-toggle").is_none(),
        "nothing archived yet"
    );

    press(&mut rt, &archive_key());

    assert!(!on_rail(&rt), "off the rail");
    assert!(
        rt.find("dm-thread").is_none(),
        "pressing it did not open the thread"
    );
    // The section stays, for the fold-out and the Undo line.
    assert!(shows(&rt, "Direct messages"));
    assert!(shows(&rt, "Archived (1)"));
    assert!(rt.find("dm-undo-line").is_some());
    assert!(shows(&rt, &format!("{ARCHIVED_LINE} Priya")));
    // One command, with the cutoff the thread showed: the peer's newest message.
    let want = Action::ArchiveDm {
        peer: crate::rooms::dm::peer_from_key(&peer_key()).unwrap(),
        row_inbound_ts: PEER_NEWEST,
    };
    assert_eq!(commands(&rt), vec![want]);
}

#[test]
fn archiving_does_not_move_the_cutoff_to_the_users_own_message() {
    let mut rt = app();
    rt.update(|s, _| s.chat.source = Source::Node(LinkStatus::Live));
    let mut f = Fixture::new();
    f.compose(PEER, ME, T0, b"theirs");
    f.compose(ME, PEER, T0 + 9_999, b"my later reply");
    give(&mut rt, f.snapshot(true));
    press(&mut rt, &archive_key());
    let Some(Action::ArchiveDm { row_inbound_ts, .. }) = commands(&rt).pop() else {
        panic!("no archive command");
    };
    assert_eq!(row_inbound_ts, T0);
}

#[test]
fn the_fold_out_lists_archived_threads_and_unarchive_brings_one_back() {
    let (mut rt, _f) = node_app();
    press(&mut rt, &archive_key());
    assert!(rt.find("dm-archived-toggle").is_some());
    // Closed: no list.
    assert!(
        rt.find(&format!("dm-archived-{ROOM}-{}", peer_key()))
            .is_none()
    );

    press(&mut rt, "dm-archived-toggle");
    let listed = format!("dm-archived-{ROOM}-{}", peer_key());
    assert!(rt.find(&listed).is_some());
    assert!(shows(&rt, "in Town Square"));
    assert!(shows(&rt, "Un-archive"));

    press(&mut rt, &format!("dm-unarchive-{ROOM}-{}", peer_key()));
    assert!(on_rail(&rt), "back on the rail");
    assert!(rt.find("dm-archived-toggle").is_none(), "nothing archived");
    let last = commands(&rt).pop().unwrap();
    assert!(matches!(last, Action::UnarchiveDm { .. }), "{last:?}");
}

#[test]
fn the_undo_line_lasts_as_long_as_the_clock_passed_in_says_and_undo_takes_it_back() {
    let (mut rt, _f) = node_app();
    let t0 = Instant::now();
    let key = peer_key();
    rt.update(move |s, _| assert!(s.chat.dm_archive_at(ROOM, &key, t0)));
    rt.frame(0.1);
    assert!(rt.find("dm-undo-line").is_some());
    // One tick short of the limit: still there. At it: gone, and the archive stays.
    let (short, at) = (t0 + UNDO_FOR - Duration::from_millis(1), t0 + UNDO_FOR);
    rt.update(move |s, _| assert!(!s.chat.dm_tick(short)));
    rt.frame(0.1);
    assert!(rt.find("dm-undo-line").is_some());
    rt.update(move |s, _| assert!(s.chat.dm_tick(at)));
    rt.frame(0.2);
    rt.frame(0.7);
    assert!(rt.find("dm-undo-line").is_none());
    assert!(!on_rail(&rt), "the thread stays archived");

    // Archive again and press Undo inside the time: it comes back, and the line goes.
    let key = peer_key();
    rt.update(|s, _| assert!(s.chat.dm_unarchive(ROOM, &key)));
    let key = peer_key();
    let t1 = Instant::now();
    rt.update(move |s, _| assert!(s.chat.dm_archive_at(ROOM, &key, t1)));
    rt.frame(0.1);
    press(&mut rt, "dm-undo");
    assert!(on_rail(&rt));
    assert!(rt.find("dm-undo-line").is_none());
    assert!(matches!(
        commands(&rt).last().unwrap(),
        Action::UnarchiveDm { .. }
    ));
}

#[test]
fn the_archive_code_reads_no_clock() {
    let src = std::fs::read_to_string(format!(
        "{}/src/chat/dm_archive.rs",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
    .replace("\r\n", "\n");
    let (code, _) = src
        .split_once("// The view ---")
        .expect("the marker of the view");
    let code: String = code
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    // `dm_archive` is the one place the real time is read, and only to pass it on.
    let reads: Vec<&str> = code
        .lines()
        .filter(|l| l.contains("Instant::now"))
        .collect();
    assert_eq!(reads.len(), 1, "{reads:?}");
    assert!(reads[0].contains("self.dm_archive_at(room, peer, Instant::now())"));
    assert!(!code.contains("SystemTime"));
}

#[test]
fn a_thread_archived_elsewhere_is_off_the_rail_here_and_counted() {
    let (mut rt, f) = node_app();
    assert!(on_rail(&rt));
    let mut snapshot = f.snapshot(true);
    snapshot.archived = vec![archived_at(PEER_NEWEST)];
    give(&mut rt, snapshot);
    assert!(!on_rail(&rt));
    assert!(shows(&rt, "Archived (1)"));
    assert!(
        rt.find("dm-undo-line").is_none(),
        "no Undo for what this window did not do"
    );
    // And the web UI unarchiving it (the list read again without it) brings it back.
    give(&mut rt, f.snapshot(true));
    assert!(on_rail(&rt));
    assert!(rt.find("dm-archived-toggle").is_none());
}

#[test]
fn a_message_from_the_peer_past_the_cutoff_brings_the_thread_back_with_its_count() {
    let (mut rt, mut f) = node_app();
    let mut snapshot = f.snapshot(true);
    snapshot.archived = vec![archived_at(PEER_NEWEST)];
    give(&mut rt, snapshot.clone());
    assert!(!on_rail(&rt));
    // At the cutoff: still archived (the same second as the peer's previous message waits).
    f.compose(PEER, ME, PEER_NEWEST, b"same second");
    let mut same = f.snapshot(true);
    same.archived = vec![archived_at(PEER_NEWEST)];
    give(&mut rt, same);
    assert!(!on_rail(&rt));
    // One second later it is back, unread.
    f.compose(PEER, ME, PEER_NEWEST + 1, b"new");
    let mut newer = f.snapshot(true);
    newer.archived = vec![archived_at(PEER_NEWEST)];
    give(&mut rt, newer);
    assert!(on_rail(&rt));
    assert!(rt.find("dm-unread").is_some());
    assert!(
        rt.find("dm-archived-toggle").is_none(),
        "no longer archived"
    );
}

#[test]
fn a_message_the_user_sends_never_brings_it_back_by_its_time() {
    let (mut rt, mut f) = node_app();
    let mut snapshot = f.snapshot(true);
    snapshot.archived = vec![archived_at(PEER_NEWEST)];
    give(&mut rt, snapshot);
    // Our reply, stamped by our clock far past the cutoff: the archive clock is the peer's.
    f.compose(ME, PEER, PEER_NEWEST + 100_000, b"stamped by my clock");
    let mut again = f.snapshot(true);
    again.archived = vec![archived_at(PEER_NEWEST)];
    give(&mut rt, again);
    assert!(!on_rail(&rt));
    // (Sending unarchives through the background work, which removes the entry: see
    // `rooms::session` tests.)
}

#[test]
fn a_thread_with_no_message_from_the_peer_archives_at_zero_and_stays_off() {
    let mut rt = app();
    rt.update(|s, _| s.chat.source = Source::Node(LinkStatus::Live));
    let mut f = Fixture::new();
    f.compose(ME, PEER, T0, b"only mine");
    give(&mut rt, f.snapshot(true));
    press(&mut rt, &archive_key());
    let Some(Action::ArchiveDm { row_inbound_ts, .. }) = commands(&rt).pop() else {
        panic!("no archive command");
    };
    assert_eq!(row_inbound_ts, 0);
    assert!(!on_rail(&rt));
}

#[test]
fn what_the_window_just_did_holds_against_a_snapshot_built_before_it() {
    let (mut rt, f) = node_app();
    press(&mut rt, &archive_key());
    assert!(!on_rail(&rt));
    // A snapshot the background work built before it took the command arrives late.
    give(&mut rt, f.snapshot(true));
    assert!(!on_rail(&rt), "no flash back onto the rail");
    // The snapshot that holds it settles the change, and the list is the background work's.
    let mut held = f.snapshot(true);
    held.archived = vec![archived_at(PEER_NEWEST)];
    give(&mut rt, held);
    assert!(!on_rail(&rt));
    // Now the web UI unarchives: the window follows (nothing pending is left to hold it).
    give(&mut rt, f.snapshot(true));
    assert!(on_rail(&rt), "the pending change was settled and is gone");
}

#[test]
fn an_unarchive_holds_against_a_stale_snapshot_too() {
    let (mut rt, f) = node_app();
    let mut held = f.snapshot(true);
    held.archived = vec![archived_at(PEER_NEWEST)];
    give(&mut rt, held.clone());
    press(&mut rt, "dm-archived-toggle");
    press(&mut rt, &format!("dm-unarchive-{ROOM}-{}", peer_key()));
    assert!(on_rail(&rt));
    give(&mut rt, held);
    assert!(
        on_rail(&rt),
        "a snapshot from before does not archive it again"
    );
    give(&mut rt, f.snapshot(true));
    assert!(on_rail(&rt));
}

#[test]
fn unread_of_an_archived_thread_is_not_counted_until_it_returns() {
    let (mut rt, f) = node_app();
    assert_eq!(rt.state.chat.dm.unread_total(), 2);
    press(&mut rt, &archive_key());
    assert_eq!(rt.state.chat.dm.unread_total(), 0);
    let _ = f;
}

#[test]
fn the_thread_header_archives_closes_the_thread_and_offers_unarchive_for_an_archived_one() {
    let (mut rt, f) = node_app();
    let key = peer_key();
    rt.update(move |s, _| assert!(s.chat.open_dm(ROOM, &key)));
    rt.frame(0.2);
    assert!(rt.find("dm-archive").is_some(), "an action in the header");
    assert!(shows(&rt, "Archive"));
    press(&mut rt, "dm-archive");
    assert!(rt.find("dm-thread").is_none(), "back to the room");
    assert!(!on_rail(&rt));
    assert!(matches!(
        commands(&rt).last().unwrap(),
        Action::ArchiveDm { .. }
    ));

    // An archived thread opened from elsewhere (the member list) offers Un-archive.
    let key = peer_key();
    rt.update(move |s, _| assert!(s.chat.open_dm(ROOM, &key)));
    rt.frame(0.2);
    assert!(shows(&rt, "Un-archive"));
    press(&mut rt, "dm-archive");
    assert!(matches!(
        commands(&rt).last().unwrap(),
        Action::UnarchiveDm { .. }
    ));
    assert!(on_rail(&rt));
    let _ = f;
}

#[test]
fn archiving_is_off_with_the_reason_while_river_has_moved_on() {
    let (mut rt, _f) = node_app();
    rt.update(|s, _| s.chat.issues.update_required = true);
    rt.frame(0.1);
    let key = peer_key();
    rt.update(move |s, _| assert!(!s.chat.dm_archive(ROOM, &key)));
    assert!(on_rail(&rt));
    assert!(commands(&rt).is_empty());
    // The header's action is dimmed and says why.
    let key = peer_key();
    rt.update(move |s, _| assert!(s.chat.open_dm(ROOM, &key)));
    rt.frame(0.2);
    let action = rt
        .elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some("dm-archive"))
        .expect("the header's action");
    assert!(action.opacity < 0.9);
    assert!(
        action
            .help
            .as_deref()
            .is_some_and(|h| h.contains("newer version of River"))
    );
}

#[test]
fn an_unknown_thread_or_one_already_archived_is_refused() {
    let (mut rt, _f) = node_app();
    rt.update(|s, _| {
        assert!(!s.chat.dm_archive(ROOM, "nobody"));
        assert!(!s.chat.dm_archive("no-room", "1"));
        assert!(!s.chat.dm_unarchive(ROOM, "1"), "not archived");
    });
    press(&mut rt, &archive_key());
    let key = peer_key();
    rt.update(move |s, _| assert!(!s.chat.dm_archive(ROOM, &key), "archived already"));
    assert_eq!(commands(&rt).len(), 1);
}

#[test]
fn the_example_archives_on_screen_and_sends_nothing() {
    let mut rt = app();
    let (f, seed) = conversation_after_seed();
    give(&mut rt, seed);
    give(&mut rt, f.snapshot(true));
    press(&mut rt, &archive_key());
    assert!(!on_rail(&rt));
    assert!(commands(&rt).is_empty());
}

// The keyboard --------------------------------------------------------------------------------

#[test]
fn every_archive_control_is_reached_with_tab_and_pressed_with_enter() {
    let (mut rt, _f) = node_app();
    let focusable = |rt: &Runtime<RiverApp>, key: &str| {
        rt.elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some(key))
            .unwrap_or_else(|| panic!("no element {key}"))
            .focusable
    };
    assert!(focusable(&rt, &archive_key()));
    tab_to(&mut rt, &archive_key());
    enter(&mut rt);
    assert!(!on_rail(&rt));
    assert!(
        rt.find("dm-thread").is_none(),
        "Enter archived, it did not open the thread"
    );

    // The Undo line, the fold-out toggle and Un-archive too.
    assert!(focusable(&rt, "dm-undo"));
    assert!(focusable(&rt, "dm-archived-toggle"));
    tab_to(&mut rt, "dm-archived-toggle");
    enter(&mut rt);
    let un = format!("dm-unarchive-{ROOM}-{}", peer_key());
    assert!(focusable(&rt, &un));
    tab_to(&mut rt, &un);
    enter(&mut rt);
    assert!(on_rail(&rt), "Un-archive by keyboard");

    // And Undo.
    press(&mut rt, &archive_key());
    tab_to(&mut rt, "dm-undo");
    enter(&mut rt);
    assert!(on_rail(&rt), "Undo by keyboard");
}

#[test]
fn the_buttons_carry_accessible_names() {
    let (mut rt, _f) = node_app();
    let label = |rt: &Runtime<RiverApp>, key: &str| {
        rt.elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some(key))
            .and_then(|e| e.label)
    };
    assert_eq!(
        label(&rt, &archive_key()).as_deref(),
        Some("Archive conversation with Priya")
    );
    press(&mut rt, &archive_key());
    assert_eq!(
        label(&rt, "dm-archived-toggle").as_deref(),
        Some("Archived conversations (1), hidden")
    );
    press(&mut rt, "dm-archived-toggle");
    assert_eq!(
        label(&rt, "dm-archived-toggle").as_deref(),
        Some("Archived conversations (1), shown")
    );
    assert_eq!(
        label(&rt, &format!("dm-unarchive-{ROOM}-{}", peer_key())).as_deref(),
        Some("Un-archive conversation with Priya")
    );
}

// Two members with one name ------------------------------------------------------------------

#[test]
fn the_thread_with_one_member_is_archived_and_not_the_one_with_their_namesake() {
    let (mut rt, f) = node_app();
    // A snapshot that archives a different member's thread in the same room: ours stays.
    let mut snapshot = f.snapshot(true);
    snapshot.archived = vec![DmArchived {
        room: ROOM.to_owned(),
        peer_key: "999".to_owned(),
        hidden_at_ts: u64::MAX,
    }];
    give(&mut rt, snapshot);
    assert!(on_rail(&rt));
    // Archived by the full id: the key is the decimal id, not the label.
    assert_eq!(peer_key(), crate::rooms::dm::peer_key(id(PEER)));
}

#[test]
fn nothing_archived_means_no_fold_out_and_no_section_without_threads() {
    let mut rt = app();
    give(&mut rt, DmSnapshot::default());
    assert!(rt.find("dm-heading").is_none());
    assert!(rt.find("dm-archived-toggle").is_none());
}

#[test]
fn space_presses_a_focused_archive_control_as_enter_does() {
    let (mut rt, _f) = node_app();
    tab_to(&mut rt, &archive_key());
    space(&mut rt);
    assert!(!on_rail(&rt), "Space archived it");
    assert!(rt.find("dm-thread").is_none());
    tab_to(&mut rt, "dm-archived-toggle");
    space(&mut rt);
    let un = format!("dm-unarchive-{ROOM}-{}", peer_key());
    tab_to(&mut rt, &un);
    space(&mut rt);
    assert!(on_rail(&rt), "Space un-archived it");
}

#[test]
fn the_row_button_is_dimmed_with_the_reason_while_river_has_moved_on() {
    let (mut rt, _f) = node_app();
    rt.update(|s, _| s.chat.issues.update_required = true);
    rt.frame(0.1);
    let button = rt
        .elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some(archive_key().as_str()))
        .expect("the row's button");
    assert!(button.opacity < 0.9);
    assert!(
        button
            .help
            .as_deref()
            .is_some_and(|h| h.contains("newer version of River"))
    );
}

#[test]
fn a_message_sent_into_an_archived_thread_brings_it_back_at_once() {
    // The example's rooms, whose members the composer may write to.
    let mut rt = app();
    rt.update(|s, _| s.chat.source = Source::Node(LinkStatus::Live));
    give(
        &mut rt,
        DmSnapshot {
            room_keys: super::example::dm_room_keys(),
            hydrated: true,
            ..Default::default()
        },
    );
    let mut snapshot = super::example::dm_snapshot();
    let priya = snapshot
        .threads
        .iter()
        .find(|t| t.peer_key == super::example::DM_PEER)
        .expect("the example has Priya");
    snapshot.archived = vec![DmArchived {
        room: priya.room.clone(),
        peer_key: priya.peer_key.clone(),
        hidden_at_ts: priya.last_inbound_ts,
    }];
    give(&mut rt, snapshot);
    let row = format!("dm-town-square-{}", super::example::DM_PEER);
    assert!(rt.find(&row).is_none(), "archived");
    rt.update(|s, _| assert!(s.chat.open_dm("town-square", super::example::DM_PEER)));
    rt.frame(0.2);
    // The user writes, and the background work answers that the message was queued.
    rt.update(|s, _| {
        s.chat.dm_write("hello again");
        assert!(s.chat.dm_send_at(Instant::now()));
    });
    let ticket = rt
        .state
        .chat
        .dm_waiting_ticket()
        .expect("an answer is awaited");
    rt.update(move |s, _| {
        s.chat
            .dm_sent("town-square", super::example::DM_PEER, ticket, Ok(()));
    });
    rt.frame(0.2);
    rt.frame(0.6);
    assert!(rt.state.chat.dm_archived_count() == 0, "no longer archived");
    assert!(rt.find(&row).is_some(), "the thread is back on the rail");
}

#[test]
fn space_presses_the_undo_line_and_the_headers_action_too() {
    let (mut rt, _f) = node_app();
    // The header's action, a toolkit button.
    let key = peer_key();
    rt.update(move |s, _| assert!(s.chat.open_dm(ROOM, &key)));
    rt.frame(0.2);
    // The thread's box has the keyboard and keeps Tab (it is a text field): let it go, as a click
    // elsewhere does, and Tab reaches the header's controls.
    rt.update(|s, _| s.chat.dm.compose_edit().focused = false);
    rt.frame(0.1);
    tab_to(&mut rt, "dm-archive");
    space(&mut rt);
    assert!(!on_rail(&rt), "Space archived it from the header");
    // The Undo line.
    tab_to(&mut rt, "dm-undo");
    space(&mut rt);
    assert!(on_rail(&rt), "Space pressed Undo");
}
