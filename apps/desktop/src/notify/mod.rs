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

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc;

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

/// Shows notifications through a worker thread.
pub struct DesktopNotifier {
    jobs: mpsc::Sender<Notification>,
}

impl DesktopNotifier {
    pub fn new(handle: AppHandle<RiverApp>) -> Self {
        let (jobs, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("river-notify".into())
            .spawn(move || worker(rx, handle));
        if let Err(e) = spawned {
            // The sender then has no receiver and every notification is dropped.
            eprintln!("river: notifications are off, could not start their thread: {e}");
        }
        DesktopNotifier { jobs }
    }
}

impl Notifier for DesktopNotifier {
    fn notify(&self, notification: Notification) {
        // Dropped without a word if the worker is gone: it said why when it went.
        let _ = self.jobs.send(notification);
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
