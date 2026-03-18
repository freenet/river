#![allow(dead_code)]

pub mod ecies;

use ed25519_dalek::VerifyingKey;
use freenet_stdlib::prelude::{ContractCode, ContractKey, Parameters};
use std::time::*;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

/// Spawn an async task safely from any context (including inside another task's poll).
///
/// On Firefox mobile, calling `spawn_local` from inside a wasm-bindgen-futures task
/// poll causes a RefCell re-entrant borrow panic (singlethread.rs:132). This helper
/// defers the spawn via `setTimeout(0)` to break out of the current call stack,
/// ensuring the TASKS RefCell is not borrowed when spawn_local runs.
#[cfg(target_arch = "wasm32")]
pub fn safe_spawn_local<F>(f: F)
where
    F: std::future::Future<Output = ()> + 'static,
{
    // We need to wrap the future in a Box to make it 'static and sendable via closure
    let boxed: std::pin::Pin<Box<dyn std::future::Future<Output = ()>>> = Box::pin(f);
    let cb = Closure::once_into_js(move || {
        wasm_bindgen_futures::spawn_local(boxed);
    });
    web_sys::window()
        .expect("no window")
        .set_timeout_with_callback(&cb.into())
        .ok();
}

/// Non-WASM fallback — just spawns normally (no-op since there's no async runtime)
#[cfg(not(target_arch = "wasm32"))]
pub fn safe_spawn_local<F>(_f: F)
where
    F: std::future::Future<Output = ()> + 'static,
{
    // No-op on non-WASM
}

/// Defer a synchronous closure to run outside the current call stack via `setTimeout(0)`.
///
/// This prevents `RefCell already borrowed` panics when mutating Dioxus signals
/// from inside `spawn_local` tasks or event handlers. The deferred closure runs
/// in a clean execution context where no signal borrows are active.
#[cfg(target_arch = "wasm32")]
pub fn defer(f: impl FnOnce() + 'static) {
    let cb = Closure::once_into_js(f);
    web_sys::window()
        .expect("no window")
        .set_timeout_with_callback(&cb.into())
        .ok();
}

/// Non-WASM fallback — runs the closure immediately.
#[cfg(not(target_arch = "wasm32"))]
pub fn defer(f: impl FnOnce() + 'static) {
    f();
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen(inline_js = "
export function get_current_time() {
    return Date.now();
}
export function format_time_local(timestamp_ms) {
    const date = new Date(timestamp_ms);
    return date.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit', hour12: false });
}
export function format_full_datetime_local(timestamp_ms) {
    const date = new Date(timestamp_ms);
    return date.toLocaleString(undefined, {
        weekday: 'short',
        year: 'numeric',
        month: 'short',
        day: 'numeric',
        hour: '2-digit',
        minute: '2-digit',
        second: '2-digit',
        hour12: false
    });
}
export function js_copy_to_clipboard(text) {
    // execCommand('copy') works in sandboxed iframes without allow-clipboard-write
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.style.position = 'fixed';
    ta.style.left = '-9999px';
    document.body.appendChild(ta);
    ta.select();
    document.execCommand('copy');
    document.body.removeChild(ta);
}
")]
extern "C" {
    fn get_current_time() -> f64;
    fn format_time_local(timestamp_ms: f64) -> String;
    fn format_full_datetime_local(timestamp_ms: f64) -> String;
    fn js_copy_to_clipboard(text: &str);
}

/// Copy text to clipboard. Works in sandboxed iframes where the Clipboard API is blocked.
pub fn copy_to_clipboard(text: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        js_copy_to_clipboard(text);
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = text;
    }
}

pub fn get_current_system_time() -> SystemTime {
    #[cfg(target_arch = "wasm32")]
    {
        // Convert milliseconds since epoch to a Duration
        let millis = get_current_time();
        let duration_since_epoch = Duration::from_millis(millis as u64);
        UNIX_EPOCH + duration_since_epoch
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        SystemTime::now()
    }
}

/// Format a UTC timestamp as a local time string (HH:MM format)
pub fn format_utc_as_local_time(timestamp_ms: i64) -> String {
    #[cfg(target_arch = "wasm32")]
    {
        format_time_local(timestamp_ms as f64)
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        use chrono::{Local, TimeZone, Utc};
        let utc_time = Utc.timestamp_millis_opt(timestamp_ms).unwrap();
        utc_time.with_timezone(&Local).format("%H:%M").to_string()
    }
}

/// Format a UTC timestamp as a full local datetime string for tooltips
pub fn format_utc_as_full_datetime(timestamp_ms: i64) -> String {
    #[cfg(target_arch = "wasm32")]
    {
        format_full_datetime_local(timestamp_ms as f64)
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        use chrono::{Local, TimeZone, Utc};
        let utc_time = Utc.timestamp_millis_opt(timestamp_ms).unwrap();
        utc_time
            .with_timezone(&Local)
            .format("%a, %b %d, %Y %H:%M:%S")
            .to_string()
    }
}

// Helper function to create a Duration from seconds
pub fn seconds(s: u64) -> Duration {
    Duration::from_secs(s)
}

// Helper function to create a Duration from milliseconds
pub fn millis(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

/// A WASM-compatible sleep function that works in both browser and native environments
pub async fn sleep(duration: Duration) {
    #[cfg(target_arch = "wasm32")]
    {
        let promise = js_sys::Promise::new(&mut |resolve, _| {
            let window = web_sys::window().unwrap();
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                &resolve,
                duration.as_millis() as i32,
            );
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        // Use futures_timer for non-WASM environments to maintain compatibility
        let _ = futures_timer::Delay::new(duration).await;
    }
}

#[cfg(feature = "example-data")]
mod name_gen;
#[cfg(feature = "example-data")]
pub use name_gen::random_full_name;

use crate::constants::ROOM_CONTRACT_WASM;
use river_core::room_state::ChatRoomParametersV1;

pub fn to_cbor_vec<T: serde::Serialize>(value: &T) -> Vec<u8> {
    let mut buffer = Vec::new();
    ciborium::ser::into_writer(value, &mut buffer).unwrap();
    buffer
}

pub fn from_cbor_slice<T: serde::de::DeserializeOwned>(data: &[u8]) -> T {
    ciborium::de::from_reader(data).unwrap()
}

/// Check if debug overlay is enabled via `?debug=1` query parameter.
#[cfg(target_arch = "wasm32")]
fn is_debug_enabled() -> bool {
    // Cache the result in a static to avoid repeated URL parsing
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        web_sys::window()
            .and_then(|w| w.location().search().ok())
            .map(|s| {
                web_sys::UrlSearchParams::new_with_str(&s)
                    .ok()
                    .and_then(|p| p.get("debug"))
                    .map(|v| v == "1" || v == "true")
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    })
}

/// Append a debug message to a floating on-screen log overlay.
/// Only active when `?debug=1` is in the URL query string.
/// On mobile browsers where console is inaccessible, this lets the user
/// see (and screenshot) what the app is doing during message send, etc.
#[cfg(target_arch = "wasm32")]
pub fn debug_log(msg: &str) {
    use dioxus::logger::tracing::info;
    info!("{}", msg);

    if !is_debug_enabled() {
        return;
    }

    let Some(window) = web_sys::window() else {
        return;
    };
    let Some(document) = window.document() else {
        return;
    };

    // Create or find the debug container (wrapper with toggle button + log)
    let container = match document.get_element_by_id("river-debug-container") {
        Some(el) => el,
        None => {
            let el = document.create_element("div").unwrap();
            el.set_id("river-debug-container");
            el.set_attribute(
                "style",
                "position:fixed;bottom:0;left:0;right:0;z-index:999998;\
                 pointer-events:auto;",
            )
            .ok();

            // Toggle button
            let btn = document.create_element("div").unwrap();
            btn.set_id("river-debug-toggle");
            btn.set_attribute(
                "style",
                "background:#222;color:#0f0;font-family:monospace;font-size:11px;\
                 padding:2px 8px;cursor:pointer;text-align:right;border-top:1px solid #333;\
                 user-select:none;-webkit-user-select:none;",
            )
            .ok();
            btn.set_inner_html("[debug] tap to minimize");
            btn.set_attribute(
                "onclick",
                "var log=document.getElementById('river-debug-log');\
                 var btn=document.getElementById('river-debug-toggle');\
                 if(log.style.display==='none'){\
                   log.style.display='block';btn.innerHTML='[debug] tap to minimize';\
                 }else{\
                   log.style.display='none';btn.innerHTML='[debug] tap to expand';\
                 }",
            )
            .ok();
            el.append_child(&btn).ok();

            // Log area
            let log = document.create_element("div").unwrap();
            log.set_id("river-debug-log");
            log.set_attribute(
                "style",
                "max-height:25vh;background:rgba(0,0,0,0.85);color:#0f0;\
                 font-family:monospace;font-size:11px;overflow:auto;\
                 padding:4px 8px;white-space:pre-wrap;word-break:break-all;",
            )
            .ok();
            el.append_child(&log).ok();

            if let Some(body) = document.body() {
                body.append_child(&el).ok();
            }
            el
        }
    };

    // Get the log element
    let Some(overlay) = document.get_element_by_id("river-debug-log") else {
        return;
    };
    let _ = container; // keep container alive

    // Timestamp
    let now = js_sys::Date::new_0();
    let ts = format!(
        "{:02}:{:02}:{:02}",
        now.get_hours(),
        now.get_minutes(),
        now.get_seconds()
    );

    // Append the new line (keep last 50 lines)
    let current = overlay.inner_html();
    let lines: Vec<&str> = current.lines().collect();
    let trimmed = if lines.len() > 49 {
        lines[lines.len() - 49..].join("\n")
    } else {
        current.clone()
    };
    let new_content = format!(
        "{}{}{} {}",
        trimmed,
        if trimmed.is_empty() { "" } else { "\n" },
        ts,
        msg.replace('<', "&lt;").replace('>', "&gt;")
    );
    overlay.set_inner_html(&new_content);

    // Auto-scroll to bottom
    overlay.set_scroll_top(overlay.scroll_height());
}

#[cfg(not(target_arch = "wasm32"))]
pub fn debug_log(msg: &str) {
    dioxus::logger::tracing::info!("{}", msg);
}

pub fn owner_vk_to_contract_key(owner_vk: &VerifyingKey) -> ContractKey {
    let params = ChatRoomParametersV1 { owner: *owner_vk };
    let params_bytes = to_cbor_vec(&params);
    let parameters = Parameters::from(params_bytes);
    let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
    // Use the full ContractKey constructor that includes the code hash
    ContractKey::from_params_and_code(parameters, &contract_code)
}
