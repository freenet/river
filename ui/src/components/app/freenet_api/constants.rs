#![allow(dead_code)]

/// Fallback WebSocket URL for non-browser environments
const FALLBACK_WEBSOCKET_URL: &str =
    "ws://localhost:7509/v1/contract/command?encodingProtocol=native";

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
        format!(
            "{}//{}/v1/contract/command?encodingProtocol=native",
            ws_protocol, host
        )
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

/// Initial retry interval for reconnection attempts in milliseconds
pub const RECONNECT_INITIAL_MS: u64 = 3000;

/// Maximum retry interval for reconnection attempts in milliseconds
pub const RECONNECT_MAX_MS: u64 = 60_000;

/// How long (ms) a connection must stay up before the reconnect backoff counter
/// is reset. Resetting merely on socket `onopen` let a socket that opens then
/// immediately dies reset the counter every cycle, defeating the exponential
/// backoff and producing an endless ~3s reconnect loop after an Android
/// background→resume (freenet/river#406). A connection must prove itself stable
/// for this long before we treat it as a fresh, healthy baseline.
pub const CONNECTION_STABLE_DWELL_MS: u64 = 15_000;

/// Maximum number of retries for API requests
pub const MAX_REQUEST_RETRIES: u8 = 3;

/// Delay before re-PUTting a contract when subscription fails (contract not found on network)
pub const REPUT_DELAY_MS: u64 = 20000;

/// Timeout for pending invitation GET requests (ms).
/// Must be > Freenet's OPERATION_TTL (60s) to avoid premature retry.
pub const INVITATION_TIMEOUT_MS: u64 = 90_000;

// --- WebSocket liveness watchdog (freenet/river#382) ---

/// How often the liveness watchdog wakes to evaluate connection health (ms).
/// Finer than the probe timeout so the timeout is checked promptly once armed.
pub const WATCHDOG_TICK_MS: u64 = 10_000;

/// Silence window (ms with no inbound WS traffic) before the watchdog sends a
/// liveness probe. A healthy but idle room receives no updates, so silence
/// alone is not proof of death — this only decides *when to actively probe*.
/// Kept comfortably above normal update cadence so an active room never probes.
pub const LIVENESS_IDLE_PROBE_MS: u64 = 60_000;

/// How long (ms) the watchdog waits for a liveness probe to be answered before
/// treating the socket as dead. The probe is a `Get` of an already-subscribed
/// room, which the node answers from its local fresh copy, so this is far
/// larger than the real round-trip and a momentarily slow node can't trigger a
/// false reconnect.
pub const LIVENESS_PROBE_TIMEOUT_MS: u64 = 20_000;
