//! Windows: quit River cleanly on Ctrl+C, Ctrl+Break and closing the console window, when
//! this process already owns a console (a console-subsystem or debug build).
//!
//! The release build is a GUI program and does NOT attach to its parent's console: the shell
//! prompt comes back at once, and an attached River would quit on a later Ctrl+C meant for an
//! unrelated command, or when that terminal closes. The close notice covers that case. Where a
//! console is already ours, the handler quits through the same path as the tray's Quit instead
//! of the default exit.

use std::sync::{Condvar, Mutex};
use std::time::Duration;

use river_app::RiverApp;
use rune::AppHandle;
use windows_sys::Win32::Foundation::{FALSE, TRUE};
use windows_sys::Win32::System::Console::{
    CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, GetConsoleProcessList, SetConsoleCtrlHandler,
};

/// The running app, for the handler. Cleared by [`finished`], so a late Ctrl+C reaches the
/// default handler instead of being swallowed.
static APP: Mutex<Option<AppHandle<RiverApp>>> = Mutex::new(None);
static EXIT: Exit = Exit::new();

/// How long a console-close handler waits for River to finish. Windows ends the process
/// once the handler returns, and after about 5 seconds whatever it does.
const CLOSE_WAIT: Duration = Duration::from_millis(3500);

/// Set when `main` has finished its exit work, so the close handler knows it may return.
struct Exit {
    done: Mutex<bool>,
    cv: Condvar,
}

impl Exit {
    const fn new() -> Self {
        Exit {
            done: Mutex::new(false),
            cv: Condvar::new(),
        }
    }

    fn set(&self) {
        *self.done.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.cv.notify_all();
    }

    /// Wait up to `limit` for [`Exit::set`]; whether it happened.
    fn wait(&self, limit: Duration) -> bool {
        let guard = self.done.lock().unwrap_or_else(|e| e.into_inner());
        let (guard, _) = self
            .cv
            .wait_timeout_while(guard, limit, |done| !*done)
            .unwrap_or_else(|e| e.into_inner());
        *guard
    }
}

/// Whether a console event should quit River. Logoff and shutdown are left to the system.
fn quits(event: u32) -> bool {
    matches!(event, CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT)
}

/// Runs on a thread Windows makes for it: only signals the app, no UI work. `quit` queues a
/// request for the event loop and returns at once (a quit before the loop starts is still
/// honoured when it does), so closing the console, which ends the process when this returns,
/// waits here for `main` to finish.
unsafe extern "system" fn on_event(event: u32) -> i32 {
    if !quits(event) {
        return FALSE;
    }
    // The guard is dropped before quitting.
    let handle = APP.lock().ok().and_then(|app| app.clone());
    let Some(handle) = handle else {
        return FALSE;
    };
    handle.quit();
    if event == CTRL_CLOSE_EVENT {
        EXIT.wait(CLOSE_WAIT);
    }
    TRUE
}

/// `main` is done: the loop has returned and cleanup has run.
pub fn finished() {
    if let Ok(mut app) = APP.lock() {
        *app = None;
    }
    EXIT.set();
}

/// Whether this process already has a console (it never attaches to one).
fn owns_console() -> bool {
    let mut pid = 0u32;
    // SAFETY: one valid u32 for a list of length 1; the result is the count of processes
    // attached to the console, 0 when there is none.
    unsafe { GetConsoleProcessList(&mut pid, 1) != 0 }
}

pub fn quit_on_console_signals(handle: AppHandle<RiverApp>) {
    if !owns_console() {
        return;
    }
    if let Ok(mut app) = APP.lock() {
        *app = Some(handle);
    }
    // SAFETY: the handler is a valid `extern "system"` fn.
    unsafe {
        SetConsoleCtrlHandler(Some(on_event), TRUE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Console::{CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT};

    #[test]
    fn the_close_wait_ends_when_main_finishes_and_times_out_when_it_does_not() {
        let exit = Exit::new();
        assert!(
            !exit.wait(Duration::from_millis(10)),
            "not finished: times out"
        );
        exit.set();
        assert!(
            exit.wait(Duration::from_secs(5)),
            "finished: returns at once"
        );
    }

    #[test]
    fn ctrl_c_break_and_closing_the_console_quit_but_logoff_does_not() {
        assert!(quits(CTRL_C_EVENT));
        assert!(quits(CTRL_BREAK_EVENT));
        assert!(quits(CTRL_CLOSE_EVENT));
        assert!(!quits(CTRL_LOGOFF_EVENT));
        assert!(!quits(CTRL_SHUTDOWN_EVENT));
    }
}
