//! Windows: toasts through notify-rust (tauri-winrt-notification).
//!
//! A toast needs an AppUserModelID. Without one it is labelled "Windows PowerShell". River
//! has no installer to register one, so [`register_identity`] writes the minimum under
//! `HKCU` at startup and sets it as the process's own.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use river_app::RiverApp;
use river_app::notify::Notification;
use rune::AppHandle;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegSetValueExW,
};
use windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;

use super::Waiters;

/// River's AppUserModelID.
const APP_ID: &str = "org.freenet.River";
const LOGO: &[u8] = include_bytes!("../../packaging/icons/river.png");

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn set_string(key: HKEY, name: &str, value: &str) -> Result<(), u32> {
    let (name, value) = (wide(name), wide(value));
    // SAFETY: `name` and `value` are NUL-terminated and outlive the call; the length counts
    // the value's bytes including its terminator, as REG_SZ requires.
    let status = unsafe {
        RegSetValueExW(
            key,
            name.as_ptr(),
            0,
            REG_SZ,
            value.as_ptr().cast(),
            (value.len() * 2) as u32,
        )
    };
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(status)
    }
}

/// The logo as a file, which a toast's identity can point at, kept in the user's local app
/// data.
fn logo_file() -> Option<PathBuf> {
    let dir = directories::BaseDirs::new()?.data_local_dir().join("River");
    let path = dir.join("river-toast.png");
    let current = std::fs::metadata(&path).is_ok_and(|m| m.len() == LOGO.len() as u64);
    if !current {
        std::fs::create_dir_all(&dir).ok()?;
        std::fs::write(&path, LOGO).ok()?;
    }
    Some(path)
}

/// Register River's toast identity for this user, without administrator rights, and make it
/// this process's. Call once at startup, before the first window. A failure is logged and
/// leaves toasts labelled as PowerShell, which is ugly but works.
pub fn register_identity() {
    let subkey = wide(&format!("Software\\Classes\\AppUserModelId\\{APP_ID}"));
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: `subkey` is NUL-terminated and outlives the call, `key` is a valid out pointer,
    // and the optional pointer arguments are allowed to be null.
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        )
    };
    if status != ERROR_SUCCESS {
        eprintln!("river: could not register River for notifications (registry error {status})");
    } else {
        let named = set_string(key, "DisplayName", "River");
        let icon = logo_file()
            .map(|p| set_string(key, "IconUri", &p.to_string_lossy()))
            .unwrap_or(Ok(()));
        if let Err(status) = named.and(icon) {
            eprintln!("river: could not fully register River for notifications ({status})");
        }
        // SAFETY: `key` was opened by RegCreateKeyExW above and is closed once.
        unsafe { RegCloseKey(key) };
    }
    let id = wide(APP_ID);
    // SAFETY: `id` is NUL-terminated and outlives the call.
    let hr = unsafe { SetCurrentProcessExplicitAppUserModelID(id.as_ptr()) };
    if hr < 0 {
        eprintln!("river: could not set River's app id (HRESULT {hr:#x})");
    }
}

pub struct Backend {
    waiters: Waiters,
    /// A number for each toast: they stack, so each has its own waiter.
    next: AtomicU64,
}

impl Backend {
    pub fn new() -> Self {
        Backend {
            waiters: Waiters::default(),
            next: AtomicU64::new(0),
        }
    }

    pub fn show(&mut self, n: &Notification, handle: &AppHandle<RiverApp>) {
        // Windows toasts here cannot be replaced per room (the toast crate sets no tag), so
        // they stack; Windows shows the newest few.
        //
        // A click is heard only while the toast is on screen or its process-local handler is
        // alive. After the toast times out (about 5 seconds) it sits in the Action Center, and
        // a click there most likely does not reach this process: that needs an activator
        // registered by an installer (a Start Menu shortcut with this app id and a COM
        // server). Untested.
        let mut note = notify_rust::Notification::new();
        note.appname("River")
            .app_id(APP_ID)
            .summary(&n.title)
            .body(&n.body);
        let shown = match note.show() {
            Ok(shown) => shown,
            Err(e) => {
                eprintln!("river: could not show a notification: {e}");
                return;
            }
        };
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.waiters
            .spawn(n.room_key.clone(), id, handle.clone(), move |on| {
                let _ = shown.wait_for_response(on);
            });
    }
}
