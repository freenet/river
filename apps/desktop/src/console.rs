//! Windows: quit River cleanly on Ctrl+C, Ctrl+Break and closing the console window, when
//! this process already owns a console (a console-subsystem or debug build).
//!
//! The release build is a GUI program and does NOT attach to its parent's console: the shell
//! prompt comes back at once, and an attached River would quit on a later Ctrl+C meant for an
//! unrelated command, or when that terminal closes. The close notice covers that case. Where a
//! console is already ours, the handler quits through the same path as the tray's Quit instead
//! of the default exit.

use std::sync::Mutex;

use river_app::RiverApp;
use rune::AppHandle;
use windows_sys::Win32::Foundation::{FALSE, TRUE};
use windows_sys::Win32::System::Console::{
    CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, GetConsoleProcessList, SetConsoleCtrlHandler,
};

static APP: Mutex<Option<AppHandle<RiverApp>>> = Mutex::new(None);

/// Whether a console event should quit River. Logoff and shutdown are left to the system.
fn quits(event: u32) -> bool {
    matches!(event, CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT)
}

/// Runs on a thread Windows makes for it: only signals the app, no UI work.
unsafe extern "system" fn on_event(event: u32) -> i32 {
    if !quits(event) {
        return FALSE;
    }
    match APP.lock() {
        Ok(app) => match app.as_ref() {
            Some(handle) => {
                handle.quit();
                TRUE
            }
            None => FALSE,
        },
        Err(_) => FALSE,
    }
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
    fn ctrl_c_break_and_closing_the_console_quit_but_logoff_does_not() {
        assert!(quits(CTRL_C_EVENT));
        assert!(quits(CTRL_BREAK_EVENT));
        assert!(quits(CTRL_CLOSE_EVENT));
        assert!(!quits(CTRL_LOGOFF_EVENT));
        assert!(!quits(CTRL_SHUTDOWN_EVENT));
    }
}
