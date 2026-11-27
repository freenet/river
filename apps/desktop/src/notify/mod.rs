//! OS notifications for the desktop app.
//!
//! River's UI decides what is worth a notification (`river_app::notify`); this shows it with
//! the operating system and reports a click back. Everything that can block (the D-Bus call, a
//! macOS permission prompt, waiting for a click) runs on threads of its own, never the UI
//! thread, and every failure is logged and dropped: a notification that cannot be shown must
//! never take the app down.
//!
//! Per platform: Linux speaks the freedesktop notification protocol over D-Bus (`xdg`),
//! Windows shows toasts (`toast`), macOS uses `UNUserNotificationCenter` and only inside the
//! signed `.app` bundle (`macos`). The unread count is shown in the window title by the UI on
//! every platform, and as a badge (`Badge`): on the Dock icon on macOS and over the taskbar
//! button on Windows. Linux has no badge: rune's call does nothing there.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use notify_rust::NotificationResponse;
use river_app::RiverApp;
use river_app::notify::{Notification, Notifier};
use rune::AppHandle;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod toast;
#[cfg(all(unix, not(target_os = "macos")))]
mod xdg;

#[cfg(target_os = "macos")]
use macos::Backend;
#[cfg(windows)]
use toast::Backend;
#[cfg(all(unix, not(target_os = "macos")))]
use xdg::Backend;

#[cfg(windows)]
pub use toast::register_identity;

/// How many notifications may be waiting for a click at once, each on a thread of its own. A
/// server that never reports a notification closing would otherwise grow this without end.
const MAX_WAITING: usize = 32;

/// How many notifications may queue for the worker. The UI never waits for it: past this, new
/// ones are dropped (the unread count in the title still shows them).
const QUEUE: usize = 16;

/// How many waiters one notification id may have. A notification that replaces another keeps
/// its id, and each shows its own handle (its own D-Bus connection) so the newest is always
/// alive to hear the click; the older ones end when the server reports the id closed.
const PER_ID: usize = 4;

/// The waiters of one notification id in one room, and the message its notification shows
/// now. A notification that replaces another keeps the id, so every waiter on it hears one
/// click: they must all act on the CURRENT message, and only the newest waiter acts.
#[derive(Default)]
struct Entry {
    live: Vec<u64>,
    message: String,
    /// Whether a waiter already acted on the notification as it stands now. Set by the first
    /// click handled, reset when a newer notification replaces it, so one click acts once.
    acted: bool,
}

type Active = Arc<Mutex<HashMap<(String, u64), Entry>>>;

/// The notifications waiting for a click, by room and the platform's id for it.
#[derive(Clone, Default)]
pub(super) struct Waiters {
    active: Active,
    capped: Arc<AtomicBool>,
    seq: Arc<std::sync::atomic::AtomicU64>,
}

/// A place among the waiters, given back when dropped: on return, panic or a thread that
/// never started.
struct Slot {
    active: Active,
    capped: Arc<AtomicBool>,
    key: (String, u64),
    seq: u64,
}

impl Slot {
    /// The message to act on, if this is the newest live waiter of its notification and no
    /// waiter has acted on it yet; else `None`. A click reaches every waiter of the id: the
    /// first to claim acts, the others stand down. If the newest waiter ended without acting
    /// (its connection died), the next newest is the newest left and acts.
    ///
    /// A waiter registered but not yet listening can miss a click for a few milliseconds after
    /// its notification is shown; the older waiters on the same id still hear it, and it cannot
    /// be closed with this API.
    fn claim(&self) -> Option<String> {
        let mut map = self.active.lock().ok()?;
        let entry = map.get_mut(&self.key)?;
        if entry.acted || entry.live.iter().max() != Some(&self.seq) {
            return None;
        }
        entry.acted = true;
        Some(entry.message.clone())
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        if let Ok(mut map) = self.active.lock()
            && let Some(entry) = map.get_mut(&self.key)
        {
            entry.live.retain(|s| *s != self.seq);
            if entry.live.is_empty() {
                map.remove(&self.key);
            }
        }
        // Room again: a later crowd of waiters is a new episode and is said again.
        if let Ok(map) = self.active.lock()
            && map.values().map(|e| e.live.len()).sum::<usize>() < MAX_WAITING
        {
            self.capped.store(false, Ordering::Relaxed);
        }
    }
}

impl Waiters {
    /// Note that the notification `id` of `room` now shows `message`, and give it a place
    /// among the waiters. `None` when it is full (the waiters already there hear the click and
    /// read the new message).
    fn register(&self, room: &str, message: &str, id: u64) -> Option<Slot> {
        let key = (room.to_owned(), id);
        let mut map = self.active.lock().ok()?;
        if let Some(entry) = map.get_mut(&key) {
            entry.message = message.to_owned();
            entry.acted = false;
            if entry.live.len() >= PER_ID {
                // Enough already wait on this very notification; they hear the click.
                return None;
            }
        }
        if map.values().map(|e| e.live.len()).sum::<usize>() >= MAX_WAITING {
            // Said once: a click on later notifications will not open their room until some
            // of these are dismissed.
            if !self.capped.swap(true, Ordering::Relaxed) {
                river_app::diag!(
                    "river: too many notifications waiting; clicks on new ones will not open their room until old ones are dismissed"
                );
            }
            return None;
        }
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let entry = map.entry(key.clone()).or_default();
        entry.message = message.to_owned();
        entry.acted = false;
        entry.live.push(seq);
        drop(map);
        Some(Slot {
            active: self.active.clone(),
            capped: self.capped.clone(),
            key,
            seq,
        })
    }

    /// Whether other waiters than the caller's are live on this notification id: the caller's
    /// notification was replaced, so its wait ending is expected.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(super) fn replaced(&self, room: &str, id: u64) -> bool {
        self.active
            .lock()
            .ok()
            .and_then(|m| m.get(&(room.to_owned(), id)).map(|e| e.live.len() > 1))
            .unwrap_or(false)
    }

    /// Wait on a thread of its own for `wait` to report a click, then act on it. `wait` gets
    /// the handler to give the platform. `id` is the platform's id for the notification: the
    /// same for notifications that replace each other, never reused for ones that do not.
    pub(super) fn spawn(
        &self,
        room: String,
        message: String,
        id: u64,
        handle: AppHandle<RiverApp>,
        wait: impl FnOnce(Box<dyn FnOnce(&NotificationResponse)>) + Send + 'static,
    ) {
        let Some(slot) = self.register(&room, &message, id) else {
            return;
        };
        let spawned = std::thread::Builder::new()
            .name("river-notify-click".into())
            .spawn(move || {
                let held = Arc::new(slot);
                let for_click = held.clone();
                let act = move |response: &NotificationResponse| {
                    let intent = intent_of(response);
                    // A dismissal is not an action: it must not use up the click.
                    if intent != Intent::Nothing
                        && let Some(message) = for_click.claim()
                    {
                        perform(&handle, room, message, intent);
                    }
                };
                // The place is given back when both are gone: after the wait ends.
                wait(Box::new(act));
                drop(held);
            });
        if let Err(e) = spawned {
            // The closure, and the slot in it, are dropped: the slot is free again.
            river_app::diag!("river: clicking a notification will not open its room: {e}");
        }
    }
}

/// Shows notifications through a worker thread.
pub struct DesktopNotifier {
    jobs: mpsc::SyncSender<Notification>,
    dropped: AtomicBool,
    badge: Badge,
}

/// The unread count on the Dock icon (macOS) or the taskbar button (Windows), the same number
/// the window title shows. Both sides run on the UI thread: the app reports a change from its
/// frame, and rune calls `attach` when the native window is created.
///
/// A start in the tray has no window, and rune can show a badge only on one. The count is kept
/// and put on the window when it is created (and again each time it is recreated), so it is
/// right as soon as the window exists. Until then there is no badge.
///
/// The tray icon gets the same number from the same call ([`Badge::set`]), and the connection
/// state for its tooltip ([`Badge::set_connection`]).
#[derive(Clone, Default)]
pub struct Badge(Arc<Mutex<BadgeState>>, crate::tray::Tracker);

#[derive(Default)]
struct BadgeState {
    total: usize,
    window: Option<rune::WindowHandle>,
}

/// The count rune is given: it caps what a taskbar can hold, never wraps.
fn badge_count(total: usize) -> u32 {
    u32::try_from(total).unwrap_or(u32::MAX)
}

/// What a screen reader says for the badge.
fn badge_description(total: usize) -> String {
    match total {
        0 => String::new(),
        1 => "1 unread message".to_owned(),
        n => format!("{n} unread messages"),
    }
}

fn taskbar_for(total: usize) -> rune::Taskbar {
    rune::Taskbar::new().badge(badge_count(total), badge_description(total))
}

impl Badge {
    /// The window rune made (or made again): give it the current count.
    pub fn attach(&self, window: &rune::WindowHandle) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.window = Some(window.clone());
        window.set_taskbar(taskbar_for(state.total));
    }

    /// The connection to the node changed: the tray's tooltip says so.
    fn set_connection(&self, connection: river_app::chat::node_status::Connection) {
        self.1.set_connection(connection);
    }

    /// The count changed.
    fn set(&self, total: usize) {
        self.1.set_unread(total);
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.total = total;
        if let Some(window) = &state.window {
            window.set_taskbar(taskbar_for(total));
        }
    }
}

impl DesktopNotifier {
    pub fn new(handle: AppHandle<RiverApp>, badge: Badge) -> Self {
        let (jobs, rx) = mpsc::sync_channel(QUEUE);
        let spawned = std::thread::Builder::new()
            .name("river-notify".into())
            .spawn(move || worker(rx, handle));
        if let Err(e) = spawned {
            // The sender then has no receiver and every notification is dropped.
            river_app::diag!("river: notifications are off, could not start their thread: {e}");
        }
        DesktopNotifier {
            jobs,
            dropped: AtomicBool::new(false),
            badge,
        }
    }
}

impl Notifier for DesktopNotifier {
    fn notify(&self, notification: Notification) -> bool {
        // Never waits. Dropped if the worker is gone (it said why when it went) or far behind.
        match self.jobs.try_send(notification) {
            Ok(()) => true,
            Err(e) => {
                if matches!(e, mpsc::TrySendError::Full(_))
                    && !self.dropped.swap(true, Ordering::Relaxed)
                {
                    river_app::diag!("river: notifications are backed up; dropping some");
                }
                false
            }
        }
    }

    /// Called on the UI thread, which is where rune wants `set_taskbar`.
    fn set_unread(&self, total: usize) {
        self.badge.set(total);
    }

    fn set_connection(&self, connection: river_app::chat::node_status::Connection) {
        self.badge.set_connection(connection);
    }
}

fn worker(rx: mpsc::Receiver<Notification>, handle: AppHandle<RiverApp>) {
    let mut backend = Backend::new();
    for notification in rx {
        let shown = catch_unwind(AssertUnwindSafe(|| backend.show(&notification, &handle)));
        if shown.is_err() {
            river_app::diag!("river: showing a notification panicked; carrying on");
        }
    }
}

/// The id of the Reply button, where the system shows buttons and has no reply field.
#[cfg(any(test, not(target_os = "macos")))]
const ACTION_REPLY: &str = "reply";
/// The id of the Mark as read button.
const ACTION_MARK_READ: &str = "mark-read";

/// What a response to a notification asks River to do.
#[derive(Debug, PartialEq, Eq)]
enum Intent {
    /// The notification itself was clicked: show the window at the message.
    Open,
    /// The Reply button, on a system with no reply field: open the room with the composer
    /// ready. (macOS has the field, so never builds this.)
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    OpenToReply,
    /// Mark as read: nothing is shown.
    MarkRead,
    /// Text typed into the notification's own reply field.
    Send(String),
    /// Dismissed, expired, or a button this version does not know.
    Nothing,
}

/// What `response` asks for. On Linux the click arrives as the action named `default`.
fn intent_of(response: &NotificationResponse) -> Intent {
    match response {
        NotificationResponse::Default => Intent::Open,
        NotificationResponse::Action(name) => match name.as_str() {
            "default" => Intent::Open,
            #[cfg(any(test, not(target_os = "macos")))]
            ACTION_REPLY => Intent::OpenToReply,
            ACTION_MARK_READ => Intent::MarkRead,
            _ => Intent::Nothing,
        },
        NotificationResponse::Reply(text) => Intent::Send(text.clone()),
        NotificationResponse::Closed(_) => Intent::Nothing,
    }
}

/// Do what a notification about `room` (showing `message`, which may be empty) was answered
/// with. Runs on the notification's waiting thread; the app's state is only touched through
/// the handle, on the UI thread.
fn perform(handle: &AppHandle<RiverApp>, room: String, message: String, intent: Intent) {
    match intent {
        Intent::Open => {
            // `open` also restores a minimised window and shows one hidden in the tray.
            handle.update(move |s, _| {
                s.open_message(&room, &message);
            });
            handle.open();
        }
        Intent::OpenToReply => {
            handle.update(move |s, _| s.open_room_to_reply(&room));
            handle.open();
        }
        Intent::MarkRead => handle.update(move |s, _| s.mark_read_through(&room, &message)),
        Intent::Send(text) => {
            // Not sent: the app keeps the text as that room's draft and says why in the room.
            // The window and the open room are left alone.
            handle.update(move |s, _| {
                if let Err(refusal) = s.reply_from_notification(&room, &text) {
                    river_app::diag!(
                        "river: a reply from a notification was not sent: {refusal:?}"
                    );
                }
            });
        }
        Intent::Nothing => {}
    }
}

/// Escape `text` for a notification body that the server reads as markup.
#[cfg(any(test, all(unix, not(target_os = "macos"))))]
fn escape_markup(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify_rust::CloseReason;

    #[test]
    fn the_badge_says_the_count_and_zero_clears_it() {
        assert_eq!(taskbar_for(0).count(), 0);
        assert_eq!(badge_description(0), "");
        assert_eq!(taskbar_for(1).count(), 1);
        assert_eq!(badge_description(1), "1 unread message");
        assert_eq!(taskbar_for(3).count(), 3);
        assert_eq!(badge_description(3), "3 unread messages");
        assert_eq!(taskbar_for(1_000).count(), 1_000);
    }

    #[test]
    fn set_unread_reaches_the_tray_and_an_unchanged_count_is_not_passed_on() {
        use river_app::chat::node_status::Connection;
        let n = DesktopNotifier {
            jobs: mpsc::sync_channel(1).0,
            dropped: AtomicBool::new(false),
            badge: Badge::default(),
        };
        n.set_unread(3);
        assert_eq!(n.badge.1.view().unread, 3);
        n.set_connection(Connection::Unreachable);
        assert_eq!(n.badge.1.view().connection, Some(Connection::Unreachable));
        n.set_unread(0);
        assert_eq!(n.badge.1.view().unread, 0);
    }

    #[test]
    fn an_absurd_count_saturates_instead_of_wrapping() {
        assert_eq!(badge_count(usize::MAX), u32::MAX);
    }

    #[test]
    fn a_count_set_before_any_window_exists_is_kept_for_the_window() {
        let badge = Badge::default();
        badge.set(4);
        assert_eq!(badge.0.lock().unwrap().total, 4);
        assert!(badge.0.lock().unwrap().window.is_none());
    }

    #[test]
    fn each_response_asks_for_one_thing() {
        assert_eq!(intent_of(&NotificationResponse::Default), Intent::Open);
        assert_eq!(
            intent_of(&NotificationResponse::Action("default".into())),
            Intent::Open
        );
        assert_eq!(
            intent_of(&NotificationResponse::Action(ACTION_REPLY.into())),
            Intent::OpenToReply
        );
        assert_eq!(
            intent_of(&NotificationResponse::Action(ACTION_MARK_READ.into())),
            Intent::MarkRead
        );
        assert_eq!(
            intent_of(&NotificationResponse::Reply("hi".into())),
            Intent::Send("hi".into())
        );
        assert_eq!(
            intent_of(&NotificationResponse::Action("other".into())),
            Intent::Nothing
        );
        assert_eq!(
            intent_of(&NotificationResponse::Closed(CloseReason::Dismissed)),
            Intent::Nothing
        );
    }

    #[test]
    fn a_replaced_notification_acts_on_its_newest_message_once() {
        let w = Waiters::default();
        let first = w.register("room", "old", 7).unwrap();
        let second = w.register("room", "new", 7).unwrap();
        // One click on the id reaches both waiters: the newest acts with the newest message,
        // and the older one, hearing the same click later, stands down.
        assert_eq!(first.claim(), None);
        assert_eq!(second.claim().as_deref(), Some("new"));
        assert_eq!(first.claim(), None, "already acted on");
        // A newer notification on the id is a new thing to act on.
        let third = w.register("room", "newest", 7).unwrap();
        assert_eq!(third.claim().as_deref(), Some("newest"));
    }

    #[test]
    fn when_the_newest_waiter_ends_without_acting_the_next_one_acts() {
        let w = Waiters::default();
        let first = w.register("room", "old", 7).unwrap();
        let second = w.register("room", "new", 7).unwrap();
        drop(second);
        assert_eq!(first.claim().as_deref(), Some("new"));
    }

    #[test]
    fn a_crowd_of_waiters_is_said_again_after_it_clears() {
        let w = Waiters::default();
        let slots: Vec<Slot> = (0..MAX_WAITING as u64)
            .map(|i| w.register("room", "m", i).unwrap())
            .collect();
        assert!(w.register("room", "m", 999).is_none());
        assert!(w.capped.load(Ordering::Relaxed));
        drop(slots);
        assert!(!w.capped.load(Ordering::Relaxed));
    }

    #[test]
    fn a_waiter_turned_away_for_a_full_id_still_updates_the_message() {
        let w = Waiters::default();
        let slots: Vec<Slot> = (0..PER_ID)
            .map(|i| w.register("room", &format!("m{i}"), 1).unwrap())
            .collect();
        assert!(w.register("room", "latest", 1).is_none());
        assert_eq!(slots.last().unwrap().claim().as_deref(), Some("latest"));
    }

    #[test]
    fn separate_notifications_of_one_room_act_on_their_own_message() {
        let w = Waiters::default();
        let a = w.register("room", "a", 1).unwrap();
        let b = w.register("room", "b", 2).unwrap();
        assert_eq!(a.claim().as_deref(), Some("a"));
        assert_eq!(b.claim().as_deref(), Some("b"));
    }

    #[test]
    fn markup_in_a_message_is_shown_as_text() {
        assert_eq!(escape_markup("a <b>&</b>"), "a &lt;b&gt;&amp;&lt;/b&gt;");
    }
}
