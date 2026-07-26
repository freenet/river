use dioxus::logger::tracing::info;
use dioxus::prelude::*;
use river_core::room_state::message::MessageId;
use std::collections::HashMap;

#[cfg(target_arch = "wasm32")]
const STORAGE_KEY: &str = "river_receive_times";
/// Discard entries older than this
#[cfg(target_arch = "wasm32")]
const MAX_AGE_MS: f64 = 24.0 * 60.0 * 60.0 * 1000.0; // 24 hours

/// Maps message ID inner value (i64) to receive timestamp in milliseconds since epoch.
pub static RECEIVE_TIMES: GlobalSignal<HashMap<i64, f64>> = Global::new(load_from_storage);

fn now_ms() -> f64 {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::now()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as f64
    }
}

/// Parse "key:value,key:value,..." format
#[cfg(target_arch = "wasm32")]
fn parse_map(s: &str) -> HashMap<i64, f64> {
    let mut map = HashMap::new();
    if s.is_empty() {
        return map;
    }
    for pair in s.split(',') {
        let mut parts = pair.splitn(2, ':');
        if let (Some(k), Some(v)) = (parts.next(), parts.next()) {
            if let (Ok(key), Ok(val)) = (k.parse::<i64>(), v.parse::<f64>()) {
                if is_plausible_arrival_ms(val) {
                    map.insert(key, val);
                }
            }
        }
    }
    map
}

/// Is `ms` a time this client could plausibly have received a message at?
///
/// These values come back out of `localStorage`, which anything running in the
/// origin can write, and they are no longer only a diagnostic: `group_messages`
/// clamps a future-dated sender timestamp to the arrival time, so a garbage
/// entry would move that message's rendered time, its 5-minute grouping and its
/// day separator. A NaN, a negative, or a value from before this project
/// existed is not an arrival time; neither is one in the future, since we
/// record arrivals as they happen. Rejecting them here means every reader gets
/// a sane map rather than each one re-checking.
#[cfg(target_arch = "wasm32")]
fn is_plausible_arrival_ms(ms: f64) -> bool {
    /// 2020-01-01, comfortably before any River build existed.
    const EARLIEST_PLAUSIBLE_MS: f64 = 1_577_836_800_000.0;
    // A little slack for a clock that ticks between the write and the read.
    const FUTURE_SLACK_MS: f64 = 60.0 * 1000.0;
    ms.is_finite() && ms >= EARLIEST_PLAUSIBLE_MS && ms <= now_ms() + FUTURE_SLACK_MS
}

/// Serialize to "key:value,key:value,..." format
#[cfg(target_arch = "wasm32")]
fn serialize_map(map: &HashMap<i64, f64>) -> String {
    map.iter()
        .map(|(k, v)| format!("{}:{}", k, *v as i64))
        .collect::<Vec<_>>()
        .join(",")
}

fn load_from_storage() -> HashMap<i64, f64> {
    #[cfg(target_arch = "wasm32")]
    {
        let window = match web_sys::window() {
            Some(w) => w,
            None => return HashMap::new(),
        };
        let storage = match window.local_storage() {
            Ok(Some(s)) => s,
            _ => return HashMap::new(),
        };
        let data = match storage.get_item(STORAGE_KEY) {
            Ok(Some(s)) => s,
            _ => return HashMap::new(),
        };
        let map = parse_map(&data);
        // Housekeep: remove entries older than MAX_AGE_MS
        let now = now_ms();
        map.into_iter()
            .filter(|(_, recv_ms)| now - recv_ms < MAX_AGE_MS)
            .collect()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        HashMap::new()
    }
}

fn save_to_storage(#[allow(unused_variables)] map: &HashMap<i64, f64>) {
    #[cfg(target_arch = "wasm32")]
    {
        let window = match web_sys::window() {
            Some(w) => w,
            None => return,
        };
        let storage = match window.local_storage() {
            Ok(Some(s)) => s,
            _ => return,
        };
        let _ = storage.set_item(STORAGE_KEY, &serialize_map(map));
    }
}

/// Record receive timestamps for newly arrived messages.
pub fn record_receive_times(message_ids: &[MessageId]) {
    if message_ids.is_empty() {
        return;
    }
    info!("Recording receive times for {} messages", message_ids.len());
    let now = now_ms();
    RECEIVE_TIMES.with_mut(|map| {
        let mut new_count = 0;
        for id in message_ids {
            if map.entry(id.0 .0).or_insert(now) == &now {
                new_count += 1;
            }
        }
        save_to_storage(map);
        info!(
            "Saved {} receive times ({} new), total entries: {}",
            message_ids.len(),
            new_count,
            map.len()
        );
    });
}

/// A snapshot of the first-seen times, taken once per grouping pass.
///
/// `group_messages` needs the same map for every message it walks, and reading
/// the `RECEIVE_TIMES` `GlobalSignal` per message both costs a borrow each time
/// and makes the function unusable outside a Dioxus runtime (so untestable).
/// Taking the reference once and threading it through fixes both.
pub type ReceiveTimes = HashMap<i64, f64>;

/// When this client FIRST saw a message, in ms since the epoch.
///
/// This is local wall-clock at arrival, so unlike the sender-supplied
/// timestamp it cannot be in the future and cannot be moved by a remote clock.
pub fn first_seen_ms(times: &ReceiveTimes, message_id: &MessageId) -> Option<f64> {
    times.get(&message_id.0 .0).copied()
}

/// Get the propagation delay for a message, if known.
/// Returns delay in seconds, or None if unknown or negative (clock skew).
pub fn get_delay_secs_from(
    times: &ReceiveTimes,
    message_id: &MessageId,
    send_time_ms: i64,
) -> Option<i64> {
    let recv_ms = first_seen_ms(times, message_id)?;
    let delay_ms = recv_ms as i64 - send_time_ms;
    let delay_secs = delay_ms / 1000;
    if delay_secs >= 0 {
        Some(delay_secs)
    } else {
        None
    }
}

/// Format a delay in seconds into a human-readable string.
pub fn format_delay(secs: i64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        let mins = secs / 60;
        format!("{}m", mins)
    } else if secs < 86400 {
        let hours = secs / 3600;
        let mins = (secs % 3600) / 60;
        if mins > 0 {
            format!("{}h {}m", hours, mins)
        } else {
            format!("{}h", hours)
        }
    } else {
        let days = secs / 86400;
        format!("{}d", days)
    }
}
