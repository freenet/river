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
    assert!(!s.tray_toggle_dnd(), "no need to show the window");
    assert!(s.chat.dnd.settings.manual);
    assert!(s.tray_menu().dnd);
    s.tray_toggle_dnd();
    assert!(!s.chat.dnd.settings.manual);
    assert!(!s.tray_menu().dnd);
}

#[test]
fn when_only_the_schedule_has_it_on_the_item_opens_settings_instead() {
    let mut s = app_with(rooms(1, 0));
    s.chat.dnd.pinned = true;
    s.chat.dnd.now = 23 * 60;
    s.chat.dnd.settings.schedule = Some(chat::DndSchedule {
        start: 22 * 60,
        end: 7 * 60,
    });
    assert!(s.tray_menu().dnd);
    assert!(s.tray_toggle_dnd(), "the caller shows the window");
    assert!(!s.chat.dnd.settings.manual, "the switch is not touched");
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
    assert_eq!(rec.menus.borrow().len(), 1, "an unchanged menu is not resent");
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
fn a_first_run_is_ticked_and_completing_setup_turns_the_login_item_on() {
    let host = Host::new(true);
    let mut s = RiverApp::detached().with_background(host.clone());
    assert_eq!(s.chat.background.as_ref().unwrap().first_run_login, Some(true));
    assert!(host.calls().is_empty(), "nothing is written before setup ends");
    connected(&mut s);
    assert_eq!(host.calls(), ["login true", "done"]);
    assert_eq!(s.chat.background.as_ref().unwrap().first_run_login, None);
    assert_eq!(s.tray_menu().start_at_login, Some(true));
    // Reaching the node again changes nothing.
    connected(&mut s);
    assert_eq!(host.calls(), ["login true", "done"]);
}

#[test]
fn unticking_on_the_setup_screen_writes_nothing() {
    let host = Host::new(true);
    let mut s = RiverApp::detached().with_background(host.clone());
    s.chat.toggle_first_run_login();
    assert_eq!(s.chat.background.as_ref().unwrap().first_run_login, Some(false));
    connected(&mut s);
    assert_eq!(host.calls(), ["done"], "no login item, and not offered again");
    assert_eq!(s.tray_menu().start_at_login, Some(false));
}

#[test]
fn a_run_that_is_not_the_first_never_writes_the_login_item() {
    let host = Host::new(false);
    let mut s = RiverApp::detached().with_background(host.clone());
    assert_eq!(s.chat.background.as_ref().unwrap().first_run_login, None);
    s.chat.toggle_first_run_login();
    connected(&mut s);
    assert!(host.calls().is_empty(), "{:?}", host.calls());
}
