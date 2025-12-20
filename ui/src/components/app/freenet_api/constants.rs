#![allow(dead_code)]

/// Fallback WebSocket URL for non-browser environments
const FALLBACK_WEBSOCKET_URL: &str = "ws://localhost:7509/v1/contract/command?encodingProtocol=native";

/// Get the WebSocket URL for connecting to the Freenet node.
/// Derives the URL from the current window.location, allowing River to work
/// on any host/port (not just localhost:7509).
#[cfg(target_arch = "wasm32")]
pub fn get_websocket_url() -> String {
    if let Some(window) = web_sys::window() {
        let location = window.location();
        let protocol = location.protocol().unwrap_or_default();
        let host = location.host().unwrap_or_default(); // includes port

        let ws_protocol = if protocol == "https:" { "wss:" } else { "ws:" };
        format!("{}//{}/v1/contract/command?encodingProtocol=native", ws_protocol, host)
    } else {
        FALLBACK_WEBSOCKET_URL.to_string()
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn get_websocket_url() -> String {
    FALLBACK_WEBSOCKET_URL.to_string()
}

/// Default timeout for WebSocket connection in milliseconds
pub const CONNECTION_TIMEOUT_MS: u64 = 5000;

/// Delay after PUT before subscribing to a room in milliseconds
pub const POST_PUT_DELAY_MS: u64 = 3000;

/// Retry interval for reconnection attempts in milliseconds
pub const RECONNECT_INTERVAL_MS: u64 = 3000;

/// Maximum number of retries for API requests
pub const MAX_REQUEST_RETRIES: u8 = 3;
