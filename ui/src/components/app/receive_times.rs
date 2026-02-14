use dioxus::prelude::*;
use river_core::room_state::message::MessageId;
use std::collections::HashMap;

const STORAGE_KEY: &str = "river_receive_times";
/// Don't show delay for messages received within this threshold
const MIN_DELAY_SECS: i64 = 10;
/// Discard entries older than this
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
fn parse_map(s: &str) -> HashMap<i64, f64> {
    let mut map = HashMap::new();
    if s.is_empty() {
        return map;
    }
    for pair in s.split(',') {
        let mut parts = pair.splitn(2, ':');
        if let (Some(k), Some(v)) = (parts.next(), parts.next()) {
            if let (Ok(key), Ok(val)) = (k.parse::<i64>(), v.parse::<f64>()) {
                map.insert(key, val);
            }
        }
    }
    map
}

/// Serialize to "key:value,key:value,..." format
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

fn save_to_storage(map: &HashMap<i64, f64>) {
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
    let now = now_ms();
    RECEIVE_TIMES.with_mut(|map| {
        for id in message_ids {
            map.entry(id.0 .0).or_insert(now);
        }
        save_to_storage(map);
    });
}

/// Get the propagation delay for a message, if known and significant.
/// Returns delay in seconds, or None if unknown or under threshold.
pub fn get_delay_secs(message_id: &MessageId, send_time_ms: i64) -> Option<i64> {
    let recv_ms = *RECEIVE_TIMES.read().get(&message_id.0 .0)?;
    let delay_ms = recv_ms as i64 - send_time_ms;
    let delay_secs = delay_ms / 1000;
    if delay_secs >= MIN_DELAY_SECS {
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
