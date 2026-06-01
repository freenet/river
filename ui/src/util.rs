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
///
/// IMPORTANT: The deferred closure runs with the Dioxus runtime pushed via
/// `RuntimeGuard`, so GlobalSignal access (which calls `Runtime::current()`)
/// won't panic. The runtime is captured from `CAPTURED_RUNTIME` which must be
/// initialized at app startup via `capture_runtime()`.
#[cfg(target_arch = "wasm32")]
pub fn defer(f: impl FnOnce() + 'static) {
    let runtime = CAPTURED_RUNTIME.with(|rt| rt.borrow().clone());
    let cb = Closure::once_into_js(move || {
        if let Some(rt) = runtime {
            // Push the Dioxus runtime AND a root scope so both Runtime::current()
            // and current_scope_id() work from setTimeout callbacks
            rt.in_scope(dioxus::dioxus_core::ScopeId::ROOT, f);
        } else {
            // No captured runtime — run without guard (may panic on signal access)
            f();
        }
    });
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

// Thread-local storage for the captured Dioxus runtime.
// In WASM (single-threaded), this is effectively a global.
thread_local! {
    static CAPTURED_RUNTIME: std::cell::RefCell<Option<std::rc::Rc<dioxus::dioxus_core::Runtime>>> =
        const { std::cell::RefCell::new(None) };
}

/// Capture the current Dioxus runtime for use in `defer()` and `safe_spawn_local()`.
///
/// Must be called once from inside a Dioxus component or effect (where the runtime
/// is on the stack). After this, `defer()` callbacks will push the runtime via
/// `RuntimeGuard` so that `GlobalSignal` access works from `setTimeout` callbacks.
pub fn capture_runtime() {
    let rt = dioxus::dioxus_core::Runtime::current();
    CAPTURED_RUNTIME.with(|cell| {
        *cell.borrow_mut() = Some(rt);
    });
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
export function local_date_key(timestamp_ms) {
    // Local calendar date (viewer's timezone) as a zero-padded YYYY-MM-DD
    // string. Used to detect day boundaries for chat date separators.
    const date = new Date(timestamp_ms);
    const y = date.getFullYear();
    const m = String(date.getMonth() + 1).padStart(2, '0');
    const d = String(date.getDate()).padStart(2, '0');
    return y + '-' + m + '-' + d;
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
    fn local_date_key(timestamp_ms: f64) -> String;
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

/// Wasm-safe replacement for `std::time::SystemTime::now()`. On wasm32,
/// `SystemTime::now()` panics at runtime ("time not implemented on this
/// platform") because rustc's wasm stub is `unreachable!()`. This helper
/// routes through `Date.now()` via the `wasm_bindgen` shim above on wasm
/// and falls through to `SystemTime::now()` on native.
///
/// **Use this in all NEW UI code that needs wall-clock time.** A direct
/// `SystemTime::now()` slipping into a code path that runs on wasm will
/// crash the entire page (incident: PR #244 DM-thread composer). The
/// existing `#[cfg(not(target_arch = "wasm32"))]`-gated `SystemTime::now()`
/// call sites in this crate are intentional (native-only test helpers),
/// but anything reachable from a wasm code path MUST go through here.
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

/// The local calendar date (viewer's timezone) on which a UTC timestamp
/// falls. Used to insert day-change separators into the message list.
///
/// On wasm the local-date computation MUST go through JS (`Date`), because
/// chrono's `Local` needs timezone data that isn't present in the wasm build
/// and would panic — the same reason `format_utc_as_local_time` routes through
/// the JS shim. The day boundary is the viewer's local midnight, so two
/// timestamps a minute apart across midnight correctly land on different days.
pub fn local_message_date(timestamp_ms: i64) -> chrono::NaiveDate {
    #[cfg(target_arch = "wasm32")]
    {
        let key = local_date_key(timestamp_ms as f64);
        chrono::NaiveDate::parse_from_str(&key, "%Y-%m-%d")
            .unwrap_or_else(|_| chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap())
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        use chrono::{Local, TimeZone, Utc};
        let utc_time = Utc.timestamp_millis_opt(timestamp_ms).unwrap();
        utc_time.with_timezone(&Local).date_naive()
    }
}

/// Today's local calendar date in the viewer's timezone.
pub fn local_today() -> chrono::NaiveDate {
    #[cfg(target_arch = "wasm32")]
    {
        local_message_date(get_current_time() as i64)
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        chrono::Local::now().date_naive()
    }
}

/// Human-readable day-separator label for `that_day` relative to `today`:
/// `"Today"`, `"Yesterday"`, `"Monday, June 3"` (same year) or
/// `"Monday, June 3, 2025"` (different year). Pure and deterministic so it can
/// be unit-tested on all targets; the timezone-dependent part lives in
/// [`local_message_date`]. Older-than-yesterday and any future-dated day both
/// fall through to the full weekday+date label.
pub fn format_date_separator_label(
    that_day: chrono::NaiveDate,
    today: chrono::NaiveDate,
) -> String {
    use chrono::Datelike;
    let diff = (today - that_day).num_days();
    if diff == 0 {
        "Today".to_string()
    } else if diff == 1 {
        "Yesterday".to_string()
    } else if that_day.year() == today.year() {
        that_day.format("%A, %B %-d").to_string()
    } else {
        that_day.format("%A, %B %-d, %Y").to_string()
    }
}

/// Given the local dates of consecutive chat display items (in display order)
/// and today's local date, return the day-change separator to render above
/// each item: `Some(label)` when that item's day differs from the previous
/// item's (the first item always gets one), `None` otherwise. Pure so the
/// "one divider per day, deduped across same-day groups" behaviour can be
/// unit-tested deterministically without a browser.
pub fn date_separator_labels(
    item_dates: &[chrono::NaiveDate],
    today: chrono::NaiveDate,
) -> Vec<Option<String>> {
    let mut out = Vec::with_capacity(item_dates.len());
    let mut prev: Option<chrono::NaiveDate> = None;
    for &date in item_dates {
        if prev != Some(date) {
            out.push(Some(format_date_separator_label(date, today)));
        } else {
            out.push(None);
        }
        prev = Some(date);
    }
    out
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

/// Like [`from_cbor_slice`] but returns `None` instead of panicking when the
/// bytes do not deserialize. Use this for bytes from a possibly-incompatible
/// source — e.g. a legacy room-contract generation whose `ChatRoomStateV1`
/// layout predates the current one (freenet/river#292).
pub fn try_from_cbor_slice<T: serde::de::DeserializeOwned>(data: &[u8]) -> Option<T> {
    ciborium::de::from_reader(data).ok()
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

/// Truncate a string to at most `max_bytes` bytes without splitting a
/// multi-byte UTF-8 character.  Returns the longest prefix whose byte
/// length is ≤ `max_bytes` and that ends on a char boundary.
pub fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    // Walk backwards from `max_bytes` to find a char boundary.
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
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

/// Contract keys for `owner_vk` under every previous room-contract WASM
/// generation, newest-first (freenet/river#292).
pub fn owner_vk_to_legacy_contract_keys(owner_vk: &VerifyingKey) -> Vec<ContractKey> {
    river_core::migration::legacy_contract_keys_for_owner(owner_vk)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn date_separator_today() {
        let today = ymd(2026, 6, 1);
        assert_eq!(format_date_separator_label(today, today), "Today");
    }

    #[test]
    fn date_separator_yesterday() {
        let today = ymd(2026, 6, 1);
        assert_eq!(
            format_date_separator_label(ymd(2026, 5, 31), today),
            "Yesterday"
        );
    }

    #[test]
    fn date_separator_same_year_uses_weekday_and_date() {
        let today = ymd(2026, 6, 1);
        assert_eq!(
            format_date_separator_label(ymd(2026, 5, 25), today),
            "Monday, May 25"
        );
    }

    #[test]
    fn date_separator_different_year_includes_year() {
        let today = ymd(2026, 1, 2);
        assert_eq!(
            format_date_separator_label(ymd(2025, 12, 30), today),
            "Tuesday, December 30, 2025"
        );
    }

    #[test]
    fn date_separator_future_date_falls_through_to_full_label() {
        // A day "after" today (diff < 0) is not Today/Yesterday — it should
        // render the full weekday+date label, never "Tomorrow".
        let today = ymd(2026, 6, 1);
        assert_eq!(
            format_date_separator_label(ymd(2026, 6, 3), today),
            "Wednesday, June 3"
        );
    }

    #[test]
    fn date_separator_single_digit_day_not_zero_padded() {
        let today = ymd(2026, 6, 20);
        assert_eq!(
            format_date_separator_label(ymd(2026, 6, 3), today),
            "Wednesday, June 3"
        );
    }

    #[test]
    fn date_separator_labels_emits_once_per_day_and_dedupes_within_a_day() {
        let today = ymd(2026, 6, 1);
        let yesterday = ymd(2026, 5, 31);
        // Display order: 3 groups yesterday, then 2 groups today.
        let dates = vec![yesterday, yesterday, yesterday, today, today];
        let labels = date_separator_labels(&dates, today);
        assert_eq!(
            labels,
            vec![
                Some("Yesterday".to_string()),
                None,
                None,
                Some("Today".to_string()),
                None,
            ]
        );
    }

    #[test]
    fn date_separator_labels_first_item_always_labeled() {
        let today = ymd(2026, 6, 1);
        assert_eq!(
            date_separator_labels(&[today], today),
            vec![Some("Today".to_string())]
        );
    }

    #[test]
    fn date_separator_labels_empty_input() {
        let today = ymd(2026, 6, 1);
        assert!(date_separator_labels(&[], today).is_empty());
    }

    #[test]
    fn date_separator_labels_relabels_when_day_recurs() {
        // A later item that lands back on an earlier day still gets its own
        // separator (the comparison is against the immediately preceding
        // item's day, not a set of seen days).
        let today = ymd(2026, 6, 2);
        let d1 = ymd(2026, 5, 31);
        let d2 = ymd(2026, 6, 1);
        let labels = date_separator_labels(&[d1, d2, d1], today);
        assert_eq!(
            labels,
            vec![
                Some("Sunday, May 31".to_string()),
                Some("Yesterday".to_string()),
                Some("Sunday, May 31".to_string()),
            ]
        );
    }

    #[test]
    fn truncate_str_ascii_shorter_than_max() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_str_ascii_at_max() {
        assert_eq!(truncate_str("hello", 5), "hello");
    }

    #[test]
    fn truncate_str_ascii_longer_than_max() {
        assert_eq!(truncate_str("hello world", 5), "hello");
    }

    #[test]
    fn truncate_str_emoji_at_boundary() {
        // 👀 is 4 bytes (F0 9F 91 80). "Hello 👀 world" has 👀 at bytes 6..10.
        // Truncating at 8 should back up to byte 6 (before the emoji).
        assert_eq!(truncate_str("Hello 👀 world", 8), "Hello ");
    }

    #[test]
    fn truncate_str_the_actual_bug() {
        // The exact crash: "That moment when you...👀👀👀!" truncated at 30
        // "That moment when you..." = 23 bytes, each 👀 = 4 bytes.
        // Byte 30 is inside the second 👀 (bytes 27..31), so we back up to 27.
        let msg = "That moment when you...\u{1F440}\u{1F440}\u{1F440}!";
        let result = truncate_str(msg, 30);
        assert_eq!(result, "That moment when you...👀");
        assert!(!std::panic::catch_unwind(|| truncate_str(msg, 30)).is_err());
    }

    #[test]
    fn truncate_str_all_emoji() {
        // Each 👀 is 4 bytes. 5 bytes should return one emoji (4 bytes).
        assert_eq!(truncate_str("👀👀👀", 5), "👀");
    }

    #[test]
    fn truncate_str_max_zero() {
        assert_eq!(truncate_str("hello", 0), "");
    }

    #[test]
    fn truncate_str_empty() {
        assert_eq!(truncate_str("", 10), "");
    }

    #[test]
    fn truncate_str_two_byte_utf8() {
        // 'é' is 2 bytes (C3 A9). "café" = [63 61 66 C3 A9] = 5 bytes.
        // Truncating at 4 should back up to byte 3 (before 'é').
        assert_eq!(truncate_str("café", 4), "caf");
    }

    #[test]
    fn truncate_str_three_byte_utf8() {
        // '€' is 3 bytes (E2 82 AC). "a€b" = [61 E2 82 AC 62] = 5 bytes.
        // Truncating at 3 should back up to byte 1 (before '€').
        assert_eq!(truncate_str("a€b", 3), "a");
    }

    /// Codex P1 finding on PR #276 round 2: any caller that needs to
    /// know "what contract id will the delegate see after the next
    /// `Rooms::merge()`" MUST derive that id via this helper (which
    /// hashes the CURRENT bundled `ROOM_CONTRACT_WASM`), NOT by reading
    /// `room_data.contract_key.id()` on a pre-merge `RoomData` (which
    /// can be stale across WASM rebuilds). Pin: for the same
    /// `owner_vk`, `owner_vk_to_contract_key` returns the same id as
    /// `regenerate_contract_key()` writes onto a `RoomData` — both call
    /// `ContractKey::from_params_and_code(<params>, ROOM_CONTRACT_WASM)`,
    /// so a drift between the two functions would re-introduce the
    /// "subscribe to stale contract id" bug that this finding fixes.
    #[test]
    fn owner_vk_to_contract_key_matches_regenerate_contract_key() {
        use crate::room_data::RoomData;
        use ed25519_dalek::SigningKey;
        use river_core::ChatRoomStateV1;
        use std::collections::HashMap;

        let owner_sk = SigningKey::from_bytes(&[7u8; 32]);
        let owner_vk = owner_sk.verifying_key();

        // Build a RoomData with an intentionally bogus contract_key (any
        // value that's not the live derivation). `regenerate_contract_key()`
        // must then overwrite it with the same id `owner_vk_to_contract_key`
        // produces.
        let bogus_params = Parameters::from(b"bogus".to_vec());
        let bogus_code = ContractCode::from(b"bogus".to_vec());
        let bogus_key = ContractKey::from_params_and_code(bogus_params, &bogus_code);
        let mut room_data = RoomData {
            owner_vk,
            room_state: ChatRoomStateV1::default(),
            self_sk: owner_sk,
            contract_key: bogus_key,
            last_read_message_id: None,
            secrets: HashMap::new(),
            current_secret_version: None,
            last_secret_rotation: None,
            key_migrated_to_delegate: false,
            self_authorized_member: None,
            invite_chain: vec![],
            self_member_info: None,
            self_nickname: None,
            previous_contract_key: None,
            invitation_secrets: HashMap::new(),
        };
        room_data.regenerate_contract_key();

        let derived = owner_vk_to_contract_key(&owner_vk);
        assert_eq!(
            room_data.contract_key.id(),
            derived.id(),
            "owner_vk_to_contract_key MUST agree with RoomData::regenerate_contract_key — \
             otherwise the response_handler load-rooms path can subscribe the delegate \
             to a stale contract id after a room-contract WASM rebuild (PR #276 round 2 \
             Codex P1)"
        );
    }

    /// freenet/river#292: the `river_core::migration` legacy-key derivation
    /// (`contract_key_for_code_hash`, used by the backward-probe path) MUST
    /// agree with the live `owner_vk_to_contract_key` derivation when fed the
    /// CURRENT bundled WASM's code hash. If they drift, a probe that recovers
    /// a stranded room would PUT it forward onto a contract id that is NOT the
    /// one the rest of the UI subscribes to.
    #[test]
    fn legacy_derivation_matches_live_key_for_current_wasm() {
        use ed25519_dalek::SigningKey;

        let owner = SigningKey::from_bytes(&[7u8; 32]).verifying_key();
        let current_code_hash = blake3::hash(ROOM_CONTRACT_WASM);
        let derived_via_migration =
            river_core::migration::contract_key_for_code_hash(&owner, current_code_hash.as_bytes());
        assert_eq!(
            derived_via_migration.id(),
            owner_vk_to_contract_key(&owner).id(),
            "river_core::migration::contract_key_for_code_hash on the current WASM's \
             code hash MUST produce the same contract id as owner_vk_to_contract_key \
             (freenet/river#292)"
        );
    }

    /// freenet/river#292: the current bundled WASM's code hash must NOT appear
    /// in the legacy registry. If it did, the backward probe would re-derive
    /// the current key as a "legacy" key and GET it redundantly.
    #[test]
    fn current_wasm_not_in_legacy_registry() {
        let current_code_hash = blake3::hash(ROOM_CONTRACT_WASM);
        assert!(
            !river_core::migration::LEGACY_ROOM_CONTRACT_CODE_HASHES
                .contains(current_code_hash.as_bytes()),
            "the current room-contract WASM's code hash must not be in \
             LEGACY_ROOM_CONTRACT_CODE_HASHES (freenet/river#292)"
        );
    }
}
