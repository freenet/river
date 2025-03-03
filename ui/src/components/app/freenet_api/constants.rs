/// WebSocket URL for connecting to local Freenet node
pub const WEBSOCKET_URL: &str = "ws://localhost:50509/v1/contract/command?encodingProtocol=native";

/// Default timeout for WebSocket connection in milliseconds
pub const CONNECTION_TIMEOUT_MS: u64 = 5000;

/// Delay after PUT before subscribing to a room in milliseconds
pub const POST_PUT_DELAY_MS: u64 = 1000;

/// Retry interval for reconnection attempts in milliseconds
pub const RECONNECT_INTERVAL_MS: u64 = 3000;

/// Maximum number of retries for API requests
pub const MAX_REQUEST_RETRIES: u8 = 3;
