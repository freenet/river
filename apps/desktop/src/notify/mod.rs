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
//! every platform; a dock badge or taskbar overlay is not done.

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

type Active = Arc<Mutex<HashMap<(String, u64), usize>>>;

/// The notifications waiting for a click, counted by room and the platform's id for it.
#[derive(Clone, Default)]
pub(super) struct Waiters {
    active: Active,
    capped: Arc<AtomicBool>,
}

/// A place among the waiters, given back when dropped: on return, panic or a thread that
/// never started.
struct Slot {
    active: Active,
    key: (String, u64),
}

impl Drop for Slot {
    fn drop(&mut self) {
        if let Ok(mut map) = self.active.lock()
            && let Some(n) = map.get_mut(&self.key)
        {
            *n -= 1;
            if *n == 0 {
                map.remove(&self.key);
            }
        }
    }
}

impl Waiters {
    /// Wait on a thread of its own for `wait` to report a click, then open `room`. `wait` gets
    /// the handler to give the platform. `id` is the platform's id for the notification; pass
    /// a number that is never reused where notifications are not replaced in place.
    pub(super) fn spawn(
        &self,
        room: String,
        id: u64,
        handle: AppHandle<RiverApp>,
        wait: impl FnOnce(Box<dyn FnOnce(&NotificationResponse)>) + Send + 'static,
    ) {
        let key = (room.clone(), id);
        let mut map = match self.active.lock() {
            Ok(map) => map,
            Err(_) => return,
        };
        if map.get(&key).is_some_and(|n| *n >= PER_ID) {
            // Enough already wait on this very notification; they hear the click.
            return;
        }
        if map.values().sum::<usize>() >= MAX_WAITING {
            // Said once: a click on later notifications will not open their room until some
            // of these are dismissed.
            if !self.capped.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "river: too many notifications waiting; clicks on new ones will not open their room until old ones are dismissed"
                );
            }
            return;
        }
        *map.entry(key.clone()).or_default() += 1;
        drop(map);
        let slot = Slot {
            active: self.active.clone(),
            key,
        };
        let spawned = std::thread::Builder::new()
            .name("river-notify-click".into())
            .spawn(move || {
                let _slot = slot;
                let open = move |response: &NotificationResponse| {
                    if is_click(response) {
                        open_room(&handle, room);
                    }
                };
                wait(Box::new(open));
            });
        if let Err(e) = spawned {
            // The closure, and the slot in it, are dropped: the slot is free again.
            eprintln!("river: clicking a notification will not open its room: {e}");
        }
    }
}

/// Shows notifications through a worker thread.
pub struct DesktopNotifier {
    jobs: mpsc::SyncSender<Notification>,
    dropped: AtomicBool,
}

impl DesktopNotifier {
    pub fn new(handle: AppHandle<RiverApp>) -> Self {
        let (jobs, rx) = mpsc::sync_channel(QUEUE);
        let spawned = std::thread::Builder::new()
            .name("river-notify".into())
            .spawn(move || worker(rx, handle));
        if let Err(e) = spawned {
            // The sender then has no receiver and every notification is dropped.
            eprintln!("river: notifications are off, could not start their thread: {e}");
        }
        DesktopNotifier {
            jobs,
            dropped: AtomicBool::new(false),
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
                    eprintln!("river: notifications are backed up; dropping some");
                }
                false
            }
        }
    }

    /// The count is in the window title. A dock badge or taskbar overlay needs main-thread
    /// platform calls this does not make.
    fn set_unread(&self, _total: usize) {}
}

fn worker(rx: mpsc::Receiver<Notification>, handle: AppHandle<RiverApp>) {
    let mut backend = Backend::new();
    for notification in rx {
        let shown = catch_unwind(AssertUnwindSafe(|| backend.show(&notification, &handle)));
        if shown.is_err() {
            eprintln!("river: showing a notification panicked; carrying on");
        }
    }
}

/// Whether `response` is the person clicking the notification itself (not a button, not a
/// dismissal). On Linux the click arrives as the action named `default`.
fn is_click(response: &NotificationResponse) -> bool {
    match response {
        NotificationResponse::Default => true,
        NotificationResponse::Action(name) => name == "default",
        NotificationResponse::Reply(_) | NotificationResponse::Closed(_) => false,
    }
}

/// Bring River to the front with `room` open.
fn open_room(handle: &AppHandle<RiverApp>, room: String) {
    handle.update(move |s, _| s.open_room(&room));
    handle.open();
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
    fn only_the_notification_itself_is_a_click() {
        assert!(is_click(&NotificationResponse::Default));
        assert!(is_click(&NotificationResponse::Action("default".into())));
        assert!(!is_click(&NotificationResponse::Action("other".into())));
        assert!(!is_click(&NotificationResponse::Closed(
            CloseReason::Dismissed
        )));
    }

    #[test]
    fn markup_in_a_message_is_shown_as_text() {
        assert_eq!(escape_markup("a <b>&</b>"), "a &lt;b&gt;&amp;&lt;/b&gt;");
    }
}
