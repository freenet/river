//! The WebSocket connection to the local Freenet node.
//!
//! Two things are kept apart here. [`control`] is what the synchronizer and the
//! watchdog decide with (is a connection up? which attempt does a socket
//! callback belong to?), updated synchronously at the moment of each decision.
//! `SYNC_STATUS` and the retained error report are only what the pill shows,
//! written through [`set_sync_status`](crate::components::app::set_sync_status),
//! which defers. A deferred display write therefore never delays a decision,
//! and a callback from a replaced socket can change neither.

/// Connection control state: the current connection attempt and its phase.
pub mod control {
    use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

    /// Identifies one call of `initialize_connection`, allocated before its
    /// socket exists, so every callback that socket fires can say which
    /// attempt it belongs to.
    pub type AttemptId = u64;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Phase {
        Disconnected,
        Connecting,
        Connected,
        /// The attempt (or its socket) reported an error. Not connected: the
        /// next room pass reconnects, as a status of `Error` always made it do.
        Failed,
    }

    impl Phase {
        const fn to_u8(self) -> u8 {
            match self {
                Phase::Disconnected => 0,
                Phase::Connecting => 1,
                Phase::Connected => 2,
                Phase::Failed => 3,
            }
        }

        fn from_u8(v: u8) -> Self {
            match v {
                1 => Phase::Connecting,
                2 => Phase::Connected,
                3 => Phase::Failed,
                _ => Phase::Disconnected,
            }
        }
    }

    /// Atomics so it is usable from callbacks without a borrow; the UI is
    /// single-threaded, so `SeqCst` costs nothing and keeps reasoning simple.
    pub struct ConnectionControl {
        attempt: AtomicU64,
        phase: AtomicU8,
    }

    // Attempts begin and end only in the WASM connection manager.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    impl ConnectionControl {
        pub const fn new() -> Self {
            Self {
                attempt: AtomicU64::new(0),
                phase: AtomicU8::new(Phase::Disconnected.to_u8()),
            }
        }

        /// A new attempt begins: it becomes the current one, `Connecting`.
        pub fn begin_attempt(&self) -> AttemptId {
            let id = self.attempt.fetch_add(1, Ordering::SeqCst) + 1;
            self.phase
                .store(Phase::Connecting.to_u8(), Ordering::SeqCst);
            id
        }

        pub fn current_attempt(&self) -> AttemptId {
            self.attempt.load(Ordering::SeqCst)
        }

        pub fn is_current(&self, attempt: AttemptId) -> bool {
            self.current_attempt() == attempt
        }

        pub fn phase(&self) -> Phase {
            Phase::from_u8(self.phase.load(Ordering::SeqCst))
        }

        pub fn is_connected(&self) -> bool {
            self.phase() == Phase::Connected
        }

        /// A decision about `attempt` (its socket's callback, its timeout):
        /// applies only while that attempt is current. Returns whether it did.
        pub fn set_phase_for(&self, attempt: AttemptId, phase: Phase) -> bool {
            if !self.is_current(attempt) {
                return false;
            }
            self.phase.store(phase.to_u8(), Ordering::SeqCst);
            true
        }

        /// A decision the message loop makes about whatever connection is
        /// current (a loss it is acting on, an unsupported operation).
        pub fn set_phase(&self, phase: Phase) {
            self.phase.store(phase.to_u8(), Ordering::SeqCst);
        }
    }

    impl Default for ConnectionControl {
        fn default() -> Self {
            Self::new()
        }
    }

    /// Which report a failure of the current attempt shows, given its phase
    /// just before. An attempt's FIRST failure explains it: the close that
    /// follows the connect timeout (which closes the socket), and the timeout
    /// that follows a refused socket's close, keep it. A close before the
    /// socket ever opened is a failed connect, not a dropped connection.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))] // the WASM socket callbacks only
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum FailureReport {
        /// Keep the report already shown for this attempt.
        Keep,
        /// "WebSocket connection failed or timed out".
        ConnectFailed,
        /// The report for the error itself ("Connection closed", …).
        AsReported,
    }

    /// For an error or close from the attempt's socket.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))] // the WASM socket callbacks only
    fn socket_failure_report(phase_before: Phase, is_close: bool) -> FailureReport {
        match (phase_before, is_close) {
            (Phase::Failed, true) => FailureReport::Keep,
            (Phase::Connecting, true) => FailureReport::ConnectFailed,
            _ => FailureReport::AsReported,
        }
    }

    /// What an error or close from `attempt`'s socket does. A replaced
    /// attempt's does nothing at all: no phase change, no report, no loss.
    /// Otherwise the attempt is `Failed` at once, `report` is asked for the
    /// report [`socket_failure_report`] chooses (never with `Keep`), and a
    /// close (`is_close`, read from the error's structure) queues one
    /// `lose`, even when the report is kept. Returns whether the failure was
    /// the current attempt's.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))] // the WASM socket callbacks only
    pub(super) fn dispatch_socket_failure(
        control: &ConnectionControl,
        attempt: AttemptId,
        is_close: bool,
        report: impl FnOnce(FailureReport),
        lose: impl FnOnce(),
    ) -> bool {
        let phase_before = control.phase();
        if !control.set_phase_for(attempt, Phase::Failed) {
            return false;
        }
        match socket_failure_report(phase_before, is_close) {
            FailureReport::Keep => {}
            chosen => report(chosen),
        }
        if is_close {
            lose();
        }
        true
    }

    /// For the connect timeout.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))] // the WASM socket callbacks only
    pub(super) fn timeout_failure_report(phase_before: Phase) -> FailureReport {
        if phase_before == Phase::Failed {
            FailureReport::Keep
        } else {
            FailureReport::ConnectFailed
        }
    }

    static CONTROL: ConnectionControl = ConnectionControl::new();

    /// The page's connection control.
    pub fn get() -> &'static ConnectionControl {
        &CONTROL
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_replaced_attempt_cannot_change_the_phase() {
            let control = ConnectionControl::new();
            assert_eq!(control.phase(), Phase::Disconnected);
            let first = control.begin_attempt();
            assert_eq!(control.phase(), Phase::Connecting);
            assert!(control.set_phase_for(first, Phase::Failed));
            let second = control.begin_attempt();
            assert_ne!(first, second);
            assert!(control.set_phase_for(second, Phase::Connected));
            assert!(control.is_connected());
            // The first socket's late close or timeout.
            assert!(!control.set_phase_for(first, Phase::Failed));
            assert!(
                control.is_connected(),
                "a stale callback took the connection down"
            );
        }

        #[test]
        fn an_attempts_first_failure_explains_it() {
            // Refused: the socket closes before it opens, then the timeout.
            assert_eq!(
                socket_failure_report(Phase::Connecting, true),
                FailureReport::ConnectFailed
            );
            assert_eq!(timeout_failure_report(Phase::Failed), FailureReport::Keep);
            // Hung: the timeout, then the close it causes.
            assert_eq!(
                timeout_failure_report(Phase::Connecting),
                FailureReport::ConnectFailed
            );
            assert_eq!(
                socket_failure_report(Phase::Failed, true),
                FailureReport::Keep
            );
            // A connection that was up and then closed.
            assert_eq!(
                socket_failure_report(Phase::Connected, true),
                FailureReport::AsReported
            );
            // Any other socket error says what it was.
            assert_eq!(
                socket_failure_report(Phase::Connected, false),
                FailureReport::AsReported
            );
            assert_eq!(
                socket_failure_report(Phase::Failed, false),
                FailureReport::AsReported
            );
        }

        /// What one socket failure did: the reports it asked for, the
        /// losses it queued, and whether it was the current attempt's.
        #[derive(Debug, PartialEq)]
        struct Effects {
            reports: Vec<FailureReport>,
            losses: usize,
            applied: bool,
        }

        /// Feeds a browser `WebApi` payload, analysed as production analyses
        /// it, to the production dispatcher.
        fn fail(
            control: &ConnectionControl,
            attempt: AttemptId,
            payload: serde_json::Value,
        ) -> Effects {
            let parts = crate::components::app::freenet_api::error_report::analyse_browser_payload(
                &payload,
            );
            let mut reports = Vec::new();
            let mut losses = 0;
            let applied = dispatch_socket_failure(
                control,
                attempt,
                parts.is_connection_closed,
                |report| reports.push(report),
                || losses += 1,
            );
            Effects {
                reports,
                losses,
                applied,
            }
        }

        fn close() -> serde_json::Value {
            serde_json::json!({"error": "connection closed", "source": "close"})
        }

        fn connected() -> (ConnectionControl, AttemptId) {
            let control = ConnectionControl::new();
            let attempt = control.begin_attempt();
            assert!(control.set_phase_for(attempt, Phase::Connected));
            (control, attempt)
        }

        #[test]
        fn a_request_that_mentions_a_close_queues_no_loss() {
            let (control, attempt) = connected();
            let effects = fail(
                &control,
                attempt,
                serde_json::json!({
                    "error": "WebSocket is not open (state: CLOSED)",
                    "origin": "send precondition check",
                    "request": "ContractOp(Put { state: \"connection closed\" })",
                }),
            );
            assert_eq!(
                effects,
                Effects {
                    reports: vec![FailureReport::AsReported],
                    losses: 0,
                    applied: true,
                }
            );
            assert_eq!(control.phase(), Phase::Failed);
        }

        #[test]
        fn a_close_of_the_current_connection_queues_one_loss() {
            let (control, attempt) = connected();
            assert_eq!(
                fail(&control, attempt, close()),
                Effects {
                    reports: vec![FailureReport::AsReported],
                    losses: 1,
                    applied: true,
                }
            );
            assert_eq!(control.phase(), Phase::Failed);
        }

        #[test]
        fn a_close_before_the_socket_opened_is_a_failed_connect() {
            let control = ConnectionControl::new();
            let attempt = control.begin_attempt();
            assert_eq!(
                fail(&control, attempt, close()),
                Effects {
                    reports: vec![FailureReport::ConnectFailed],
                    losses: 1,
                    applied: true,
                }
            );
            assert_eq!(control.phase(), Phase::Failed);
        }

        #[test]
        fn a_close_after_a_failure_keeps_its_report_and_still_queues_a_loss() {
            let control = ConnectionControl::new();
            let attempt = control.begin_attempt();
            // The connect timeout already explained this attempt.
            assert!(control.set_phase_for(attempt, Phase::Failed));
            assert_eq!(
                fail(&control, attempt, close()),
                Effects {
                    reports: vec![],
                    losses: 1,
                    applied: true,
                }
            );
            assert_eq!(control.phase(), Phase::Failed);
        }

        #[test]
        fn a_replaced_sockets_close_has_no_effect() {
            let control = ConnectionControl::new();
            let old = control.begin_attempt();
            let current = control.begin_attempt();
            assert!(control.set_phase_for(current, Phase::Connected));
            assert_eq!(
                fail(&control, old, close()),
                Effects {
                    reports: vec![],
                    losses: 0,
                    applied: false,
                }
            );
            assert_eq!(control.phase(), Phase::Connected);
            assert!(control.is_current(current));
        }

        #[test]
        fn the_loop_decides_immediately() {
            let control = ConnectionControl::new();
            let a = control.begin_attempt();
            control.set_phase_for(a, Phase::Connected);
            control.set_phase(Phase::Disconnected);
            assert!(!control.is_connected());
            assert!(control.is_current(a), "a loss does not start an attempt");
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod imp {
    use super::control::{self, Phase};
    use crate::components::app::freenet_api::constants::*;
    use crate::components::app::freenet_api::error::SynchronizerError;
    use crate::components::app::freenet_api::error_report::{analyse_client_error, ErrorReport};
    use crate::components::app::freenet_api::freenet_synchronizer;
    use crate::components::app::freenet_api::freenet_synchronizer::SynchronizerStatus;
    use crate::components::app::{set_sync_status, StatusOrigin, AUTH_TOKEN, WEB_API};
    use crate::util::sleep;
    use dioxus::logger::tracing::{debug, error, info, warn};
    use dioxus::prelude::ReadableExt;
    use freenet_stdlib::client_api::{ClientError, HostResponse, WebApi};
    use futures::channel::mpsc::UnboundedSender;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use wasm_bindgen_futures::spawn_local;

    /// Error prefix sent by freenet-core when auth token is invalid/stale.
    /// This typically happens after a node restart when the in-memory token map is cleared.
    const AUTH_TOKEN_INVALID_ERROR: &str = "AUTH_TOKEN_INVALID";

    /// Guard to prevent multiple page reload tasks from being spawned.
    /// Once a reload is scheduled, subsequent AUTH_TOKEN_INVALID errors are ignored.
    static RELOAD_SCHEDULED: AtomicBool = AtomicBool::new(false);

    /// Manages the connection to the Freenet node
    pub struct ConnectionManager;

    impl ConnectionManager {
        pub fn new() -> Self {
            info!("Creating new ConnectionManager");
            Self
        }

        /// Whether the current attempt is connected. Reads the connection
        /// control state, never the display: the pill's status is written a
        /// macrotask later.
        pub fn is_connected(&self) -> bool {
            control::get().is_connected()
        }

        /// Initializes the connection to the Freenet node
        pub async fn initialize_connection(
            &mut self,
            message_tx: UnboundedSender<freenet_synchronizer::SynchronizerMessage>,
        ) -> Result<(), SynchronizerError> {
            // Get auth token to add as query parameter
            let auth_token = AUTH_TOKEN.read().clone();
            let base_url = get_websocket_url();
            let websocket_url = if let Some(token) = auth_token {
                if base_url.contains('?') {
                    format!("{}&authToken={}", base_url, token)
                } else {
                    format!("{}?authToken={}", base_url, token)
                }
            } else {
                base_url
            };

            let log_url = websocket_url.split('?').next().unwrap_or(&websocket_url);
            info!("Connecting to Freenet node at: {}", log_url);
            // Allocated before the socket exists, so every callback it fires
            // (and the timeout below) is tied to this attempt and cannot
            // change a newer one's state.
            let attempt = control::get().begin_attempt();
            let origin = StatusOrigin::Attempt(attempt);
            set_sync_status(SynchronizerStatus::Connecting, origin);

            info!("Connecting to WebSocket URL: {}", log_url);
            let websocket = web_sys::WebSocket::new(&websocket_url).map_err(|e| {
                let error_msg = format!("Failed to create WebSocket: {:?}", e);
                error!("{}", error_msg);
                control::get().set_phase_for(attempt, Phase::Failed);
                // The exception can echo the URL and its auth token: the
                // copyable report names only the endpoint.
                set_sync_status(
                    SynchronizerStatus::Error(ErrorReport::websocket_creation_failed(log_url)),
                    origin,
                );
                SynchronizerError::WebSocketError(error_msg)
            })?;

            // Create a simple oneshot channel for connection readiness
            let (ready_tx, ready_rx) = futures::channel::oneshot::channel();
            let message_tx_clone = message_tx.clone();

            info!("Starting WebAPI with callbacks");

            let web_api = WebApi::start(
                websocket.clone(),
                move |result: Result<HostResponse, ClientError>| {
                    // Every inbound message — success OR error — proves the
                    // socket delivered bytes, so it feeds the liveness watchdog
                    // (freenet/river#382). Recorded before any early return.
                    crate::components::app::freenet_api::connection_watchdog::record_ws_activity();

                    // Check for AUTH_TOKEN_INVALID error - this means the node was restarted
                    // and we need to refresh the page to get a new valid token
                    if let Err(ref e) = result {
                        let error_str = e.to_string();
                        if error_str.contains(AUTH_TOKEN_INVALID_ERROR) {
                            // Guard against multiple reload tasks being spawned
                            if RELOAD_SCHEDULED.swap(true, Ordering::SeqCst) {
                                info!("Page reload already scheduled, ignoring duplicate AUTH_TOKEN_INVALID");
                                return;
                            }
                            warn!(
                                "Auth token is no longer valid (node may have restarted). \
                                 Scheduling page refresh to get a new token."
                            );
                            // Don't reload immediately - this would interrupt any pending
                            // async operations like delegate registration. Instead, schedule
                            // the reload with a small delay to allow current operations to
                            // complete or fail gracefully.
                            let error_str = error_str.clone();
                            spawn_local(async move {
                                // Check localStorage to prevent reload loops: if we
                                // reloaded recently (within 10s) and still get
                                // AUTH_TOKEN_INVALID, the cache is likely serving stale
                                // HTML. Stop reloading and tell the user to refresh.
                                if let Some(window) = web_sys::window() {
                                    if let Ok(Some(storage)) = window.local_storage() {
                                        let now = js_sys::Date::now();
                                        if let Ok(Some(last)) =
                                            storage.get_item("river_last_auth_reload")
                                        {
                                            if let Ok(ts) = last.parse::<f64>() {
                                                if now - ts < 10_000.0 {
                                                    warn!("Auth reload loop detected — stopping automatic reload");
                                                    control::get()
                                                        .set_phase_for(attempt, Phase::Failed);
                                                    set_sync_status(
                                                        SynchronizerStatus::Error(
                                                            ErrorReport::auth_reload_loop(format!(
                                                                "{error_str} (again within 10 s of an automatic reload; not reloading again)"
                                                            )),
                                                        ),
                                                        origin,
                                                    );
                                                    return;
                                                }
                                            }
                                        }
                                        let _ = storage
                                            .set_item("river_last_auth_reload", &now.to_string());
                                    }
                                }

                                control::get().set_phase_for(attempt, Phase::Failed);
                                set_sync_status(
                                    SynchronizerStatus::Error(ErrorReport::auth_expired(
                                        error_str.clone(),
                                    )),
                                    origin,
                                );
                                // Wait for pending delegate registration to complete.
                                // This is needed because set_up_chat_delegate() might be
                                // running concurrently, and we need its WebSocket messages
                                // to be sent before we trigger a page reload.
                                // 500ms should be enough for the messages to be queued and
                                // sent over the WebSocket.
                                sleep(Duration::from_millis(500)).await;
                                // Navigate with cache-busting param to ensure the browser
                                // fetches fresh HTML (with a new auth token) from the
                                // gateway instead of serving a cached copy.
                                if let Some(window) = web_sys::window() {
                                    let path = window
                                        .location()
                                        .pathname()
                                        .unwrap_or_else(|_| "/".to_string());
                                    let bust = js_sys::Date::now() as u64;
                                    let url = format!("{}?_reload={}", path, bust);
                                    if let Err(e) = window.location().set_href(&url) {
                                        error!("Failed to navigate for auth reload: {:?}", e);
                                    }
                                }
                            });
                            return; // Don't process this error further
                        }
                    }

                    let mapped_result: Result<
                        freenet_stdlib::client_api::HostResponse,
                        SynchronizerError,
                    > = result.map_err(|e| SynchronizerError::from_api_error(&e));
                    let tx = message_tx_clone.clone();
                    spawn_local(async move {
                        if let Err(e) = tx.unbounded_send(
                            freenet_synchronizer::SynchronizerMessage::ApiResponse(mapped_result),
                        ) {
                            error!("Failed to send API response: {}", e);
                        }
                    });
                },
                {
                    move |error| {
                        // A socket this page already replaced: its errors and
                        // its close belong to a connection nobody uses any
                        // more, and must not mark the current one down,
                        // report, or start another reconnect. Whether this is
                        // a close is read from the error's structure: its
                        // text can carry request contents.
                        let parts = analyse_client_error(&error);
                        let current = control::dispatch_socket_failure(
                            control::get(),
                            attempt,
                            parts.is_connection_closed,
                            move |chosen| {
                                let report = match chosen {
                                    control::FailureReport::ConnectFailed => {
                                        ErrorReport::connect_timeout(format!(
                                            "{} (closed before it opened)",
                                            parts.detail
                                        ))
                                    }
                                    _ => ErrorReport::from_parts(parts),
                                };
                                set_sync_status(SynchronizerStatus::Error(report), origin);
                            },
                            || {
                                info!("Connection closed, triggering reconnection");
                                if let Err(e) = message_tx.unbounded_send(
                                    freenet_synchronizer::SynchronizerMessage::ConnectionLost,
                                ) {
                                    error!("Failed to send ConnectionLost message: {}", e);
                                }
                            },
                        );
                        if current {
                            error!("WebSocket error: {}", error);
                        } else {
                            debug!("Ignoring an error from a replaced WebSocket");
                        }
                    }
                },
                {
                    move || {
                        info!("WebSocket connected successfully");
                        // Record a new socket generation on (re)connect. This
                        // both resets the liveness clock (so the watchdog doesn't
                        // treat a freshly-opened, still-idle socket as
                        // long-silent) AND advances the connection-generation
                        // counter that releases the watchdog's ReconnectRequested
                        // latch — proof that a reconnect actually landed, as
                        // opposed to a straggler message on the old condemned
                        // socket (freenet/river#382; Codex #384 review).
                        crate::components::app::freenet_api::connection_watchdog::record_ws_connected();
                        // Clear the reload-loop guard so a future node restart can
                        // trigger a fresh reload without being falsely detected as a loop.
                        if let Some(window) = web_sys::window() {
                            if let Ok(Some(storage)) = window.local_storage() {
                                let _ = storage.remove_item("river_last_auth_reload");
                            }
                        }
                        let _ = ready_tx.send(());
                    }
                },
            );
            // stdlib's onerror reads ErrorEvent::filename() off a plain Event and throws; the close that follows reports it.
            websocket.set_onerror(None);

            info!(
                "Waiting for connection with timeout of {}ms",
                CONNECTION_TIMEOUT_MS
            );

            let timeout_future = sleep(Duration::from_millis(CONNECTION_TIMEOUT_MS));

            let result =
                futures::future::select(Box::pin(ready_rx), Box::pin(timeout_future)).await;

            match result {
                futures::future::Either::Left((Ok(_), _)) => {
                    info!("WebSocket connection established successfully");
                    *WEB_API.write() = Some(web_api);
                    control::get().set_phase_for(attempt, Phase::Connected);
                    set_sync_status(SynchronizerStatus::Connected, origin);
                    Ok(())
                }
                _ => {
                    let error = SynchronizerError::WebSocketError(
                        "WebSocket connection failed or timed out".to_string(),
                    );
                    error!("{}", error);
                    let phase_before = control::get().phase();
                    if control::get().set_phase_for(attempt, Phase::Failed)
                        && control::timeout_failure_report(phase_before)
                            == control::FailureReport::ConnectFailed
                    {
                        set_sync_status(
                            SynchronizerStatus::Error(ErrorReport::connect_timeout(format!(
                                "{error} (no open within {CONNECTION_TIMEOUT_MS} ms)"
                            ))),
                            origin,
                        );
                    }

                    let ready_state = websocket.ready_state();
                    if ready_state == web_sys::WebSocket::CONNECTING
                        || ready_state == web_sys::WebSocket::OPEN
                    {
                        info!("Closing WebSocket due to connection failure");
                        if let Err(e) = websocket.close() {
                            error!("Failed to close WebSocket: {:?}", e);
                        }
                    }

                    Err(error)
                }
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub use imp::ConnectionManager;

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use crate::components::app::freenet_api::error::SynchronizerError;
    use crate::components::app::freenet_api::error_report::{ErrorReport, LABEL_DISCONNECTED};
    use crate::components::app::freenet_api::freenet_synchronizer;
    use crate::components::app::freenet_api::freenet_synchronizer::SynchronizerStatus;
    use crate::components::app::{set_sync_status, StatusOrigin, WEB_API};
    use dioxus::logger::tracing::warn;
    use futures::channel::mpsc::UnboundedSender;

    #[derive(Default)]
    pub struct ConnectionManager;

    impl ConnectionManager {
        pub fn new() -> Self {
            Self
        }

        pub fn is_connected(&self) -> bool {
            false
        }

        pub async fn initialize_connection(
            &mut self,
            message_tx: UnboundedSender<freenet_synchronizer::SynchronizerMessage>,
        ) -> Result<(), SynchronizerError> {
            let _ = message_tx;
            warn!("ConnectionManager::initialize_connection is a no-op on non-wasm targets");
            set_sync_status(
                SynchronizerStatus::Error(ErrorReport::from_text(
                    LABEL_DISCONNECTED,
                    "Web API connection only available when targeting wasm32",
                )),
                StatusOrigin::current(),
            );
            WEB_API.write().take();
            Err(SynchronizerError::WebSocketNotSupported(
                "River UI connection only available when targeting wasm32".into(),
            ))
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use imp::ConnectionManager;

#[cfg(test)]
mod tests {
    /// The production source between `start` and the end of the first
    /// following `end`.
    fn section<'a>(code: &'a str, start: &str, end: &str) -> &'a str {
        let from = code.find(start).unwrap_or_else(|| panic!("{start}"));
        let len = code[from..].find(end).unwrap_or_else(|| panic!("{end}"));
        &code[from..from + len + end.len()]
    }

    /// The constructor's exception can echo the authenticated URL, so the
    /// copyable report is built from the query-free `log_url` alone.
    #[test]
    fn the_constructor_failure_report_never_carries_the_exception() {
        let code = crate::util::source_scan::production_only(include_str!("connection_manager.rs"));
        let branch = section(&code, "web_sys::WebSocket::new(&websocket_url)", "})?;");
        let status = section(branch, "set_sync_status(", "origin,");
        assert!(
            status.contains("ErrorReport::websocket_creation_failed(log_url)"),
            "{status}"
        );
        for raw in [
            "error_msg",
            "ErrorReport::new",
            "{:?}",
            "(e)",
            "websocket_url",
        ] {
            assert!(!status.contains(raw), "{raw} reaches the report: {status}");
        }
    }

    /// The call starting at `start`, through its balanced closing paren.
    fn call<'a>(code: &'a str, start: &str) -> &'a str {
        let from = code.find(start).unwrap_or_else(|| panic!("{start}"));
        let mut depth = 0;
        for (i, c) in code[from..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' if depth == 1 => return &code[from..=from + i],
                ')' => depth -= 1,
                _ => {}
            }
        }
        panic!("unbalanced {start}");
    }

    /// The WASM socket-error callback reports and queues a loss only through
    /// the tested `dispatch_socket_failure`, with the close read from the
    /// error's structure, never from its text.
    #[test]
    fn the_socket_error_callback_delegates_its_effects_to_the_dispatcher() {
        let code = crate::util::source_scan::production_only(include_str!("connection_manager.rs"));
        let callback = section(&code, "move |error| {", "move || {");
        assert!(
            callback.contains("let parts = analyse_client_error(&error);"),
            "{callback}"
        );
        let dispatch = call(callback, "control::dispatch_socket_failure(");
        assert!(
            dispatch.contains("parts.is_connection_closed"),
            "{dispatch}"
        );
        for effect in ["set_sync_status(", "SynchronizerMessage::ConnectionLost"] {
            assert_eq!(callback.matches(effect).count(), 1, "{effect}: {callback}");
            assert!(
                dispatch.contains(effect),
                "{effect} happens outside the dispatcher: {callback}"
            );
        }
        for independent in [".contains(", "set_phase_for", "socket_failure_report("] {
            assert!(
                !callback.contains(independent),
                "{independent} decided outside the dispatcher: {callback}"
            );
        }
    }

    #[test]
    fn webapi_onerror_is_cleared_after_start() {
        let code = crate::util::source_scan::production_only(include_str!("connection_manager.rs"));
        let start = code.find("WebApi::start(").expect("the WebApi::start call");
        let cleared = code
            .find("websocket.set_onerror(None)")
            .expect("River clears WebApi's onerror");
        assert!(
            cleared > start,
            "onerror must be cleared after WebApi::start sets it"
        );
    }
}
