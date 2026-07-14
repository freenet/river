#![allow(dead_code)]

use super::connection_manager::ConnectionManager;
use super::error::SynchronizerError;
use super::response_handler::ResponseHandler;
use super::room_synchronizer::RoomSynchronizer;
use crate::components::app::chat_delegate::{
    mark_legacy_migration_done, reset_ensure_subscription_dedup, set_up_chat_delegate,
};
use crate::components::app::sync_info::SYNC_INFO;
use crate::components::app::{ROOMS, SYNC_STATUS, WEB_API};
use crate::util::{owner_vk_to_contract_key, safe_spawn_local, sleep};
use dioxus::logger::tracing::{debug, error, info, warn};
use dioxus::prelude::*;
use ed25519_dalek::SigningKey;
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::client_api::HostResponse;
use freenet_stdlib::prelude::OutboundDelegateMsg;
use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::StreamExt;
use river_core::room_state::member::AuthorizedMember;
use river_core::room_state::member::MemberId;
use std::time::Duration;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;

/// Compute reconnection delay with exponential backoff and ±20% jitter.
/// `consecutive_failures` is the number of failed attempts so far (0-indexed).
fn reconnect_delay_ms(consecutive_failures: u32) -> u64 {
    use super::constants::{RECONNECT_INITIAL_MS, RECONNECT_MAX_MS};

    let base = RECONNECT_INITIAL_MS.saturating_mul(1u64 << consecutive_failures.min(20));
    let capped = base.min(RECONNECT_MAX_MS);

    // Add ±20% jitter using simple WASM-compatible pseudo-random
    let jitter_range = capped / 5; // 20%
    let jitter = if jitter_range > 0 {
        // Use js_sys::Math::random() for WASM-compatible randomness
        #[cfg(target_arch = "wasm32")]
        let rand_val = (js_sys::Math::random() * (2.0 * jitter_range as f64)) as u64;
        #[cfg(not(target_arch = "wasm32"))]
        let rand_val = jitter_range; // deterministic for tests
        rand_val
    } else {
        0
    };

    (capped - jitter_range + jitter).min(RECONNECT_MAX_MS)
}

/// A scheduled reconnect: the backoff delay to wait, plus the `generation` that
/// identifies this specific arming. A delayed reconnect timer captures the
/// generation and, when it fires, drives a reconnect only if the generation is
/// still current (`ReconnectState::is_current_reconnect`). This lets an
/// immediate Connect — from PageBecameVisible/ProcessRooms, or a thawed
/// background-timer backlog firing all at once on resume — supersede an
/// already-scheduled backoff timer instead of running in parallel with it, so
/// reconnect chains can't stack (freenet/river#406).
#[derive(Clone, Copy)]
struct ArmedReconnect {
    delay_ms: u64,
    generation: u64,
}

/// Reconnect bookkeeping: the exponential-backoff failure counter, a
/// single-outstanding-reconnect latch, and a generation counter that lets a
/// stale reconnect timer detect it has been superseded. Extracted from the
/// message loop so the behaviours behind the endless Android reconnect loop
/// (freenet/river#406) are unit-testable: (1) the backoff must GROW across an
/// open-then-die flap and reset only after a connection proves stable; (2) a
/// duplicate ConnectionLost (a dropped socket's orphaned `onclose`, or the
/// watchdog and transport reporting the same death) must be coalesced, not
/// stack another reconnect; and (3) once a newer reconnect is armed, an older
/// timer that fires late must drop itself rather than launch a parallel
/// attempt.
#[derive(Default)]
struct ReconnectState {
    consecutive_failures: u32,
    reconnect_pending: bool,
    /// Bumped on every `arm()`. A delayed reconnect timer is honored only if the
    /// generation it captured still equals this value (see
    /// [`is_current_reconnect`](Self::is_current_reconnect)).
    generation: u64,
}

impl ReconnectState {
    /// A Connect is now being attempted — clear the single-pending latch.
    fn note_connect_attempt(&mut self) {
        self.reconnect_pending = false;
    }

    /// A ConnectionLost arrived. `Some(armed)` → schedule a reconnect with that
    /// delay/generation; `None` → a reconnect is already pending, so this is a
    /// coalesced duplicate loss and must be ignored.
    fn on_connection_lost(&mut self) -> Option<ArmedReconnect> {
        if self.reconnect_pending {
            None
        } else {
            Some(self.arm())
        }
    }

    /// A Connect attempt failed to open — always schedule a retry.
    fn on_connect_failed(&mut self) -> ArmedReconnect {
        self.arm()
    }

    /// A connection proved stable for the dwell window. Reset the backoff ONLY
    /// if it is still the live connection (`still_current`), so an open-then-die
    /// socket keeps backing off instead of resetting the counter every cycle.
    fn note_stable(&mut self, still_current: bool) {
        if still_current {
            self.consecutive_failures = 0;
        }
    }

    /// Whether a delayed reconnect timer tagged with `generation` should still
    /// drive a reconnect. False once a newer `arm()` has superseded it (a newer
    /// timer is already scheduled) or the pending reconnect has already been
    /// consumed — so a late-firing stale timer drops itself instead of stacking
    /// a parallel attempt (freenet/river#406).
    fn is_current_reconnect(&self, generation: u64) -> bool {
        self.reconnect_pending && self.generation == generation
    }

    /// Current backoff delay, then advance the failure count, arm the latch, and
    /// bump the generation (invalidating any previously-scheduled timer).
    fn arm(&mut self) -> ArmedReconnect {
        let delay = reconnect_delay_ms(self.consecutive_failures);
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.reconnect_pending = true;
        self.generation = self.generation.saturating_add(1);
        ArmedReconnect {
            delay_ms: delay,
            generation: self.generation,
        }
    }

    /// Human-facing attempt number for logs (1-based after arming).
    fn attempt(&self) -> u32 {
        self.consecutive_failures
    }
}

/// Message types for communicating with the synchronizer
pub enum SynchronizerMessage {
    ProcessRooms,
    Connect,
    /// Sent (delayed) by a reconnect backoff timer, tagged with the reconnect
    /// generation it was armed for. Drives a reconnect only if that generation
    /// is still current — a newer arming (e.g. from an immediate Connect after
    /// resume) supersedes it, so stale timers drop themselves instead of
    /// stacking parallel reconnect chains (freenet/river#406).
    ScheduledReconnect {
        generation: u64,
    },
    /// Sent when WebSocket connection is lost (closed or errored)
    ConnectionLost,
    /// Sent (delayed) after a Connect succeeds, tagged with the connection
    /// generation it was scheduled for. Resets the reconnect backoff only if
    /// that same connection is still up — so backoff is reset by proven
    /// stability, not merely by a socket opening (freenet/river#406).
    ConnectionStable {
        connect_seq: u64,
    },
    /// Sent when page becomes visible after being hidden (e.g., after sleep/wake)
    PageBecameVisible,
    /// Sent to refresh all room states after reconnection (e.g., after sleep/wake)
    /// This fetches current state for all rooms to catch any updates missed during suspension
    RefreshAllRooms,
    ApiResponse(Result<HostResponse, SynchronizerError>),
    AcceptInvitation {
        owner_vk: VerifyingKey,
        authorized_member: Box<AuthorizedMember>,
        invitee_signing_key: Box<SigningKey>,
        nickname: String,
    },
}

/// Manages synchronization between local room state and Freenet network
pub struct FreenetSynchronizer {
    pub message_tx: UnboundedSender<SynchronizerMessage>,
    message_rx: Option<UnboundedReceiver<SynchronizerMessage>>,
    connection_manager: ConnectionManager,
    response_handler: ResponseHandler,
    connection_ready: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SynchronizerStatus {
    Disconnected,
    Connecting,
    Connected,
    Error(String),
}

impl From<SynchronizerError> for SynchronizerStatus {
    fn from(error: SynchronizerError) -> Self {
        SynchronizerStatus::Error(error.to_string())
    }
}

impl FreenetSynchronizer {
    pub fn new() -> Self {
        let (message_tx, message_rx) = unbounded();
        let connection_manager = ConnectionManager::new();
        let room_synchronizer = RoomSynchronizer::new();
        let response_handler = ResponseHandler::new(room_synchronizer);

        info!("Creating new FreenetSynchronizer instance");

        Self {
            message_tx,
            message_rx: Some(message_rx),
            connection_manager,
            response_handler,
            connection_ready: false,
        }
    }

    pub fn get_message_sender(&self) -> UnboundedSender<SynchronizerMessage> {
        self.message_tx.clone()
    }

    pub async fn start(&mut self) {
        info!("Starting FreenetSynchronizer");
        if self.message_rx.is_none() {
            info!("FreenetSynchronizer is already running, ignoring start request");
            return;
        }

        let mut message_rx = self
            .message_rx
            .take()
            .expect("Message receiver already taken");
        let message_tx = self.message_tx.clone();

        info!("Setting up message processing loop");

        let mut connection_manager = ConnectionManager::new();
        let room_synchronizer_ref = self.response_handler.get_room_synchronizer();
        let mut response_handler =
            ResponseHandler::new_with_shared_synchronizer(room_synchronizer_ref);

        info!("Starting message processing loop");
        spawn_local(async move {
            // Set up Page Visibility API listener to detect sleep/wake cycles
            // When computer wakes from sleep, we need to check if connection is still alive
            let visibility_tx = message_tx.clone();
            if let Some(window) = web_sys::window() {
                if let Some(document) = window.document() {
                    let callback = Closure::<dyn Fn()>::new(move || {
                        if let Some(window) = web_sys::window() {
                            if let Some(document) = window.document() {
                                if document.visibility_state() == web_sys::VisibilityState::Visible
                                {
                                    info!("Page became visible, checking connection health");
                                    if let Err(e) = visibility_tx
                                        .unbounded_send(SynchronizerMessage::PageBecameVisible)
                                    {
                                        error!("Failed to send PageBecameVisible message: {}", e);
                                    }
                                }
                            }
                        }
                    });
                    if let Err(e) = document.add_event_listener_with_callback(
                        "visibilitychange",
                        callback.as_ref().unchecked_ref(),
                    ) {
                        error!("Failed to add visibility change listener: {:?}", e);
                    }
                    // Keep the closure alive for the lifetime of the app
                    callback.forget();
                    info!("Page Visibility listener installed for sleep/wake detection");
                }
            }

            // Start the WebSocket liveness watchdog (freenet/river#382). It
            // detects a half-open / silently-dead socket — one the browser
            // never reports as closed/errored — and raises ConnectionLost so
            // the normal reconnect + re-subscribe + re-GET path recovers the
            // frozen live-update stream. Dormant unless SYNC_STATUS==Connected,
            // so it can't add reconnect pressure while already reconnecting.
            super::connection_watchdog::spawn_liveness_watchdog(message_tx.clone());

            info!("Sending initial Connect message");
            if let Err(e) = message_tx.unbounded_send(SynchronizerMessage::Connect) {
                error!("Failed to send Connect message: {}", e);
            }

            let mut reconnect = ReconnectState::default();

            // Spawn the delayed reconnect (the async setTimeout part; the backoff
            // math + pending latch + generation live in `ReconnectState`). The
            // timer sends `ScheduledReconnect { generation }` rather than a bare
            // Connect, so a stale timer (superseded by a newer arming) drops
            // itself in the handler instead of stacking a parallel reconnect
            // (freenet/river#406). `safe_spawn_local` defers the spawn via
            // setTimeout(0) so spawning from inside this polled loop future can't
            // re-enter wasm-bindgen's task scheduler and panic on Firefox mobile
            // (mirrors the watchdog's own convention).
            let spawn_reconnect =
                |armed: ArmedReconnect, tx: &UnboundedSender<SynchronizerMessage>| {
                    let tx = tx.clone();
                    safe_spawn_local(async move {
                        sleep(Duration::from_millis(armed.delay_ms)).await;
                        if let Err(e) = tx.unbounded_send(SynchronizerMessage::ScheduledReconnect {
                            generation: armed.generation,
                        }) {
                            error!("Failed to send reconnect message: {}", e);
                        }
                    });
                };

            info!("Entering message loop");
            while let Some(msg) = message_rx.next().await {
                match msg {
                    SynchronizerMessage::ProcessRooms => {
                        info!("DEBUG: ProcessRooms message received in synchronizer");
                        info!("Processing rooms request received");
                        if !connection_manager.is_connected() {
                            info!("Connection not ready, deferring room processing and attempting to connect");
                            if let Err(e) = message_tx.unbounded_send(SynchronizerMessage::Connect)
                            {
                                error!("Failed to send Connect message: {}", e);
                            }
                            continue;
                        }
                        info!("Connection is ready, processing rooms");
                        if let Err(e) = response_handler
                            .get_room_synchronizer_mut()
                            .process_rooms()
                            .await
                        {
                            error!("Error processing rooms: {}", e);
                            // Check if this is a WebSocket error that needs reconnection
                            let error_str = e.to_string();
                            if error_str.contains("WebSocket") || error_str.contains("not open") {
                                warn!("WebSocket error during room processing, triggering reconnection");
                                if let Err(e) =
                                    message_tx.unbounded_send(SynchronizerMessage::ConnectionLost)
                                {
                                    error!("Failed to send ConnectionLost: {}", e);
                                }
                            }
                        } else {
                            info!("Successfully processed rooms");
                        }
                    }
                    SynchronizerMessage::ConnectionLost => {
                        // Coalesce: if a reconnect is already scheduled this is a
                        // duplicate loss — the socket we're about to drop will
                        // itself fire `onclose` → another ConnectionLost, and the
                        // watchdog and transport can both report the same death.
                        // Acting again would stack extra reconnects and inflate
                        // the backoff, part of the endless loop (freenet/river#406).
                        match reconnect.on_connection_lost() {
                            None => {
                                info!("ConnectionLost ignored — a reconnect is already pending");
                            }
                            Some(armed) => {
                                // Clear the web API so is_connected() returns false
                                WEB_API.write().take();
                                *SYNC_STATUS.write() = SynchronizerStatus::Disconnected;
                                warn!(
                                    "Connection lost (attempt {}), reconnecting in {}ms",
                                    reconnect.attempt(),
                                    armed.delay_ms
                                );
                                spawn_reconnect(armed, &message_tx);
                            }
                        }
                    }
                    SynchronizerMessage::ScheduledReconnect { generation } => {
                        // A backoff timer fired. Honor it only if it is still the
                        // current reconnect generation; a newer arming (typically
                        // an immediate Connect after resume that itself failed, or
                        // another loss) supersedes it, so an older timer that fires
                        // late drops itself here instead of launching a second,
                        // parallel reconnect chain (freenet/river#406). When
                        // current, funnel through the normal Connect path so the
                        // connect/dedup logic lives in one place.
                        if reconnect.is_current_reconnect(generation) {
                            if let Err(e) = message_tx.unbounded_send(SynchronizerMessage::Connect)
                            {
                                error!("Failed to send Connect from ScheduledReconnect: {}", e);
                            }
                        } else {
                            info!(
                                "Stale reconnect timer (generation {}) dropped — superseded by a newer attempt",
                                generation
                            );
                        }
                    }
                    SynchronizerMessage::ConnectionStable { connect_seq } => {
                        // The connection scheduled at `connect_seq` has now stayed
                        // up for the dwell window. Reset the backoff ONLY if this
                        // is still the live connection — a flap that reconnected
                        // meanwhile bumped `ws_connect_seq`, so its own (later)
                        // ConnectionStable governs the reset. This is what makes a
                        // socket that opens-then-dies keep backing off instead of
                        // resetting the counter every cycle (freenet/river#406).
                        let still_current = connection_manager.is_connected()
                            && super::connection_watchdog::ws_connect_seq() == connect_seq;
                        if still_current {
                            info!(
                                "Connection stable for dwell window; resetting reconnect backoff"
                            );
                        }
                        reconnect.note_stable(still_current);
                    }
                    SynchronizerMessage::PageBecameVisible => {
                        // Page became visible after being hidden (e.g., after sleep/wake)
                        // Check if we're still connected, if not trigger reconnection
                        info!("Page visibility changed to visible, checking connection status");
                        if !connection_manager.is_connected() {
                            // Send Connect for an immediate reconnect attempt (it
                            // runs now rather than waiting out any pending backoff
                            // timer). Do NOT reset the backoff here: eagerly zeroing
                            // it on every resume was part of the never-terminating
                            // loop. This immediate Connect also bumps the reconnect
                            // generation if it fails, so any backoff timer already
                            // scheduled for the old generation drops itself when it
                            // fires (see `ScheduledReconnect`) rather than adding a
                            // parallel attempt; the dwell resets the backoff once a
                            // connection proves stable (freenet/river#406).
                            info!("Connection is not active after wake, triggering reconnection");
                            if let Err(e) = message_tx.unbounded_send(SynchronizerMessage::Connect)
                            {
                                error!("Failed to send Connect message after wake: {}", e);
                            }
                        } else {
                            // Connection appears active, but we may have missed updates during suspension.
                            // First verify connection with ProcessRooms, then refresh all rooms to
                            // catch any updates that arrived while the page was hidden/PC was suspended.
                            info!("Connection appears active, refreshing all rooms to catch missed updates");
                            if let Err(e) =
                                message_tx.unbounded_send(SynchronizerMessage::RefreshAllRooms)
                            {
                                error!("Failed to send RefreshAllRooms message after wake: {}", e);
                            }
                        }
                    }
                    SynchronizerMessage::RefreshAllRooms => {
                        // Refresh all room states by sending GET requests
                        // This catches any updates missed during PC suspension or page being hidden
                        info!("Refreshing all rooms to catch missed updates");
                        if !connection_manager.is_connected() {
                            info!(
                                "Connection not ready, deferring refresh and attempting to connect"
                            );
                            if let Err(e) = message_tx.unbounded_send(SynchronizerMessage::Connect)
                            {
                                error!("Failed to send Connect message: {}", e);
                            }
                            continue;
                        }
                        // This is the sleep/wake path where the WebSocket *appears* alive
                        // (SYNC_STATUS == Connected) so we refresh rather than reconnect —
                        // but the node-side contract subscriptions may have been dropped
                        // while suspended. Re-arm the per-session EnsureRoomSubscription
                        // dedup and re-run the load-rooms pass so owned private rooms are
                        // re-subscribed; otherwise an entry left "done" before suspension
                        // would never re-fire on this refresh-only wake path, leaving the
                        // delegate unsubscribed (freenet/river#277). Same reasoning as the
                        // Connect handler — this path does not route through Connect.
                        reset_ensure_subscription_dedup();
                        if WEB_API.read().is_some() {
                            if let Err(e) = set_up_chat_delegate().await {
                                error!(
                                    "Failed to set up chat delegate on refresh wake path: {}",
                                    e
                                );
                            }
                        }
                        if let Err(e) = response_handler
                            .get_room_synchronizer_mut()
                            .refresh_all_rooms()
                            .await
                        {
                            error!("Error refreshing rooms: {}", e);
                            // Check if this is a WebSocket error that needs reconnection
                            let error_str = e.to_string();
                            if error_str.contains("WebSocket") || error_str.contains("not open") {
                                warn!(
                                    "WebSocket error during room refresh, triggering reconnection"
                                );
                                if let Err(e) =
                                    message_tx.unbounded_send(SynchronizerMessage::ConnectionLost)
                                {
                                    error!("Failed to send ConnectionLost: {}", e);
                                }
                            }
                        } else {
                            info!("Successfully refreshed all rooms");
                        }
                    }
                    SynchronizerMessage::Connect => {
                        // A reconnect (or the first connect) is now being
                        // attempted; clear the pending latch so the next loss can
                        // arm a fresh reconnect. A redundant Connect — from
                        // repeated PageBecameVisible, a frozen background-timer
                        // backlog that all fires at once on resume, or the
                        // ProcessRooms/RefreshAllRooms no-delay Connect paths —
                        // must NOT tear down a live connection: initialize_connection
                        // would overwrite a live WEB_API, dropping its socket, whose
                        // orphaned `onclose` then injects a spurious ConnectionLost
                        // and self-sustains the loop (freenet/river#406). Skip when
                        // we already have a live, usable connection. Both conditions
                        // are required: SYNC_STATUS and WEB_API are written by
                        // separate deferred tasks, so a late `onopen` can leave
                        // `is_connected()` true while WEB_API is still None (a
                        // half-open zombie). Gating on WEB_API too keeps the skip
                        // self-contained rather than relying on a later onclose to
                        // re-assert the disconnect.
                        reconnect.note_connect_attempt();
                        if connection_manager.is_connected() && WEB_API.read().is_some() {
                            info!("Connect ignored — already connected");
                            continue;
                        }
                        info!("Connecting to Freenet");
                        match connection_manager
                            .initialize_connection(message_tx.clone())
                            .await
                        {
                            Ok(()) => {
                                info!("Connection established successfully");
                                // Do NOT reset the backoff on mere socket-open — an
                                // open-then-die socket would reset it every cycle,
                                // defeating the exponential backoff. Instead arm a
                                // dwell check tagged with this connection's
                                // generation; the backoff resets only if this same
                                // connection is still up after the dwell window
                                // (freenet/river#406).
                                let connect_seq = super::connection_watchdog::ws_connect_seq();
                                let stable_tx = message_tx.clone();
                                // `safe_spawn_local` (not raw `spawn_local`) defers
                                // the spawn via setTimeout(0): spawning from inside
                                // this polled loop future can otherwise re-enter
                                // wasm-bindgen's task scheduler and panic on Firefox
                                // mobile — the same convention the liveness watchdog
                                // uses for exactly this reason.
                                safe_spawn_local(async move {
                                    sleep(Duration::from_millis(
                                        super::constants::CONNECTION_STABLE_DWELL_MS,
                                    ))
                                    .await;
                                    if let Err(e) = stable_tx.unbounded_send(
                                        SynchronizerMessage::ConnectionStable { connect_seq },
                                    ) {
                                        error!("Failed to send ConnectionStable: {}", e);
                                    }
                                });
                                // Check if web API is available without holding the lock
                                // during process_rooms() call
                                let api_available = WEB_API.read().is_some();
                                if api_available {
                                    // Re-arm the per-session EnsureRoomSubscription dedup
                                    // before the load-rooms pass below re-subscribes owned
                                    // private rooms. The dedup set is process-global and
                                    // survives a session, but the node-level contract
                                    // subscriptions it tracks die with the WebSocket. Without
                                    // this, a room whose EnsureRoomSubscription succeeded
                                    // before a transport blip stays marked "done", so
                                    // set_up_chat_delegate's load-rooms pass returns
                                    // Ok(false) (dedup hit) and the delegate is never
                                    // re-subscribed — silently disabling delegate-driven
                                    // secret rotation until tab reload (freenet/river#277).
                                    // Clearing here (rather than in ConnectionLost) covers
                                    // every reconnect that routes through Connect: a real
                                    // transport drop (ConnectionLost -> Connect) and the
                                    // sleep/wake PageBecameVisible health check when the
                                    // connection is found dead (-> Connect). The OTHER wake
                                    // path — PageBecameVisible with an apparently-live
                                    // connection -> RefreshAllRooms — re-arms separately in
                                    // the RefreshAllRooms handler, since it does not route
                                    // through Connect. It is a no-op on the first connection
                                    // (the set is already empty), and only re-arms retry — it
                                    // does not re-introduce the retry storm PR #276 fixed (the
                                    // re-subscribe still flows through the once-per-reconnect
                                    // load-rooms path, gated on a non-Failed signing-key
                                    // migration).
                                    reset_ensure_subscription_dedup();
                                    // Set up the chat delegate to load rooms from storage
                                    if let Err(e) = set_up_chat_delegate().await {
                                        error!("Failed to set up chat delegate: {}", e);
                                    }

                                    info!("Processing rooms after successful connection");
                                    if let Err(e) = response_handler
                                        .get_room_synchronizer_mut()
                                        .process_rooms()
                                        .await
                                    {
                                        error!("Error processing rooms after connection: {}", e);
                                    } else {
                                        info!("Successfully processed rooms after connection");
                                    }
                                } else {
                                    error!("API not available after successful connection");
                                }
                            }
                            Err(e) => {
                                error!("Failed to initialize connection: {}", e);
                                let armed = reconnect.on_connect_failed();
                                warn!(
                                    "Connection failed (attempt {}), reconnecting in {}ms",
                                    reconnect.attempt(),
                                    armed.delay_ms
                                );
                                spawn_reconnect(armed, &message_tx);
                            }
                        }
                    }
                    SynchronizerMessage::ApiResponse(response) => {
                        info!("Received API response");
                        match response {
                            Ok(host_response) => {
                                info!("Processing valid API response");

                                // Log more details based on response type
                                match &host_response {
                                    HostResponse::DelegateResponse { key, values } => {
                                        info!(
                                            "Delegate response with key: {:?}, values count: {}",
                                            key,
                                            values.len()
                                        );
                                        for (i, v) in values.iter().enumerate() {
                                            match v {
                                                OutboundDelegateMsg::ApplicationMessage(
                                                    app_msg,
                                                ) => {
                                                    info!("Value #{} is ApplicationMessage, processed: {}, payload size: {}",
                                                          i, app_msg.processed, app_msg.payload.len());
                                                }
                                                _ => debug!("Value #{} is: {:?}", i, v),
                                            }
                                        }
                                    }
                                    _ => debug!("Other response type: {:?}", host_response),
                                }

                                match response_handler.handle_api_response(host_response).await {
                                    Ok(flags) => {
                                        if flags.needs_reput {
                                            // Subscription failed but we have local state - schedule a re-PUT
                                            info!("Scheduling re-PUT after subscription failure (waiting {}ms)",
                                                  super::constants::REPUT_DELAY_MS);
                                            let tx = message_tx.clone();
                                            spawn_local(async move {
                                                sleep(Duration::from_millis(
                                                    super::constants::REPUT_DELAY_MS,
                                                ))
                                                .await;
                                                info!("Re-PUT delay elapsed, triggering ProcessRooms to PUT contract");
                                                if let Err(e) = tx.unbounded_send(
                                                    SynchronizerMessage::ProcessRooms,
                                                ) {
                                                    error!("Failed to schedule re-PUT: {}", e);
                                                }
                                            });
                                        }
                                        if flags.subscriptions_initiated {
                                            // Subscriptions were initiated - schedule a timeout check
                                            info!("Scheduling subscription timeout check (waiting {}ms)",
                                                  super::constants::REPUT_DELAY_MS);
                                            let tx = message_tx.clone();
                                            spawn_local(async move {
                                                sleep(Duration::from_millis(
                                                    super::constants::REPUT_DELAY_MS + 1000, // Add 1s buffer
                                                ))
                                                .await;
                                                info!("Subscription timeout check triggered");
                                                if let Err(e) = tx.unbounded_send(
                                                    SynchronizerMessage::ProcessRooms,
                                                ) {
                                                    error!(
                                                        "Failed to schedule timeout check: {}",
                                                        e
                                                    );
                                                }
                                            });
                                        }
                                    }
                                    Err(e) => {
                                        error!("Error handling API response: {}", e);
                                    }
                                }
                                info!("Finished processing API response");
                            }
                            Err(e) => {
                                error!("Received error in API response: {}", e);

                                // "delegate X not found in store" errors from legacy migration
                                // requests should permanently mark migration as done. The old
                                // delegate WASM was never installed on this node, so retrying
                                // across sessions will never succeed.
                                if e.to_string().contains("delegate")
                                    && e.to_string().contains("not found")
                                {
                                    info!("Delegate not found error (likely legacy migration) - marking migration complete");
                                    mark_legacy_migration_done();
                                }

                                // Special handling for "not supported" errors
                                if e.to_string().contains("not supported") {
                                    warn!("Detected 'not supported' WebSocket operation. This may indicate API version mismatch.");
                                    // Don't immediately reconnect for this specific error as it's likely to recur
                                    *SYNC_STATUS.write() = SynchronizerStatus::Error(
                                        "WebSocket API operation not supported. Check server compatibility.".to_string()
                                    );
                                    continue;
                                }

                                // Log more details about the error
                                if e.to_string().contains("contract")
                                    && e.to_string().contains("not found")
                                {
                                    let error_msg = e.to_string();
                                    if let Some(contract_id) = error_msg
                                        .split_whitespace()
                                        .find(|&word| word.len() > 30 && !word.contains(':'))
                                    {
                                        info!(
                                            "Contract not found error for contract ID: {}",
                                            contract_id
                                        );

                                        // Check if this contract ID exists in our rooms
                                        // Collect room information first to avoid nested borrows
                                        let room_matches: Vec<(VerifyingKey, String)> = {
                                            let rooms = ROOMS.read();
                                            rooms
                                                .map
                                                .keys()
                                                .map(|room_key| {
                                                    let contract_key =
                                                        owner_vk_to_contract_key(room_key);
                                                    let room_contract_id = contract_key.id();
                                                    (*room_key, room_contract_id.to_string())
                                                })
                                                .collect()
                                        };

                                        let mut found = false;
                                        let mut matching_rooms = Vec::new();

                                        for (room_key, room_contract_id) in &room_matches {
                                            if room_contract_id == contract_id {
                                                info!("Contract ID {} matches room with owner key: {:?}", 
                                                      contract_id, MemberId::from(*room_key));
                                                found = true;
                                                matching_rooms.push(*room_key);
                                            }
                                        }

                                        if found {
                                            // A "contract not found" error here is usually the
                                            // freenet-core#1470 contract-creation race (a freshly-PUT
                                            // contract briefly not found while creation completes),
                                            // so we retry. But a restored/imported room whose contract
                                            // is genuinely absent from the network fails every time —
                                            // without a bound it re-GETs forever and the
                                            // "Syncing room state…" spinner spins forever
                                            // (freenet/river#290). Bound the retries: after
                                            // MAX_SYNC_ATTEMPTS_BEFORE_ERROR failures the room is
                                            // promoted to a terminal Error and we stop retrying it.
                                            let mut any_room_still_retrying = false;
                                            for room_key in &matching_rooms {
                                                // The retry bound only applies to a room still
                                                // awaiting its initial sync (placeholder state —
                                                // the #290 case). A room with valid synced state is
                                                // never given up on, so a transient contract-not-found
                                                // can't strand it.
                                                let awaiting_initial_sync =
                                                    ROOMS.read().map.get(room_key).is_some_and(
                                                        |rd| rd.is_awaiting_initial_sync(),
                                                    );
                                                let should_retry =
                                                    SYNC_INFO.write().record_failed_sync_attempt(
                                                        room_key,
                                                        awaiting_initial_sync,
                                                    );
                                                if should_retry {
                                                    any_room_still_retrying = true;
                                                    info!(
                                                        "Contract not found for room {:?} (likely #1470 race) — retrying",
                                                        MemberId::from(*room_key)
                                                    );
                                                } else {
                                                    warn!(
                                                        "Contract not found for room {:?} after retry bound — giving up (room absent from network)",
                                                        MemberId::from(*room_key)
                                                    );
                                                }
                                            }

                                            // Only schedule a retry if at least one matching room is
                                            // still within its retry budget. Otherwise every matching
                                            // room is now terminal Error and another ProcessRooms would
                                            // just no-op for them.
                                            if any_room_still_retrying {
                                                let tx = message_tx.clone();
                                                spawn_local(async move {
                                                    info!("Waiting before retrying room processing...");
                                                    sleep(Duration::from_millis(
                                                        super::constants::POST_PUT_DELAY_MS,
                                                    ))
                                                    .await;
                                                    info!("Retrying room processing after contract not found error");
                                                    if let Err(e) = tx.unbounded_send(
                                                        SynchronizerMessage::ProcessRooms,
                                                    ) {
                                                        error!("Failed to schedule retry: {}", e);
                                                    }
                                                });
                                            }
                                        } else {
                                            info!(
                                                "Contract ID {} not found in any of our rooms",
                                                contract_id
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                    SynchronizerMessage::AcceptInvitation {
                        owner_vk: _,
                        authorized_member: _,
                        invitee_signing_key: _,
                        nickname: _,
                    } => {
                        info!("Processing invitation acceptance");
                        // Instead of creating the room immediately, we'll process it through
                        // the regular room processing flow which will subscribe to the room
                        if let Err(e) = response_handler
                            .get_room_synchronizer_mut()
                            .process_rooms()
                            .await
                        {
                            error!("Failed to process rooms after invitation acceptance: {}", e);
                        }
                    }
                }
            }
            warn!("Synchronizer message loop ended");
        });
    }

    pub fn connect(&self) {
        if let Err(e) = self.message_tx.unbounded_send(SynchronizerMessage::Connect) {
            error!("Failed to send Connect message: {}", e);
        }
    }

    pub fn is_running(&self) -> bool {
        self.message_rx.is_none()
    }

    pub fn is_connected(&self) -> bool {
        matches!(*SYNC_STATUS.read(), SynchronizerStatus::Connected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_increases_exponentially() {
        // On non-wasm, jitter is deterministic (always jitter_range),
        // so delay = capped - jitter_range + jitter_range = capped
        assert_eq!(reconnect_delay_ms(0), 3000); // 3s * 2^0 = 3s (below cap)
        assert_eq!(reconnect_delay_ms(1), 6000); // 3s * 2^1 = 6s
        assert_eq!(reconnect_delay_ms(2), 12000); // 3s * 2^2 = 12s
        assert_eq!(reconnect_delay_ms(3), 24000); // 3s * 2^3 = 24s
        assert_eq!(reconnect_delay_ms(4), 48000); // 3s * 2^4 = 48s
        assert_eq!(reconnect_delay_ms(5), 60000); // 3s * 2^5 = 96s → capped at 60s
    }

    #[test]
    fn backoff_caps_at_max() {
        // All high failure counts should cap at RECONNECT_MAX_MS
        for failures in 5..30 {
            assert_eq!(reconnect_delay_ms(failures), 60000);
        }
        // Extreme values must not overflow
        assert_eq!(reconnect_delay_ms(u32::MAX), 60000);
    }

    #[test]
    fn flapping_open_then_die_backs_off() {
        // The regression: an Android socket that opens then immediately dies
        // must NOT reset the backoff on mere open. Because the stability dwell is
        // never reached, `note_stable` is never called, so each loss grows the
        // delay instead of pinning it at the 3s floor forever (freenet/river#406).
        let mut r = ReconnectState::default();
        let mut delays = vec![];
        for _ in 0..5 {
            r.note_connect_attempt(); // Connect dequeued
                                      // socket opens (Ok) — no failure increment, dwell not yet reached
            let armed = r
                .on_connection_lost()
                .expect("first loss of each cycle schedules a reconnect");
            delays.push(armed.delay_ms);
        }
        assert_eq!(delays, vec![3000, 6000, 12000, 24000, 48000]);
    }

    #[test]
    fn stable_connection_resets_backoff_but_stale_dwell_does_not() {
        let mut r = ReconnectState::default();
        r.note_connect_attempt();
        r.on_connection_lost(); // failures 0 -> 1
        r.note_connect_attempt();
        r.on_connection_lost(); // failures 1 -> 2

        // A dwell for a connection that is no longer the live one must NOT reset.
        r.note_stable(false);
        r.note_connect_attempt();
        assert_eq!(r.on_connection_lost().map(|a| a.delay_ms), Some(12000)); // failures=2 -> 12s

        // A connection that stayed up for the dwell (still current) resets it.
        r.note_stable(true);
        r.note_connect_attempt();
        assert_eq!(r.on_connection_lost().map(|a| a.delay_ms), Some(3000)); // reset -> 3s
    }

    #[test]
    fn duplicate_connection_lost_is_coalesced() {
        // A dropped socket's orphaned `onclose`, plus the watchdog reporting the
        // same death, must not each schedule a reconnect (freenet/river#406).
        let mut r = ReconnectState::default();
        r.note_connect_attempt();
        assert_eq!(r.on_connection_lost().map(|a| a.delay_ms), Some(3000)); // schedules; pending latched
        assert!(r.on_connection_lost().is_none()); // duplicate ignored
        assert!(r.on_connection_lost().is_none()); // and again
                                                   // The next real attempt clears the latch; a later loss schedules again,
                                                   // and the duplicates above did NOT inflate the failure count (6s, not 24s).
        r.note_connect_attempt();
        assert_eq!(r.on_connection_lost().map(|a| a.delay_ms), Some(6000));
    }

    #[test]
    fn stale_reconnect_timer_is_dropped_but_current_one_fires() {
        // A backoff timer must only drive a reconnect if its generation is still
        // current. When an immediate Connect (e.g. from PageBecameVisible on
        // resume) supersedes an already-scheduled timer, the old timer that fires
        // late must drop itself rather than stack a parallel reconnect chain
        // (freenet/river#406).
        let mut r = ReconnectState::default();

        // Loss 1 arms a timer at generation g1.
        r.note_connect_attempt();
        let g1 = r.on_connection_lost().expect("first loss arms").generation;
        assert!(r.is_current_reconnect(g1)); // its own timer is current

        // An immediate Connect runs, fails, and arms a NEW timer at g2. That
        // arming supersedes g1.
        r.note_connect_attempt(); // clears the pending latch (Connect dequeued)
        let g2 = r.on_connect_failed().generation;
        assert_ne!(g1, g2);
        assert!(!r.is_current_reconnect(g1)); // the g1 timer is now stale -> dropped
        assert!(r.is_current_reconnect(g2)); // the g2 timer is current -> fires

        // Once the g2 timer's Connect is dequeued (latch cleared), even g2 is no
        // longer a pending reconnect, so a duplicate firing is a no-op.
        r.note_connect_attempt();
        assert!(!r.is_current_reconnect(g2));
    }
}
