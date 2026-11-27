//! What the tray menu is told ([`RiverApp::tray_menu`]), what its items do, and the first run's
//! "Start River when I log in".

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use crate::chat::model::Room;
use crate::chat::{self, LinkStatus};
use crate::notify::{Notification, Notifier, TRAY_UNREAD_ROOMS, TrayMenu};
use crate::rooms::modes::NotificationMode;
use crate::{RiverApp, Setup, chat::example};

#[derive(Clone, Default)]
struct Recorder {
    menus: Rc<RefCell<Vec<TrayMenu>>>,
}

impl Notifier for Recorder {
    fn notify(&self, _: Notification) -> bool {
        true
    }
    fn set_unread(&self, _: usize) {}
    fn set_menu(&self, menu: TrayMenu) {
        self.menus.borrow_mut().push(menu);
    }
}

/// `n` rooms, "r0".."r{n-1}", each with the example's messages, the newest message of `r{i}`
/// at time `i`, and `unread` messages unread in each.
fn rooms(n: usize, unread: usize) -> Vec<Room> {
    let base = example::data().rooms.remove(0);
    (0..n)
        .map(|i| {
            let mut r = base.clone();
            r.key = format!("r{i}");
            r.name = format!("Room {i}");
            r.unread = unread;
            for m in &mut r.messages {
                m.time = i as u32;
            }
            r
        })
        .collect()
}

fn app_with(rooms: Vec<Room>) -> RiverApp {
    let mut s = RiverApp::detached();
    // Rooms with no choice of their own count mentions and replies only: make every one "All".
    s.chat.notify_all_in(&rooms);
    s.chat.show_node_rooms(rooms, None, LinkStatus::Live, 1000);
    s
}

fn names(m: &TrayMenu) -> Vec<(&str, usize)> {
    m.unread_rooms
        .iter()
        .map(|r| (r.key.as_str(), r.count))
        .collect()
}

#[test]
fn nothing_unread_lists_no_rooms() {
    let s = app_with(rooms(3, 0));
    assert!(s.tray_menu().unread_rooms.is_empty());
}

#[test]
fn at_most_five_rooms_newest_message_first() {
    let mut rs = rooms(8, 2);
    rs[3].unread = 7;
    let s = app_with(rs);
    let m = s.tray_menu();
    assert_eq!(TRAY_UNREAD_ROOMS, 5);
    assert_eq!(
        names(&m),
        [("r7", 2), ("r6", 2), ("r5", 2), ("r4", 2), ("r3", 7)],
        "capped at five, newest message first, each with its own count"
    );
}

#[test]
fn a_room_that_does_not_count_is_not_listed() {
    let mut s = app_with(rooms(4, 2));
    s.chat.set_mode("r3", NotificationMode::Muted);
    // Mentions only: unread messages with no mention for me count for nothing.
    s.chat.set_mode("r2", NotificationMode::MentionsAndReplies);
    let m = s.tray_menu();
    assert_eq!(
        s.chat.unread_total(),
        m.unread_rooms.iter().map(|r| r.count).sum::<usize>(),
        "the list is exactly what the total counts"
    );
    assert!(
        !m.unread_rooms.iter().any(|r| r.key == "r3"),
        "a muted room is not listed"
    );
    for r in &m.unread_rooms {
        let room = s.chat.rooms().iter().find(|x| x.key == r.key).unwrap();
        assert_eq!(r.count, s.chat.unread_of(room));
    }
    assert!(
        !m.unread_rooms.iter().any(|r| r.key == "r2"),
        "a mentions-only room with no mention is not listed"
    );
}

#[test]
fn a_chosen_room_opens() {
    let mut s = app_with(rooms(3, 1));
    s.open_room("r2");
    assert_eq!(s.chat.room().unwrap().key, "r2");
}

#[test]
fn mark_all_read_clears_the_total_and_the_list() {
    let mut s = app_with(rooms(3, 4));
    assert!(s.unread() > 0 && !s.tray_menu().unread_rooms.is_empty());
    s.tray_mark_all_read();
    assert_eq!(s.unread(), 0);
    assert!(s.tray_menu().unread_rooms.is_empty());
}

#[test]
fn mark_all_read_leaves_a_muted_room_alone() {
    let mut s = app_with(rooms(2, 4));
    s.chat.set_mode("r1", NotificationMode::Muted);
    s.tray_mark_all_read();
    let r1 = s.chat.rooms().iter().find(|r| r.key == "r1").unwrap();
    assert_eq!(r1.unread, 4, "the same rooms the overview's button marks");
}

#[test]
fn the_menu_item_flips_the_same_switch_as_settings() {
    let mut s = app_with(rooms(1, 0));
    s.chat.dnd.pinned = true;
    s.chat.dnd.now = 12 * 60;
    assert!(!s.tray_menu().dnd);
    s.tray_toggle_dnd();
    assert!(s.chat.dnd.settings.manual);
    assert!(s.tray_menu().dnd && s.tray_menu().dnd_switch);
    s.tray_toggle_dnd();
    assert!(!s.chat.dnd.settings.manual);
    assert!(!s.tray_menu().dnd);
}

#[test]
fn when_only_the_schedule_has_it_on_the_menu_says_so_and_the_switch_is_off() {
    let mut s = app_with(rooms(1, 0));
    s.chat.dnd.pinned = true;
    s.chat.dnd.now = 23 * 60;
    s.chat.dnd.settings.schedule = Some(chat::DndSchedule {
        start: 22 * 60,
        end: 7 * 60,
    });
    let m = s.tray_menu();
    assert!(m.dnd && !m.dnd_switch, "on by the schedule alone");
    s.tray_toggle_dnd();
    assert!(s.tray_menu().dnd_switch, "a manual switch on top of it");
}

#[test]
fn the_update_item_follows_a_ready_update() {
    let mut s = app_with(rooms(1, 0));
    assert!(!s.tray_menu().update_ready);
    s.apply_update(crate::update::UpdateEvent::Status(
        crate::update::UpdateStatus::Ready {
            version: "9.9.9".into(),
        },
    ));
    assert!(s.tray_menu().update_ready);
}

#[test]
fn the_notifier_hears_the_menu_only_when_it_changes() {
    let rec = Recorder::default();
    let mut s = app_with(rooms(2, 1)).with_notifier(Box::new(rec.clone()));
    s.sync_tray_menu();
    s.sync_tray_menu();
    assert_eq!(
        rec.menus.borrow().len(),
        1,
        "an unchanged menu is not resent"
    );
    s.tray_mark_all_read();
    s.sync_tray_menu();
    s.sync_tray_menu();
    assert_eq!(rec.menus.borrow().len(), 2);
    assert!(rec.menus.borrow()[1].unread_rooms.is_empty());
}

/// A shell that records what it is asked to do about the login item.
struct Host {
    first: Mutex<bool>,
    login: Mutex<bool>,
    calls: Mutex<Vec<String>>,
}

impl Host {
    fn new(first: bool) -> Arc<Self> {
        Arc::new(Host {
            first: Mutex::new(first),
            login: Mutex::new(false),
            calls: Mutex::default(),
        })
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl chat::BackgroundHost for Host {
    fn tray_available(&self) -> bool {
        true
    }
    fn close_to_tray(&self) -> bool {
        true
    }
    fn set_close_to_tray(&self, _: bool) -> Result<(), String> {
        Ok(())
    }
    fn start_at_login(&self) -> bool {
        *self.login.lock().unwrap()
    }
    fn set_start_at_login(&self, on: bool) -> Result<(), String> {
        self.calls.lock().unwrap().push(format!("login {on}"));
        *self.login.lock().unwrap() = on;
        Ok(())
    }
    fn first_run(&self) -> bool {
        *self.first.lock().unwrap()
    }
    fn first_run_done(&self) {
        self.calls.lock().unwrap().push("done".into());
        *self.first.lock().unwrap() = false;
    }
}

fn connected(s: &mut RiverApp) {
    s.set_setup(Setup::Connected {
        version: "0.2.156".into(),
    });
}

#[test]
fn a_first_run_has_a_ticked_card_and_reaching_the_node_writes_nothing() {
    let host = Host::new(true);
    let mut s = RiverApp::detached().with_background(host.clone());
    assert_eq!(
        s.chat.background.as_ref().unwrap().first_run_login,
        Some(true)
    );
    connected(&mut s);
    connected(&mut s);
    assert!(host.calls().is_empty(), "{:?}", host.calls());
    assert_eq!(
        s.chat.background.as_ref().unwrap().first_run_login,
        Some(true),
        "still waiting for OK"
    );
}

#[test]
fn ok_with_the_box_ticked_writes_the_login_item_once_and_retires_the_card() {
    let host = Host::new(true);
    let mut s = RiverApp::detached().with_background(host.clone());
    connected(&mut s);
    s.chat.confirm_first_run();
    assert_eq!(host.calls(), ["login true", "done"]);
    assert_eq!(s.chat.background.as_ref().unwrap().first_run_login, None);
    assert_eq!(s.tray_menu().start_at_login, Some(true));
    s.chat.confirm_first_run();
    assert_eq!(host.calls(), ["login true", "done"]);
}

#[test]
fn ok_with_the_box_unticked_writes_nothing_and_retires_the_card() {
    let host = Host::new(true);
    let mut s = RiverApp::detached().with_background(host.clone());
    s.chat.toggle_first_run_login();
    s.chat.confirm_first_run();
    assert_eq!(host.calls(), ["done"]);
    assert_eq!(s.chat.background.as_ref().unwrap().first_run_login, None);
    assert_eq!(s.tray_menu().start_at_login, Some(false));
}

#[test]
fn a_run_that_is_not_the_first_has_no_card_and_never_writes_the_login_item() {
    let host = Host::new(false);
    let mut s = RiverApp::detached().with_background(host.clone());
    assert_eq!(s.chat.background.as_ref().unwrap().first_run_login, None);
    s.chat.toggle_first_run_login();
    s.chat.confirm_first_run();
    connected(&mut s);
    assert!(host.calls().is_empty(), "{:?}", host.calls());
}

#[test]
fn choosing_the_tray_or_settings_toggle_while_the_card_waits_is_the_answer() {
    let host = Host::new(true);
    let mut s = RiverApp::detached().with_background(host.clone());
    s.tray_toggle_start_at_login();
    assert_eq!(host.calls(), ["login true", "done"]);
    assert_eq!(s.chat.background.as_ref().unwrap().first_run_login, None);
    // The card does not write it again.
    s.chat.confirm_first_run();
    assert_eq!(host.calls(), ["login true", "done"]);
}

#[test]
fn a_failed_login_change_from_the_tray_is_said_as_a_notification() {
    #[derive(Clone, Default)]
    struct Said(Rc<RefCell<Vec<String>>>);
    impl Notifier for Said {
        fn notify(&self, n: Notification) -> bool {
            self.0.borrow_mut().push(n.body);
            true
        }
        fn set_unread(&self, _: usize) {}
    }
    struct Refusing;
    impl chat::BackgroundHost for Refusing {
        fn tray_available(&self) -> bool {
            true
        }
        fn close_to_tray(&self) -> bool {
            true
        }
        fn set_close_to_tray(&self, _: bool) -> Result<(), String> {
            Ok(())
        }
        fn start_at_login(&self) -> bool {
            false
        }
        fn set_start_at_login(&self, _: bool) -> Result<(), String> {
            Err("no permission".into())
        }
    }
    let said = Said::default();
    let mut s = RiverApp::detached()
        .with_notifier(Box::new(said.clone()))
        .with_background(Arc::new(Refusing));
    s.tray_toggle_start_at_login();
    assert_eq!(
        *said.0.borrow(),
        ["Couldn't change Start at login: no permission"]
    );
    assert_eq!(
        s.tray_menu().start_at_login,
        Some(false),
        "the tick is the truth"
    );
}

#[test]
fn a_chosen_room_opens_through_the_notification_click_path_and_closes_the_overview() {
    let mut s = app_with(rooms(3, 1));
    s.chat.toggle_overview();
    assert!(s.chat.overview);
    assert!(s.open_message("r2", ""));
    assert_eq!(s.chat.room().unwrap().key, "r2");
    assert!(!s.chat.overview);
    assert!(
        !s.open_message("gone", ""),
        "a room that has gone opens nothing"
    );
}

fn reported_reads(s: &mut RiverApp) -> Vec<(String, String)> {
    std::mem::take(&mut s.chat.reported)
        .into_iter()
        .filter_map(|c| match c.action {
            crate::rooms::outbox::Action::MarkRead { up_to } => Some((c.room, up_to)),
            _ => None,
        })
        .collect()
}

#[test]
fn mark_all_read_tells_the_node_for_every_room_it_marks() {
    // r0 is the open room, read already; r2 is muted.
    let mut s = app_with(rooms(4, 4));
    s.chat.reported.clear();
    s.chat.set_mode("r2", NotificationMode::Muted);
    s.tray_mark_all_read();
    let mut got = reported_reads(&mut s);
    got.sort();
    let newest = |k: &str| {
        s.chat
            .rooms()
            .iter()
            .find(|r| r.key == k)
            .unwrap()
            .messages
            .last()
            .unwrap()
            .key
            .clone()
    };
    assert_eq!(
        got,
        [
            ("r1".to_owned(), newest("r1")),
            ("r3".to_owned(), newest("r3"))
        ],
        "each marked room, up to its newest message; the muted one is left alone"
    );
    // Marking again reports nothing new.
    s.tray_mark_all_read();
    assert!(reported_reads(&mut s).is_empty());
}

#[test]
fn the_open_rooms_missed_messages_are_on_its_row() {
    let mut s = app_with(rooms(2, 0));
    s.missed = ("r1".into(), 3);
    let m = s.tray_menu();
    assert_eq!(names(&m), [("r1", 3)]);
    assert_eq!(
        m.unread_rooms.iter().map(|r| r.count).sum::<usize>(),
        s.unread()
    );
    s.tray_mark_all_read();
    assert_eq!(s.unread(), 0);
}
