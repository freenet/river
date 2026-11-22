//! Windows: quit River on Ctrl+C, Ctrl+Break and closing the console window.
//!
//! The release build is a GUI program with no console, so a River started from PowerShell or
//! `cargo run` would otherwise ignore Ctrl+C. This attaches to the parent's console when there
//! is one and installs a control handler that quits through the same path as the tray's Quit.
//! With no console to attach to (Explorer, login item, tray) nothing happens and nothing is
//! said. A debug build already has a console, so the attach is refused as already attached and
//! the handler is installed all the same.

use std::sync::Mutex;

use river_app::RiverApp;
use rune::AppHandle;
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, FALSE, GetLastError, TRUE};
use windows_sys::Win32::System::Console::{
    ATTACH_PARENT_PROCESS, AttachConsole, CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT,
    SetConsoleCtrlHandler,
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

pub fn quit_on_console_signals(handle: AppHandle<RiverApp>) {
    // SAFETY: plain Win32 calls with no pointers; the handler is a valid `extern "system"` fn.
    unsafe {
        // Zero is failure. "Access denied" means this process already has a console (a debug
        // build); any other failure means there is no parent console to listen on.
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 && GetLastError() != ERROR_ACCESS_DENIED {
            return;
        }
        if let Ok(mut app) = APP.lock() {
            *app = Some(handle);
        }
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
