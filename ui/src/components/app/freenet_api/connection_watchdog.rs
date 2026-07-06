// Much of this module is compiled but unused on native (the watchdog only runs
// against a real browser WebSocket; its call sites are `#[cfg(wasm32)]`). The
// pure state machine is still exercised by the native unit tests below. Mirror
// the sibling freenet_api modules (`constants.rs`, `freenet_synchronizer.rs`),
// which blanket-allow dead_code for the same wasm-only reason.
#![allow(dead_code)]

//! WebSocket liveness watchdog (freenet/river#382).
//!
//! ## The bug
//!
//! River opens a WebSocket to the local Freenet node and receives live room
//! updates as `UpdateNotification`s. The stdlib `WebApi` wires `onclose` and
//! `onerror`, both of which River maps to a `ConnectionLost` message that
//! drives reconnect + re-subscribe + re-GET. That covers a socket that closes
//! or errors cleanly.
//!
//! A **half-open** socket does not. When the underlying TCP connection dies
//! without a FIN/RST (NAT/idle timeout, a silent network drop), the browser
//! fires **neither** `onclose` nor `onerror` — `WebSocket.readyState` stays
//! `OPEN`. River keeps believing it is `Connected`, the node keeps
//! `try_send`-ing notifications into a channel nothing reads, and the tab sits
//! frozen on its last snapshot with no recovery and no indication to the user.
//! The only recovery trigger that could help — `PageBecameVisible` — never
//! fires for a tab that is simply left open and visible.
//!
//! ## The fix
//!
//! Track the timestamp/sequence of the last inbound WS message (proof the
//! socket is alive). After a period of silence, send a cheap probe: a
//! `ContractRequest::Get` of the room the user is viewing (`subscribe: false`).
//! The node is already subscribed to that room, so it answers from its **local**
//! fresh copy — the round-trip reflects WS/node health, not network routing
//! (unlike a probe that has to route toward a far key). The probe is deliberate
//! double-duty:
//!   1. **Liveness** — a reply proves the socket is alive; no reply within a
//!      timeout means it is dead, so raise `ConnectionLost` and let the existing
//!      reconnect + re-subscribe + re-GET path recover.
//!   2. **Resync** — the GET response is merged idempotently (the same CRDT
//!      merge every GET uses), so it also catches up the room state if a live
//!      socket silently stopped delivering notifications (the second #382
//!      mechanism), un-freezing the view even without a reconnect.
//!
//! Why not `NodeQuery::ConnectedPeers` (a natural "ping")? The published River
//! UI is a *contract web app*, and freenet-core **blocks** `NodeQueries` from
//! contract origins (`websocket.rs`: "NodeQueries is not available to contract
//! web applications"), so such a probe would be rejected — spamming node/client
//! error logs every interval. A room GET is allowed and is ordinary River
//! traffic.
//!
//! ## Anti-storm design
//!
//! * The watchdog only guards an **established** connection (`SYNC_STATUS ==
//!   Connected`). While disconnected/reconnecting it is dormant — the existing
//!   backoff owns recovery, so the watchdog can't add reconnect pressure.
//! * At most **one** probe is outstanding per idle period. A healthy idle room
//!   costs one local GET per [`LIVENESS_IDLE_PROBE_MS`]; any inbound traffic
//!   (the probe's own reply, or a real update) clears the probe.
//! * Declaring death raises `ConnectionLost` **exactly once per stall episode**.
//!   On declaring death the watchdog latches into
//!   [`WatchdogState::ReconnectRequested`] and refuses to raise a second
//!   `ConnectionLost` until it observes the reconnect actually happen — either
//!   `SYNC_STATUS` leaving `Connected`, or a NEW socket opening (the
//!   connection-generation counter [`ws_connect_seq`] advancing, bumped only by
//!   a reconnect's `onopen`). It deliberately does NOT release on arbitrary
//!   inbound activity: a straggler message delivered late on the old,
//!   condemned socket would otherwise release the latch mid-lag and let a second
//!   `ConnectionLost` through (Codex #384 review, P2). This latch is
//!   load-bearing because `SYNC_STATUS` flips to `Disconnected`
//!   *asynchronously* in the synchronizer message loop: if that loop lags
//!   longer than a probe tick plus [`LIVENESS_PROBE_TIMEOUT_MS`], a naive
//!   watchdog would re-probe and fire a SECOND `ConnectionLost`, and each one
//!   independently schedules a reconnect and bumps `consecutive_failures`,
//!   producing overlapping reconnects and inflated backoff. The latch makes the
//!   "exactly once" guarantee hold even under message-loop lag.
//! * The probe timeout ([`LIVENESS_PROBE_TIMEOUT_MS`]) is far larger than a
//!   local GET's real latency, so a momentarily slow node does not trigger a
//!   false reconnect. If no room exists to probe, the watchdog never declares
//!   death (nothing was sent, so silence proves nothing).
//!
//! The tick decision is factored into the pure [`watchdog_tick`] so the state
//! machine is unit-testable without a browser.

use super::constants::{LIVENESS_IDLE_PROBE_MS, LIVENESS_PROBE_TIMEOUT_MS};
use std::sync::atomic::{AtomicU64, Ordering};

/// Wall-clock (epoch-ms) of the most recent inbound WebSocket message, used for
/// the idle threshold. `0` means "no activity recorded yet".
static LAST_WS_ACTIVITY_MS: AtomicU64 = AtomicU64::new(0);

/// Monotonic count of inbound WebSocket messages. Used to detect "did any
/// message arrive since we sent the probe?" independently of clock resolution —
/// some browsers coarsen `Date.now()` (Firefox `privacy.resistFingerprinting`
/// and Tor round to 100ms), so comparing timestamps could miss a probe reply
/// delivered within the same coarse tick and falsely reconnect a healthy socket
/// (Codex review, P2). A counter increments per message regardless of the clock.
static WS_ACTIVITY_SEQ: AtomicU64 = AtomicU64::new(0);

/// Monotonic count of successful WebSocket opens (`onopen`). This is a
/// connection *generation* counter, DISTINCT from [`WS_ACTIVITY_SEQ`] (which
/// counts every inbound message). The watchdog uses it to release the
/// `ReconnectRequested` latch only when a genuinely NEW socket has opened, so a
/// straggler message delivered late on the OLD, already-condemned socket — which
/// bumps `WS_ACTIVITY_SEQ` but not this — cannot masquerade as a completed
/// reconnect. Releasing on old-socket traffic would reopen the very
/// duplicate-`ConnectionLost` gap the latch closes (Codex #384 review, P2).
static WS_CONNECT_SEQ: AtomicU64 = AtomicU64::new(0);

// All three are plain atomics, NOT Dioxus signals — internal bookkeeping with
// zero UI reactivity, like `backward_probe::BACKWARD_PROBES`.

/// Record that an inbound WebSocket message just arrived — proof the socket is
/// alive. Called from the `WebApi` result callback for every inbound
/// `HostResult` (success OR error: either way, bytes came off the socket). This
/// is the freshness signal the watchdog reads to decide *when to probe* and
/// whether a probe was answered.
pub fn record_ws_activity() {
    LAST_WS_ACTIVITY_MS.store(now_ms(), Ordering::Relaxed);
    WS_ACTIVITY_SEQ.fetch_add(1, Ordering::Relaxed);
}

/// Record that the WebSocket just (re)opened — a fresh, live socket. Called from
/// the `onopen` callback ONLY. Bumps the connection-generation counter (which
/// releases the watchdog's `ReconnectRequested` latch — a new socket means the
/// reconnect actually landed) AND resets the activity freshness signal (so a
/// freshly-opened, still-idle socket isn't treated as long-silent).
pub fn record_ws_connected() {
    WS_CONNECT_SEQ.fetch_add(1, Ordering::Relaxed);
    record_ws_activity();
}

/// Epoch-ms of the last recorded inbound WS activity (`0` if none yet).
pub(crate) fn last_ws_activity_ms() -> u64 {
    LAST_WS_ACTIVITY_MS.load(Ordering::Relaxed)
}

/// Monotonic inbound-message counter.
pub(crate) fn ws_activity_seq() -> u64 {
    WS_ACTIVITY_SEQ.load(Ordering::Relaxed)
}

/// Monotonic WebSocket connection-generation counter (increments per `onopen`).
pub(crate) fn ws_connect_seq() -> u64 {
    WS_CONNECT_SEQ.load(Ordering::Relaxed)
}

/// Current wall-clock time in epoch-ms.
#[cfg(target_arch = "wasm32")]
fn now_ms() -> u64 {
    js_sys::Date::now() as u64
}

#[cfg(not(target_arch = "wasm32"))]
fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Timing thresholds for the liveness watchdog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WatchdogConfig {
    /// Silence (ms with no inbound WS traffic) before sending a probe.
    pub idle_probe_ms: u64,
    /// How long (ms) to wait for the probe to be answered before declaring the
    /// socket dead.
    pub probe_timeout_ms: u64,
}

impl WatchdogConfig {
    pub const fn from_constants() -> Self {
        Self {
            idle_probe_ms: LIVENESS_IDLE_PROBE_MS,
            probe_timeout_ms: LIVENESS_PROBE_TIMEOUT_MS,
        }
    }
}

/// Watchdog state carried across ticks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchdogState {
    /// Connection believed healthy; no probe in flight.
    Monitoring,
    /// A liveness probe was sent at `probe_sent_ms`, when the inbound-message
    /// counter was `activity_seq_at_probe`; awaiting a later message.
    Probing {
        probe_sent_ms: u64,
        activity_seq_at_probe: u64,
    },
    /// Death has been declared and a single `ConnectionLost` raised; the
    /// watchdog is latched dormant until the reconnect is actually observed, so
    /// it cannot raise a second `ConnectionLost` for the same stall episode
    /// while the synchronizer message loop is still lagging in flipping
    /// `SYNC_STATUS` to `Disconnected`. `connect_seq_at_reconnect` snapshots the
    /// connection-generation counter (`ws_connect_seq`) at the moment death was
    /// declared — a later increment means a NEW socket opened (`onopen`), i.e.
    /// the reconnect landed, so the latch releases back to `Monitoring`. It is
    /// keyed on the connection generation, NOT the inbound-message counter, so a
    /// straggler on the old socket can't release it early (Codex #384 review).
    /// The connection leaving `Connected` also releases the latch (handled at
    /// the top of `watchdog_tick`), so a genuinely-dead-then-reconnected socket
    /// is never permanently suppressed. See the "Anti-storm design" note
    /// (freenet/river#382).
    ReconnectRequested { connect_seq_at_reconnect: u64 },
}

/// What the async loop should do this tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchdogAction {
    /// Do nothing.
    Wait,
    /// Send a liveness probe now.
    Probe,
    /// Treat the socket as dead — raise `ConnectionLost`.
    Reconnect,
}

/// Pure state-machine step for one watchdog tick.
///
/// * Not connected → dormant (`Monitoring`/`Wait`): the reconnect path owns
///   recovery while disconnected, so the watchdog never piles on.
/// * `Monitoring` and idle ≥ `idle_probe_ms` → send a probe, enter `Probing`
///   (recording the current `activity_seq`).
/// * `Probing` and any message arrived since the probe (`activity_seq` moved) →
///   healthy, back to `Monitoring`.
/// * `Probing` and probe unanswered ≥ `probe_timeout_ms` → `Reconnect`, and
///   latch into `ReconnectRequested` (snapshotting `connect_seq`) so this same
///   stall can't fire a second `ConnectionLost`.
/// * `ReconnectRequested` → `Wait`, holding the latch until the reconnect is
///   observed: `connect_seq` advancing (a NEW socket opened via `onopen`)
///   returns to `Monitoring`; the connection dropping (`!connected`, handled
///   above) also returns to `Monitoring`. It does NOT release on `activity_seq`
///   alone, so an old-socket straggler can't spring the latch early. This
///   guarantees at most one `ConnectionLost` per stall episode even while the
///   synchronizer message loop lags in flipping `SYNC_STATUS` to `Disconnected`.
/// * Otherwise → keep waiting.
pub fn watchdog_tick(
    state: WatchdogState,
    connected: bool,
    now_ms: u64,
    last_activity_ms: u64,
    activity_seq: u64,
    connect_seq: u64,
    cfg: WatchdogConfig,
) -> (WatchdogState, WatchdogAction) {
    if !connected {
        // Dormant: an unestablished / torn-down connection is the reconnect
        // path's responsibility. Reset any in-flight probe.
        return (WatchdogState::Monitoring, WatchdogAction::Wait);
    }

    let idle = now_ms.saturating_sub(last_activity_ms);

    match state {
        WatchdogState::Monitoring => {
            if idle >= cfg.idle_probe_ms {
                (
                    WatchdogState::Probing {
                        probe_sent_ms: now_ms,
                        activity_seq_at_probe: activity_seq,
                    },
                    WatchdogAction::Probe,
                )
            } else {
                (WatchdogState::Monitoring, WatchdogAction::Wait)
            }
        }
        WatchdogState::Probing {
            probe_sent_ms,
            activity_seq_at_probe,
        } => {
            if activity_seq != activity_seq_at_probe {
                // A message (the probe reply, or any other traffic) arrived
                // since the probe was sent — the socket is alive. Counter-based
                // so a coarse clock can't hide the reply.
                (WatchdogState::Monitoring, WatchdogAction::Wait)
            } else if now_ms.saturating_sub(probe_sent_ms) >= cfg.probe_timeout_ms {
                // Probe unanswered past the timeout — treat as dead. Latch into
                // `ReconnectRequested` (snapshotting `connect_seq`) rather than
                // returning to `Monitoring`, so that a lagging synchronizer
                // message loop — which flips `SYNC_STATUS` to `Disconnected`
                // only asynchronously — can't drive us to re-probe and fire a
                // SECOND `ConnectionLost` for this same stall episode.
                //
                // NOTE (LOW, #382 review): inbound decode is async
                // (`onmessage` → `FileReader` → `onloadend` → result callback →
                // `record_ws_activity`), so a main-thread stall longer than
                // `probe_timeout_ms` while `Probing` could reach this arm even
                // though a probe reply was already in flight, yielding a
                // spurious reconnect. We accept that: the resulting reconnect is
                // recoverable and CRDT-safe (the probe GET's resync merges
                // idempotently), so adding timing complexity to avoid it isn't
                // worth it — recorded here so the next reader knows it's
                // understood, not overlooked.
                (
                    WatchdogState::ReconnectRequested {
                        connect_seq_at_reconnect: connect_seq,
                    },
                    WatchdogAction::Reconnect,
                )
            } else {
                // Still within the probe window.
                (state, WatchdogAction::Wait)
            }
        }
        WatchdogState::ReconnectRequested {
            connect_seq_at_reconnect,
        } => {
            if connect_seq != connect_seq_at_reconnect {
                // A new socket has opened (`onopen` bumped the
                // connection-generation counter) since death was declared — the
                // reconnect landed. Release the latch and monitor the new
                // connection, so a genuine later stall is still caught. Keyed on
                // `connect_seq`, NOT `activity_seq`, so a straggler message on
                // the OLD condemned socket can't release the latch early and let
                // a second `ConnectionLost` through (Codex #384 review).
                (WatchdogState::Monitoring, WatchdogAction::Wait)
            } else {
                // No new socket yet and `SYNC_STATUS` hasn't flipped to
                // `Disconnected` yet (message-loop lag). Hold the latch — do NOT
                // fire another `ConnectionLost`. The `!connected` branch above
                // is the other release path once the loop catches up.
                (state, WatchdogAction::Wait)
            }
        }
    }
}

/// Spawn the liveness watchdog loop. Idempotent-by-construction: called once
/// from `FreenetSynchronizer::start()` (which itself runs at most once).
#[cfg(target_arch = "wasm32")]
pub fn spawn_liveness_watchdog(
    message_tx: futures::channel::mpsc::UnboundedSender<
        super::freenet_synchronizer::SynchronizerMessage,
    >,
) {
    use super::constants::WATCHDOG_TICK_MS;
    use super::freenet_synchronizer::{SynchronizerMessage, SynchronizerStatus};
    use crate::components::app::SYNC_STATUS;
    use crate::util::sleep;
    use dioxus::logger::tracing::{info, warn};
    use dioxus::prelude::ReadableExt;
    use std::time::Duration;

    let cfg = WatchdogConfig::from_constants();
    // Start the activity clock so a freshly-started watchdog doesn't treat the
    // not-yet-connected socket as long-idle on its first ticks.
    record_ws_activity();

    // `safe_spawn_local` (setTimeout-deferred) rather than raw `spawn_local`:
    // this is called from inside the synchronizer's message-loop task (a polled
    // future), and spawning directly from a poll can re-enter the wasm-bindgen
    // task scheduler on Firefox mobile (AGENTS.md signal-safety rules).
    crate::util::safe_spawn_local(async move {
        info!(
            "Liveness watchdog started (idle_probe={}ms, probe_timeout={}ms)",
            cfg.idle_probe_ms, cfg.probe_timeout_ms
        );
        let mut state = WatchdogState::Monitoring;
        loop {
            sleep(Duration::from_millis(WATCHDOG_TICK_MS)).await;

            // Read connection state via `try_read()` (signal-safety): a
            // momentary Err (a writer holds the borrow) is treated as
            // "not connected" for this tick, which only defers monitoring one
            // tick — harmless.
            let connected = matches!(
                SYNC_STATUS.try_read().map(|s| s.clone()),
                Ok(SynchronizerStatus::Connected)
            );

            let (next_state, action) = watchdog_tick(
                state,
                connected,
                now_ms(),
                last_ws_activity_ms(),
                ws_activity_seq(),
                ws_connect_seq(),
                cfg,
            );
            state = next_state;

            match action {
                WatchdogAction::Wait => {}
                WatchdogAction::Probe => {
                    info!(
                        "WS liveness: no inbound traffic for >= {}ms — sending room-GET probe",
                        cfg.idle_probe_ms
                    );
                    if !send_liveness_probe().await {
                        // No probe was actually sent (no room to GET, or the
                        // socket is already gone). Do NOT stay in `Probing`:
                        // silence proves nothing if we never asked a question,
                        // so revert to `Monitoring` and retry next idle cycle.
                        // Never declare a dead socket without evidence.
                        state = WatchdogState::Monitoring;
                    }
                }
                WatchdogAction::Reconnect => {
                    warn!(
                        "WS liveness: probe unanswered for >= {}ms — treating socket as dead, \
                         forcing reconnect (freenet/river#382)",
                        cfg.probe_timeout_ms
                    );
                    if let Err(e) = message_tx.unbounded_send(SynchronizerMessage::ConnectionLost) {
                        warn!("Watchdog failed to send ConnectionLost: {}", e);
                    }
                }
            }
        }
    });
}

/// Send a single liveness probe: a `Get` of the room the user is viewing (or
/// any room), `subscribe: false`. Returns `true` iff a request was actually
/// sent — the caller must not conclude "dead" when nothing was sent.
///
/// The node is subscribed to the room, so it answers from its local fresh copy;
/// the reply records WS activity (via the `WebApi` result callback) and its
/// state is merged idempotently by `handle_get_response`, so the probe doubles
/// as a resync. Contract web apps are allowed to send `ContractOp::Get` (unlike
/// `NodeQueries`), so this works in the published gateway-served UI too.
#[cfg(target_arch = "wasm32")]
async fn send_liveness_probe() -> bool {
    use crate::components::app::{CURRENT_ROOM, ROOMS, WEB_API};
    use crate::util::owner_vk_to_contract_key;
    use dioxus::logger::tracing::warn;
    use dioxus::prelude::ReadableExt;
    use freenet_stdlib::client_api::{ClientRequest, ContractRequest};

    // Prefer the room the user is viewing (so the resync targets the very room
    // whose updates may be stalling); fall back to any known room.
    let owner_vk = {
        let current = CURRENT_ROOM.try_read().ok().and_then(|c| c.owner_key);
        match current {
            Some(vk) => Some(vk),
            None => ROOMS
                .try_read()
                .ok()
                .and_then(|r| r.map.keys().next().copied()),
        }
    };
    let Some(owner_vk) = owner_vk else {
        // No room to probe — there is no live-update stream that could be
        // stalling, so there is nothing to keep alive or catch up.
        return false;
    };

    let key = owner_vk_to_contract_key(&owner_vk);
    let get_request = ContractRequest::Get {
        key: *key.id(),
        return_contract_code: false,
        // subscribe:false — already subscribed; this only fetches current
        // state (and resyncs via the idempotent merge in handle_get_response).
        subscribe: false,
        blocking_subscribe: false,
    };

    // Holding the `WEB_API` write guard across this `.await` is safe: the
    // browser `WebApi::send` is `async` but contains no `.await` (synchronous
    // serialize + `WebSocket.send`), so it never yields to let another task
    // re-borrow `WEB_API`. This matches `follow_upgrade_pointer_if_needed`.
    if let Some(web_api) = WEB_API.write().as_mut() {
        match web_api.send(ClientRequest::ContractOp(get_request)).await {
            Ok(()) => true,
            Err(e) => {
                // A send error means the socket is already CLOSING/CLOSED — the
                // WebApi error handler raises ConnectionLost. Report "not sent"
                // so the watchdog stays Monitoring rather than timing out.
                warn!("Liveness probe GET send failed: {}", e);
                false
            }
        }
    } else {
        false
    }
}

/// Non-wasm stub — the watchdog only runs against a real browser WebSocket.
/// Present so `FreenetSynchronizer::start()` (compiled on all targets for the
/// native unit tests) can call it unconditionally.
#[cfg(not(target_arch = "wasm32"))]
pub fn spawn_liveness_watchdog(
    _message_tx: futures::channel::mpsc::UnboundedSender<
        super::freenet_synchronizer::SynchronizerMessage,
    >,
) {
}

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: WatchdogConfig = WatchdogConfig {
        idle_probe_ms: 60_000,
        probe_timeout_ms: 20_000,
    };

    // A stable connection generation used by tests whose focus is not the latch
    // (Monitoring / Probing arms never read `connect_seq`).
    const CONN: u64 = 0;

    #[test]
    fn disconnected_is_always_dormant() {
        // Even wildly idle, a non-connected socket never probes or reconnects —
        // the reconnect/backoff path owns recovery while disconnected.
        let (state, action) = watchdog_tick(
            WatchdogState::Probing {
                probe_sent_ms: 1_000,
                activity_seq_at_probe: 5,
            },
            false,
            10_000_000,
            0,
            5,
            CONN,
            CFG,
        );
        assert_eq!(state, WatchdogState::Monitoring);
        assert_eq!(action, WatchdogAction::Wait);
    }

    #[test]
    fn monitoring_recent_activity_waits() {
        // now - last_activity = 10s < 60s idle threshold → no probe.
        let (state, action) = watchdog_tick(
            WatchdogState::Monitoring,
            true,
            100_000,
            90_000,
            7,
            CONN,
            CFG,
        );
        assert_eq!(state, WatchdogState::Monitoring);
        assert_eq!(action, WatchdogAction::Wait);
    }

    #[test]
    fn monitoring_idle_beyond_threshold_probes() {
        // now - last_activity = 60s >= threshold → send a probe, enter Probing,
        // recording the current activity sequence.
        let now = 100_000;
        let (state, action) =
            watchdog_tick(WatchdogState::Monitoring, true, now, 40_000, 42, CONN, CFG);
        assert_eq!(
            state,
            WatchdogState::Probing {
                probe_sent_ms: now,
                activity_seq_at_probe: 42,
            }
        );
        assert_eq!(action, WatchdogAction::Probe);
    }

    #[test]
    fn probing_activity_after_probe_recovers() {
        // The sequence advanced past the probe snapshot → a message arrived →
        // healthy. This is the coarse-clock-safe replacement for the old
        // timestamp `>` check (Codex P2): note now_ms == probe_sent_ms here, so
        // a timestamp compare would have missed the reply.
        let probe_sent = 100_000;
        let (state, action) = watchdog_tick(
            WatchdogState::Probing {
                probe_sent_ms: probe_sent,
                activity_seq_at_probe: 10,
            },
            true,
            probe_sent, // same millisecond as the probe send
            probe_sent,
            11, // one message recorded since the probe
            CONN,
            CFG,
        );
        assert_eq!(state, WatchdogState::Monitoring);
        assert_eq!(action, WatchdogAction::Wait);
    }

    #[test]
    fn probing_within_timeout_keeps_waiting() {
        // Probe sent, no new message yet (seq unchanged), only 10s elapsed.
        let probe_sent = 100_000;
        let (state, action) = watchdog_tick(
            WatchdogState::Probing {
                probe_sent_ms: probe_sent,
                activity_seq_at_probe: 3,
            },
            true,
            probe_sent + 10_000,
            40_000,
            3, // no new messages
            CONN,
            CFG,
        );
        assert_eq!(
            state,
            WatchdogState::Probing {
                probe_sent_ms: probe_sent,
                activity_seq_at_probe: 3,
            }
        );
        assert_eq!(action, WatchdogAction::Wait);
    }

    #[test]
    fn probing_timeout_without_reply_reconnects() {
        // Probe unanswered (seq unchanged) for >= 20s → declare dead AND latch
        // into ReconnectRequested (snapshotting the current connection
        // generation) so the same stall can't fire a second ConnectionLost.
        let probe_sent = 100_000;
        let conn = 9u64;
        let (state, action) = watchdog_tick(
            WatchdogState::Probing {
                probe_sent_ms: probe_sent,
                activity_seq_at_probe: 3,
            },
            true,
            probe_sent + 20_000,
            40_000,
            3,
            conn,
            CFG,
        );
        assert_eq!(
            state,
            WatchdogState::ReconnectRequested {
                connect_seq_at_reconnect: conn,
            }
        );
        assert_eq!(action, WatchdogAction::Reconnect);
    }

    #[test]
    fn probe_reply_is_preferred_over_timeout_on_the_same_tick() {
        // If a reply arrived AND the timeout has elapsed on the same tick, the
        // reply wins (activity check comes first) — no spurious reconnect.
        let probe_sent = 100_000;
        let (state, action) = watchdog_tick(
            WatchdogState::Probing {
                probe_sent_ms: probe_sent,
                activity_seq_at_probe: 3,
            },
            true,
            probe_sent + 25_000,
            probe_sent + 1_000,
            4, // reply recorded
            CONN,
            CFG,
        );
        assert_eq!(state, WatchdogState::Monitoring);
        assert_eq!(action, WatchdogAction::Wait);
    }

    #[test]
    fn full_cycle_idle_probe_then_death() {
        // Drive the machine across ticks the way the async loop would, proving
        // a genuinely dead socket ends in exactly one Reconnect. The activity
        // sequence never moves (no messages ever arrive).
        let mut state = WatchdogState::Monitoring;
        let last_activity = 0u64;
        let seq = 1u64; // frozen — nothing inbound
        let conn = 2u64; // frozen — no new socket opened

        // t=30s: under the idle threshold → wait.
        let (s, a) = watchdog_tick(state, true, 30_000, last_activity, seq, conn, CFG);
        state = s;
        assert_eq!(a, WatchdogAction::Wait);
        assert_eq!(state, WatchdogState::Monitoring);

        // t=60s: idle threshold hit → probe.
        let (s, a) = watchdog_tick(state, true, 60_000, last_activity, seq, conn, CFG);
        state = s;
        assert_eq!(a, WatchdogAction::Probe);
        assert_eq!(
            state,
            WatchdogState::Probing {
                probe_sent_ms: 60_000,
                activity_seq_at_probe: seq,
            }
        );

        // t=70s: 10s into the probe, no reply → wait.
        let (s, a) = watchdog_tick(state, true, 70_000, last_activity, seq, conn, CFG);
        state = s;
        assert_eq!(a, WatchdogAction::Wait);

        // t=80s: 20s into the probe, still no reply → reconnect, latching into
        // ReconnectRequested rather than bare Monitoring.
        let (s, a) = watchdog_tick(state, true, 80_000, last_activity, seq, conn, CFG);
        state = s;
        assert_eq!(a, WatchdogAction::Reconnect);
        assert_eq!(
            state,
            WatchdogState::ReconnectRequested {
                connect_seq_at_reconnect: conn,
            }
        );
    }

    // --- ReconnectRequested latch: at most one ConnectionLost per stall ---

    #[test]
    fn reconnect_requested_holds_latch_and_does_not_refire() {
        // Core anti-refire guard: while latched (SYNC_STATUS still Connected, no
        // new socket opened), even a wildly-idle tick must NOT declare a second
        // death. This is the failure the guard exists to prevent.
        let (state, action) = watchdog_tick(
            WatchdogState::ReconnectRequested {
                connect_seq_at_reconnect: 7,
            },
            true,
            10_000_000, // absurdly idle
            0,
            42, // activity_seq value is irrelevant while latched
            7,  // connect_seq unchanged — reconnect hasn't landed
            CFG,
        );
        assert_eq!(
            state,
            WatchdogState::ReconnectRequested {
                connect_seq_at_reconnect: 7,
            }
        );
        assert_eq!(action, WatchdogAction::Wait);
    }

    #[test]
    fn reconnect_requested_ignores_old_socket_straggler() {
        // Codex #384 P2 regression: a straggler message delivered late on the
        // OLD, condemned socket bumps `activity_seq` but NOT `connect_seq` (no
        // new socket opened). The latch must HOLD — releasing here would let the
        // watchdog return to Monitoring, re-probe after the idle interval, and
        // fire a SECOND ConnectionLost while SYNC_STATUS is still Connected,
        // which is exactly the duplicate-reconnect the latch exists to prevent.
        let (state, action) = watchdog_tick(
            WatchdogState::ReconnectRequested {
                connect_seq_at_reconnect: 4,
            },
            true,
            10_000_000,
            9_999_000, // fresh-ish timestamp from the straggler
            999,       // activity_seq jumped — stragglers landed on the old socket
            4,         // connect_seq UNCHANGED — still the same (dead) socket
            CFG,
        );
        assert_eq!(
            state,
            WatchdogState::ReconnectRequested {
                connect_seq_at_reconnect: 4,
            },
            "an old-socket straggler must not release the latch"
        );
        assert_eq!(action, WatchdogAction::Wait);
    }

    #[test]
    fn reconnect_requested_releases_on_new_socket() {
        // A reconnect's onopen bumps `connect_seq` past the snapshot → the latch
        // releases back to Monitoring so a genuine later stall is still caught.
        // This is what prevents permanent suppression of recovery, and it fires
        // even if the watchdog never sampled the brief `Disconnected` window (a
        // fast reconnect).
        let (state, action) = watchdog_tick(
            WatchdogState::ReconnectRequested {
                connect_seq_at_reconnect: 7,
            },
            true,
            10_000_000,
            10_000_000,
            50,
            8, // connect_seq advanced — a new socket opened (onopen)
            CFG,
        );
        assert_eq!(state, WatchdogState::Monitoring);
        assert_eq!(action, WatchdogAction::Wait);
    }

    #[test]
    fn reconnect_requested_releases_when_disconnected() {
        // The other release path: once the message loop flips SYNC_STATUS to
        // Disconnected (connected == false), the latch clears to Monitoring via
        // the top-of-function dormant branch, even if no new socket opened yet.
        let (state, action) = watchdog_tick(
            WatchdogState::ReconnectRequested {
                connect_seq_at_reconnect: 7,
            },
            false,
            10_000_000,
            0,
            0,
            7,
            CFG,
        );
        assert_eq!(state, WatchdogState::Monitoring);
        assert_eq!(action, WatchdogAction::Wait);
    }

    /// Drive the pure machine across ticks the way the async loop does
    /// (`state = next_state`), holding `connected` / `last_activity` /
    /// `activity_seq` / `connect_seq` fixed across the window — i.e. the exact
    /// conditions of a dead socket with a lagging message loop and no reconnect
    /// yet. Returns the final state and the number of `Reconnect` actions.
    #[allow(clippy::too_many_arguments)]
    fn drive_ticks(
        mut state: WatchdogState,
        connected: bool,
        last_activity_ms: u64,
        activity_seq: u64,
        connect_seq: u64,
        start_now: u64,
        end_now: u64,
        step: u64,
        cfg: WatchdogConfig,
    ) -> (WatchdogState, usize) {
        let mut reconnects = 0usize;
        let mut now = start_now;
        while now <= end_now {
            let (next, action) = watchdog_tick(
                state,
                connected,
                now,
                last_activity_ms,
                activity_seq,
                connect_seq,
                cfg,
            );
            state = next;
            if action == WatchdogAction::Reconnect {
                reconnects += 1;
            }
            now += step;
        }
        (state, reconnects)
    }

    #[test]
    fn straggler_on_old_socket_during_lag_never_refires() {
        // Codex #384 P2 regression, driven over a long lag window: the watchdog
        // is latched (death declared at connection generation `conn`). Old-socket
        // stragglers have bumped `activity_seq` to a high value, but no NEW socket
        // has opened (`connect_seq` stays `conn`) because the reconnect hasn't
        // landed yet and SYNC_STATUS is still Connected (loop lagged). No matter
        // how long we wait, the latch must hold and NOT fire another
        // ConnectionLost. (With the earlier activity-keyed release this fired
        // repeatedly.)
        let conn = 4u64;
        let (state, reconnects) = drive_ticks(
            WatchdogState::ReconnectRequested {
                connect_seq_at_reconnect: conn,
            },
            true,
            1_000, // stale straggler timestamp relative to `now`
            999,   // activity_seq high — stragglers landed on the old socket
            conn,  // connect_seq UNCHANGED — reconnect hasn't opened a socket
            10_000,
            300_000,
            10_000,
            CFG,
        );
        assert_eq!(
            reconnects, 0,
            "an old-socket straggler must not release the latch or trigger a \
             second ConnectionLost, no matter how long the message loop lags"
        );
        assert_eq!(
            state,
            WatchdogState::ReconnectRequested {
                connect_seq_at_reconnect: conn,
            }
        );
    }

    #[test]
    fn at_most_one_connection_lost_per_stall_then_recovers() {
        // This is the regression test for the #382 follow-up MEDIUM. Without the
        // ReconnectRequested latch (i.e. if the timeout arm returned
        // `Monitoring` like the pre-fix code), the lagged window in phase 1
        // fires MULTIPLE Reconnects — the timeline is: probe at t=60s, death at
        // t=80s (Reconnect #1, back to Monitoring), immediate re-probe at t=90s
        // (already past the idle threshold), death again at t=110s (Reconnect
        // #2), and so on every 30s. So the pre-fix code yields >= 3 reconnects
        // across 10s..150s; the guard yields exactly 1. Phase 2 then proves the
        // latch is not a permanent gag: after a real reconnect a fresh stall is
        // still caught.
        let seq_dead = 1u64; // frozen — nothing inbound on the dead socket
        let conn0 = 5u64; // frozen — no new socket during the first stall

        // Phase 1: dead socket, message loop LAGGED (connected stays true), no
        // inbound activity and no new socket. Expect EXACTLY ONE ConnectionLost.
        let (state, reconnects) = drive_ticks(
            WatchdogState::Monitoring,
            true,
            0,        // last activity at epoch
            seq_dead, // frozen
            conn0,    // frozen
            10_000,
            150_000,
            10_000,
            CFG,
        );
        assert_eq!(
            reconnects, 1,
            "a single stall episode must raise ConnectionLost exactly once, \
             even while the message loop lags with SYNC_STATUS still Connected"
        );
        assert_eq!(
            state,
            WatchdogState::ReconnectRequested {
                connect_seq_at_reconnect: conn0,
            },
            "should still be latched after the single fire"
        );

        // The reconnect finally lands: onopen opens a NEW socket (connect_seq
        // advances) and records activity; SYNC_STATUS returns to Connected. The
        // latch must release.
        let conn1 = conn0 + 1;
        let seq_after_reconnect = seq_dead + 1;
        let reconnect_at = 200_000u64;
        let (state, action) = watchdog_tick(
            state,
            true,
            reconnect_at,
            reconnect_at,        // fresh activity timestamp
            seq_after_reconnect, // onopen recorded activity too
            conn1,               // onopen bumped the connection generation
            CFG,
        );
        assert_eq!(state, WatchdogState::Monitoring);
        assert_eq!(action, WatchdogAction::Wait);

        // Phase 2: a SECOND, independent stall on the new connection (activity
        // and connection generation frozen again at their post-reconnect
        // values). Recovery must still fire.
        let (_state, reconnects2) = drive_ticks(
            WatchdogState::Monitoring,
            true,
            reconnect_at,        // last activity = the reconnect
            seq_after_reconnect, // frozen again — new socket also dead
            conn1,               // frozen — no further reconnect yet
            reconnect_at + 10_000,
            reconnect_at + 100_000,
            10_000,
            CFG,
        );
        assert_eq!(
            reconnects2, 1,
            "after a genuine reconnect, a fresh stall episode must be caught \
             again — the latch must not permanently suppress recovery"
        );
    }
}
