//! Keeping the chat delegate's copy of each room's signing key, as the web UI does.
//!
//! The web UI stores every room's signing key in the delegate when it creates or joins a room
//! and again on every load (`create_room_modal.rs`, `migrate_signing_key` in
//! `ui/src/signing.rs`). This app signs with the room's stored `self_sk`, which gives the same
//! signatures (Ed25519 is deterministic), but a user who only uses this app would otherwise
//! never have a key in the delegate's `signing_key:` namespace.
//!
//! **A background sweep, not a step of creating or joining.** After a complete read of the
//! stored rooms, and only while no create or join works and no read is out, the sweep takes
//! the rooms that hold a `self_sk` one at a time: `GetPublicKey`; if the delegate holds no key
//! for the room, `StoreSigningKey` with that `self_sk` under the owner's key, then
//! `GetPublicKey` again to check. So a create or join is done as soon as the room is stored,
//! whatever the delegate does about keys. It covers rooms created before this existed, and one
//! whose key was not stored because the app ended first, with no state kept: a new run starts
//! the sweep again.
//!
//! **Registering an owned room (`EnsureRoomSubscription`).** For a room the user owns, once its
//! key is the delegate's (a check that found ours, or the check after a store), the sweep asks
//! the delegate to follow the room, as the web UI does for owned rooms (`ensure_room_subscription`
//! in `ui/src/components/app/freenet_api/chat_delegate.rs`), so the delegate can rotate a private
//! room's secret when members leave. The delegate refuses it unless it holds the owner's key
//! under this app's origin, which is why the key comes first. It is asked once per room per
//! connection whatever the answer, and again after a reconnect, as the web UI re-arms. A refusal
//! is logged, never shown. An ask that fails (no answer within the delegate deadline, a send
//! that fails, an error answer) is given up on for the rest of the run: it never drops the
//! connection and never repeats. An ask also goes out only when the generation watch has
//! positively confirmed that this build's room contract is the current one
//! ([`KeySweep::allow_ensure`]): the delegate keeps one record per room naming the contract of
//! the last ask, so a build on an older room contract would point it at a dead one. Rooms the
//! user does not own are never asked. A room created here reaches the sweep through the stored
//! rooms like any other, so it is asked once its key is stored and verified.
//!
//! **A key that is there is never replaced, except after the user imported that room's identity**
//! ([`KeySweep::authoritative`]): the web UI's import replaces it too.
//! The rest of this paragraph is the rule without that exception. The delegate overwrites without comparing
//! (`handle_store_signing_key`, `delegates/chat-delegate/src/handlers.rs`), so the check is
//! this app's. A key equal to ours is done. A different one is what the web UI's
//! `migrate_signing_key` reports as `StaleKeyOverwritten`, and its caller then removes the
//! messages the old key signed (`remove_unverifiable_messages`), which this app cannot do. So
//! the room is left alone, named once on stderr (never the key), and not counted as verified;
//! the web UI replaces the key when it next loads the room.
//!
//! **Why storing an absent key is safe beside the web UI.** The key stored is the room's
//! `self_sk` as stored, the same one the web UI stores for that room. The one way to store
//! over a different key is another live web UI tab that holds an older identity for the room
//! (the room left and rejoined under a new key, which the first tab has not reloaded) storing
//! its own between this app's check and its store: one round trip. That tab would be storing
//! the stale identity, and the web UI's own check-then-store has the same window against
//! itself. The store follows its check at once: a store that would have to wait (a task
//! started, a read is due) goes back to the check first, so the window stays one round trip.
//!
//! **Strictly lowest priority, one request at a time.** Delegate answers carry no request id,
//! and an error names nothing. The sweep sends only when the session is quiet
//! ([`super::session`]'s gate: no task works, no read is out, pending or due, no subscribe is
//! pending, no update is owed, the read was complete, and a few seconds have passed since
//! it). A task or read starts only when the sweep has nothing out, so the only thing a user's
//! action can wait for is the one request already out, at most the delegate deadline. An
//! answer is taken only if it is the kind awaited for the room awaited. A request unanswered
//! for the delegate deadline drops the connection, as for a read.
//!
//! **Failures retry later, never in a loop.** An error answer (`DelegateError::Other` from
//! the real handlers, `Missing` before the store is registered again), a refusal, an answer
//! that does not confirm the key, or no answer, counts against that room. It is asked again
//! after a wait that starts at [`super::session::Pace::key_retry_first`] and doubles to
//! `key_retry_max`, so a delegate that keeps refusing is asked about once per cap. A dropped
//! connection with a request out, and a request that could not be sent, count as failures
//! too, whatever dropped the connection: a drop that had nothing to do with the delegate
//! bumps the waits as well, which only delays the key. On top of the per-room wait there is one for the whole sweep, with the same doubling,
//! reset by any success: with many rooms and a delegate that fails, the app does not
//! reconnect once per room back to back. A `Missing` answer is a failure here and then takes
//! the same path as one to a read (the session registers the store again), so it is paced by
//! the sweep's wait. Nothing is shown: the app works without the key.
//!
//! **Which rooms are done** is kept by room and identity, `(owner, self)`, and forgotten when
//! the room is no longer shown, so a room left and rejoined under a new key is checked again.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use ed25519_dalek::VerifyingKey;
use freenet_stdlib::client_api::ClientRequest;
use river_core::chat_delegate::ChatDelegateResponseMsg;

use super::delegate;
use super::room::LiveRoom;

/// A room as the sweep knows it: its owner's key and the identity it is stored under here.
type Key = (VerifyingKey, VerifyingKey);

fn key_of(room: &LiveRoom) -> Option<Key> {
    room.stored
        .self_sk
        .as_ref()
        .map(|sk| (room.stored.owner_vk, sk.verifying_key()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Ask which key the delegate holds.
    Check,
    /// It holds none: store ours.
    Store,
    /// Stored: ask again.
    Verify,
    /// The room's key is the delegate's, and the user owns the room: ask the delegate to
    /// follow the room (`EnsureRoomSubscription`) so it can rotate a private room's secret.
    Ensure,
}

/// What the sweep is doing now, for one room.
#[derive(Clone, Copy, Debug)]
struct Current {
    room: Key,
    phase: Phase,
    /// When the request for this phase went out, if it has.
    sent: Option<Instant>,
    /// The `request_id` of the `EnsureRoomSubscription` out, in [`Phase::Ensure`].
    request_id: u64,
}

/// Failures so far in a row, and when to try again.
#[derive(Clone, Copy, Debug)]
struct Backoff {
    failures: u32,
    not_before: Instant,
}

#[derive(Default)]
pub struct KeySweep {
    current: Option<Current>,
    /// Rooms whose key the delegate holds, this run.
    verified: HashSet<Key>,
    /// Owned rooms the delegate was asked to follow on this connection, whatever it answered:
    /// asked once per room per connection, never again after a refusal or an error. Cleared
    /// when the connection drops, as the web UI asks again after a reconnect.
    ensured: HashSet<Key>,
    /// Owned rooms whose ask failed (it went unanswered, could not be sent, or the delegate
    /// answered it with an error): not asked again for the rest of the run. An ask is best
    /// effort, so one that fails never drops the connection and never repeats.
    gave_up: HashSet<Key>,
    /// Whether an ask may go out now: the session sets it when the generation watch has
    /// confirmed that this build's room contract is the current one. The delegate keeps one
    /// record per room, naming the contract the last ask gave, so an ask from a build on an
    /// older room contract would point the delegate at a contract nobody uses. Off until told.
    ensure_allowed: bool,
    /// The last `request_id` used.
    last_request_id: u64,
    /// Rooms the delegate holds a different key for: left alone, named once.
    mismatched: HashSet<Key>,
    backoff: HashMap<Key, Backoff>,
    /// Rooms whose identity the user just imported: the delegate's key for them is replaced
    /// even when it holds another, as the web UI's import does (`migrate_signing_key` with the
    /// authoritative flag). Kept in memory until the key is stored; an entry for a room that never
    /// shows is a few bytes, and a restart forgets it (the next sweep then leaves a different key).
    authoritative: HashSet<Key>,
    /// The wait that holds for the whole sweep.
    overall: Option<Backoff>,
}

/// How long to wait after `failures` failures in a row: `first` doubling, up to `max`.
pub fn wait_after(failures: u32, first: Duration, max: Duration) -> Duration {
    first
        .saturating_mul(1u32 << failures.saturating_sub(1).min(16))
        .min(max)
}

/// A room the user owns: its signing key is the owner's. The delegate's rotation of a private
/// room's secret is the owner's act, so only these are registered.
fn owned(k: &Key) -> bool {
    k.0 == k.1
}

/// The waits: the first, and the most it doubles to.
pub type Pace = (Duration, Duration);

impl KeySweep {
    /// The user imported `identity` for `owner`'s room: the delegate's key for the room is made
    /// this one, over whatever it holds. (The only case it is replaced: the user chose it.)
    pub fn authoritative(&mut self, owner: VerifyingKey, identity: VerifyingKey) {
        self.authoritative.insert((owner, identity));
        self.verified.remove(&(owner, identity));
        self.mismatched.remove(&(owner, identity));
        self.backoff.remove(&(owner, identity));
    }

    /// Whether asks to follow a room may go out now (see `ensure_allowed`). The session sets it
    /// before each [`next_request`](Self::next_request).
    pub fn allow_ensure(&mut self, allowed: bool) {
        self.ensure_allowed = allowed;
    }

    /// Whether a request is out: nothing else may be sent to the delegate meanwhile.
    pub fn busy(&self) -> bool {
        self.current.is_some_and(|c| c.sent.is_some())
    }

    /// The connection is gone, and with it whatever was out. A request that was out counts as
    /// a failure; whatever the sweep had decided starts again from its check.
    pub fn connection_lost(&mut self, pace: Pace, now: Instant) {
        if let Some(cur) = self.current.filter(|c| c.sent.is_some()) {
            self.ensure_over(cur.room, cur.phase, pace, now);
        }
        self.current = None;
        // Ask again on the next connection, except for rooms whose ask failed.
        self.ensured.clear();
    }

    /// A request of `phase` for `room` failed: the ask to follow a room is not repeated for the
    /// rest of the run and holds nothing else back; a key request backs off.
    fn ensure_over(&mut self, room: Key, phase: Phase, pace: Pace, now: Instant) {
        if phase == Phase::Ensure {
            self.current = None;
            self.gave_up.insert(room);
        } else {
            self.fail(room, pace, now);
        }
    }

    /// The request just returned by [`next_request`](Self::next_request) could not be sent.
    pub fn send_failed(&mut self, pace: Pace, now: Instant) {
        if let Some(cur) = self.current {
            self.ensure_over(cur.room, cur.phase, pace, now);
        }
    }

    /// The next request, if the sweep may send one now (`idle`: the session is quiet, see the
    /// module docs) and a room needs it.
    pub fn next_request(
        &mut self,
        rooms: &[LiveRoom],
        idle: bool,
        now: Instant,
    ) -> Option<ClientRequest<'static>> {
        let shown: HashSet<Key> = rooms.iter().filter_map(key_of).collect();
        self.verified.retain(|k| shown.contains(k));
        self.mismatched.retain(|k| shown.contains(k));
        self.backoff.retain(|k, _| shown.contains(k));
        self.ensured.retain(|k| shown.contains(k));
        self.gave_up.retain(|k| shown.contains(k));
        if self.current.is_some_and(|c| !shown.contains(&c.room)) {
            self.current = None;
        }
        if self.busy() {
            return None;
        }
        if !idle {
            // Never hold a store (or a check of one) across a gap: what it was based on may
            // change while it waits. It starts again from its check. (Not while a request is
            // out, which was returned above.)
            if !self.busy() {
                self.current = None;
            }
            return None;
        }
        if self.current.is_none() {
            if self.overall.is_some_and(|b| now < b.not_before) {
                return None;
            }
            // Of the rooms that need asking and whose own wait is over, the one that has waited
            // longest: a room never tried first, then the one whose wait ended earliest. Taking
            // the first in the list would pick a room that always fails every time the sweep's
            // wait ends, and the rooms after it would never be asked.
            let room = rooms
                .iter()
                .filter_map(key_of)
                .filter(|k| {
                    self.backoff.get(k).is_none_or(|b| b.not_before <= now)
                        && !self.mismatched.contains(k)
                        && (!self.verified.contains(k) || self.needs_ensure(k))
                })
                .min_by_key(|k| self.backoff.get(k).map(|b| b.not_before))?;
            let phase = if self.verified.contains(&room) {
                Phase::Ensure
            } else {
                Phase::Check
            };
            self.current = Some(Current {
                room,
                phase,
                sent: None,
                request_id: 0,
            });
        }
        let cur = self.current.as_mut()?;
        let (owner, _) = cur.room;
        let request = match cur.phase {
            Phase::Ensure => {
                if !self.ensure_allowed {
                    self.current = None;
                    return None;
                }
                self.last_request_id += 1;
                cur.request_id = self.last_request_id;
                delegate::ensure_room_subscription(&owner, cur.request_id)
            }
            Phase::Check | Phase::Verify => delegate::public_key(&owner),
            Phase::Store => {
                let sk = rooms
                    .iter()
                    .find(|r| key_of(r) == Some(cur.room))
                    .and_then(|r| r.stored.self_sk.as_ref())?;
                delegate::store_signing_key(&owner, sk)
            }
        };
        cur.sent = Some(now);
        Some(request)
    }

    /// A chat delegate answer. True when it was the awaited one.
    pub fn on_answer(
        &mut self,
        answer: &ChatDelegateResponseMsg,
        pace: Pace,
        now: Instant,
    ) -> bool {
        let Some(cur) = self.current.filter(|c| c.sent.is_some()) else {
            return false;
        };
        let (owner, ours) = cur.room;
        match (cur.phase, answer) {
            (
                Phase::Check | Phase::Verify,
                ChatDelegateResponseMsg::GetPublicKeyResponse {
                    room_key,
                    public_key,
                },
            ) if *room_key == owner.to_bytes() => match (cur.phase, public_key) {
                (Phase::Check, None) => self.go(Phase::Store),
                (_, Some(held)) if *held == ours.to_bytes() => self.done(cur.room),
                // Imported by the user: theirs replaces what the delegate holds.
                (Phase::Check, Some(_)) if self.authoritative.contains(&cur.room) => {
                    self.go(Phase::Store)
                }
                (Phase::Check, Some(_)) => {
                    // The owner's key names the room; a room's own name may be private.
                    crate::diag!(
                        "River: the delegate holds a different signing key for room {}; \
                         leaving it for River in your browser to replace",
                        bs58::encode(owner.to_bytes()).into_string()
                    );
                    self.current = None;
                    self.mismatched.insert(cur.room);
                    self.overall = None;
                }
                // Stored, but not what the delegate now holds.
                (_, _) => self.fail(cur.room, pace, now),
            },
            (
                Phase::Store,
                ChatDelegateResponseMsg::StoreSigningKeyResponse { room_key, result },
            ) if *room_key == owner.to_bytes() => match result {
                Ok(()) => self.go(Phase::Verify),
                Err(e) => {
                    crate::diag!(
                        "River: the delegate refused a room's signing key: {}",
                        crate::diag::scrub(e)
                    );
                    self.fail(cur.room, pace, now)
                }
            },
            (
                Phase::Ensure,
                ChatDelegateResponseMsg::EnsureRoomSubscriptionResponse {
                    room_owner_vk,
                    request_id,
                    result,
                },
            ) if *room_owner_vk == owner.to_bytes() && *request_id == cur.request_id => {
                // Asked once; whatever the delegate said, it is not asked again. A refusal (it
                // holds no key for this app under the room) is not shown: the web UI asks too.
                if let Err(e) = result {
                    crate::diag!(
                        "River: the delegate did not follow a room: {}",
                        crate::diag::scrub(e)
                    );
                }
                self.current = None;
                self.ensured.insert(cur.room);
            }
            _ => return false,
        }
        true
    }

    /// An error answer to the awaited request. True when it was taken (the caller checked that
    /// nothing else could have caused it).
    pub fn on_error(&mut self, why: &str, pace: Pace, now: Instant) -> bool {
        let Some(cur) = self.current.filter(|c| c.sent.is_some()) else {
            return false;
        };
        if cur.phase == Phase::Ensure {
            crate::diag!(
                "River: asking the delegate to follow a room failed: {}",
                crate::diag::scrub(why)
            );
        } else {
            crate::diag!(
                "River: asking the delegate about a room's signing key failed: {}",
                crate::diag::scrub(why)
            );
        }
        self.ensure_over(cur.room, cur.phase, pace, now);
        true
    }

    /// Whether the awaited request has gone unanswered past `wait`, so the connection must be
    /// dropped (its answer must not arrive late and be taken for another request's). An ask to
    /// follow a room is the exception: its answer names the room and the ask, so a late one is
    /// taken for nothing; it is given up on (never asked again this run) and the connection
    /// stays, so a delegate that never answers it cannot drive a reconnect loop.
    pub fn expire(&mut self, wait: Duration, pace: Pace, now: Instant) -> bool {
        let Some(cur) = self.current else {
            return false;
        };
        match cur.sent {
            Some(at) if now.saturating_duration_since(at) >= wait => {
                let drop_connection = cur.phase != Phase::Ensure;
                self.ensure_over(cur.room, cur.phase, pace, now);
                drop_connection
            }
            _ => false,
        }
    }

    fn go(&mut self, phase: Phase) {
        if let Some(c) = &mut self.current {
            c.phase = phase;
            c.sent = None;
        }
    }

    fn done(&mut self, room: Key) {
        self.current = None;
        self.backoff.remove(&room);
        self.verified.insert(room);
        self.authoritative.remove(&room);
        self.overall = None;
        // The key is the delegate's: an owned room is next registered with it.
        if self.needs_ensure(&room) {
            self.current = Some(Current {
                room,
                phase: Phase::Ensure,
                sent: None,
                request_id: 0,
            });
        }
    }

    /// An owned room whose key the delegate holds and that was not yet asked about.
    fn needs_ensure(&self, room: &Key) -> bool {
        self.ensure_allowed
            && owned(room)
            && self.verified.contains(room)
            && !self.ensured.contains(room)
            && !self.gave_up.contains(room)
    }

    fn fail(&mut self, room: Key, (first, max): Pace, now: Instant) {
        self.current = None;
        let failures = self.backoff.get(&room).map_or(0, |b| b.failures) + 1;
        self.backoff.insert(
            room,
            Backoff {
                failures,
                not_before: now + wait_after(failures, first, max),
            },
        );
        let all = self.overall.map_or(0, |b| b.failures) + 1;
        self.overall = Some(Backoff {
            failures: all,
            not_before: now + wait_after(all, first, max),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rooms::room_tests::Fixture;
    use ed25519_dalek::SigningKey;
    use freenet_stdlib::client_api::DelegateRequest;
    use freenet_stdlib::prelude::InboundDelegateMsg;
    use river_core::chat_delegate::ChatDelegateRequestMsg;

    const FIRST: Duration = Duration::from_secs(60);
    const MAX: Duration = Duration::from_secs(30 * 60);
    const PACE: Pace = (FIRST, MAX);

    /// A room owned by seed `owner`, held under the identity of seed `me`.
    fn room_of(owner: u8, me: u8) -> LiveRoom {
        let f = Fixture::new();
        let mut stored = f.stored(f.private_state());
        stored.owner_vk = SigningKey::from_bytes(&[owner; 32]).verifying_key();
        stored.self_sk = Some(SigningKey::from_bytes(&[me; 32]));
        LiveRoom::new(stored)
    }

    fn room() -> (LiveRoom, SigningKey) {
        let r = room_of(1, 2);
        let sk = r.stored.self_sk.clone().unwrap();
        (r, sk)
    }

    fn msg(r: &ClientRequest<'_>) -> ChatDelegateRequestMsg {
        let ClientRequest::DelegateOp(DelegateRequest::ApplicationMessages { inbound, .. }) = r
        else {
            panic!("a delegate request");
        };
        let [InboundDelegateMsg::ApplicationMessage(m)] = inbound.as_slice() else {
            panic!("one message");
        };
        ciborium::de::from_reader(m.payload.as_slice()).unwrap()
    }

    fn got(room: &VerifyingKey, held: Option<[u8; 32]>) -> ChatDelegateResponseMsg {
        ChatDelegateResponseMsg::GetPublicKeyResponse {
            room_key: room.to_bytes(),
            public_key: held,
        }
    }

    fn stored_ok(room: &VerifyingKey) -> ChatDelegateResponseMsg {
        ChatDelegateResponseMsg::StoreSigningKeyResponse {
            room_key: room.to_bytes(),
            result: Ok(()),
        }
    }

    fn is_check(r: &ClientRequest<'_>) -> bool {
        matches!(msg(r), ChatDelegateRequestMsg::GetPublicKey { .. })
    }

    #[test]
    fn the_wait_doubles_to_its_cap() {
        let waits: Vec<u64> = (1..=8)
            .map(|n| wait_after(n, FIRST, MAX).as_secs())
            .collect();
        assert_eq!(waits, [60, 120, 240, 480, 960, 1800, 1800, 1800]);
        // Never overflows, however many failures.
        assert_eq!(wait_after(u32::MAX, FIRST, MAX), MAX);
    }

    /// A delegate that fails every time is asked at exactly the backed-off times, a bounded
    /// number in a day: 7 asks in the first 61 minutes (at 0, 1, 3, 7, 15, 31 and 61 minutes),
    /// then one every 30 minutes.
    #[test]
    fn a_failing_delegate_is_asked_on_the_backoff_schedule_and_no_more() {
        let (live, _) = room();
        let rooms = [live];
        let mut sweep = KeySweep::default();
        let t0 = Instant::now();
        let mut asks = Vec::new();
        for second in 0..24 * 3600u64 {
            let now = t0 + Duration::from_secs(second);
            if sweep.next_request(&rooms, true, now).is_some() {
                asks.push(second);
                assert!(sweep.on_error("boom", PACE, now));
            }
        }
        let mut want = vec![0u64, 60, 180, 420, 900, 1860, 3660];
        while *want.last().unwrap() + 1800 < 24 * 3600 {
            want.push(want.last().unwrap() + 1800);
        }
        assert_eq!(asks, want);
        assert_eq!(asks.len(), 7 + 45);
    }

    /// With many rooms and a delegate that fails, the whole sweep waits, not each room in turn:
    /// five rooms are not five asks back to back.
    #[test]
    fn one_failure_holds_back_every_room_not_only_its_own() {
        let rooms: Vec<LiveRoom> = (1..=5).map(|n| room_of(n, 50 + n)).collect();
        let mut sweep = KeySweep::default();
        let t0 = Instant::now();
        assert!(sweep.next_request(&rooms, true, t0).is_some());
        assert!(sweep.on_error("boom", PACE, t0));
        // Four other rooms are due, and none is asked until the sweep's wait is over.
        assert!(
            sweep
                .next_request(&rooms, true, t0 + FIRST - Duration::from_secs(1))
                .is_none()
        );
        assert!(sweep.next_request(&rooms, true, t0 + FIRST).is_some());
        // Over a day, five always-failing rooms are asked far fewer than 5 times per wait.
        let mut sweep = KeySweep::default();
        let mut asks = 0;
        for second in 0..24 * 3600u64 {
            let now = t0 + Duration::from_secs(second);
            if sweep.next_request(&rooms, true, now).is_some() {
                asks += 1;
                sweep.on_error("boom", PACE, now);
            }
        }
        assert_eq!(asks, 7 + 45, "the same as one room alone");
    }

    #[test]
    fn a_missing_key_is_stored_then_checked_and_a_present_one_is_not_asked_again() {
        let (live, sk) = room();
        let owner = live.stored.owner_vk;
        let rooms = [live];
        let mut sweep = KeySweep::default();
        let now = Instant::now();
        // Nothing goes out unless the session is idle.
        assert!(sweep.next_request(&rooms, false, now).is_none());
        let r = sweep.next_request(&rooms, true, now).unwrap();
        assert!(
            matches!(msg(&r), ChatDelegateRequestMsg::GetPublicKey { room_key } if room_key == owner.to_bytes())
        );
        assert!(sweep.busy());
        // One request at a time.
        assert!(sweep.next_request(&rooms, true, now).is_none());
        // An answer for another room is not ours.
        let other = SigningKey::from_bytes(&[9; 32]).verifying_key();
        assert!(!sweep.on_answer(&got(&other, None), PACE, now));
        assert!(sweep.on_answer(&got(&owner, None), PACE, now));
        let r = sweep.next_request(&rooms, true, now).unwrap();
        match msg(&r) {
            ChatDelegateRequestMsg::StoreSigningKey {
                room_key,
                signing_key_bytes,
            } => {
                assert_eq!(room_key, owner.to_bytes());
                assert_eq!(signing_key_bytes, sk.to_bytes());
            }
            other => panic!("{other:?}"),
        }
        assert!(sweep.on_answer(&stored_ok(&owner), PACE, now));
        let r = sweep.next_request(&rooms, true, now).unwrap();
        assert!(is_check(&r));
        assert!(sweep.on_answer(&got(&owner, Some(sk.verifying_key().to_bytes())), PACE, now));
        // Done for this run.
        assert!(sweep.next_request(&rooms, true, now + MAX * 4).is_none());
    }

    /// A store that has to wait is not sent later on the strength of an old check: whatever
    /// made it wait (a task, a read) may have changed what the delegate holds.
    #[test]
    fn a_store_that_has_to_wait_starts_again_from_its_check() {
        let (live, _) = room();
        let owner = live.stored.owner_vk;
        let rooms = [live];
        let mut sweep = KeySweep::default();
        let now = Instant::now();
        sweep.next_request(&rooms, true, now).unwrap();
        assert!(sweep.on_answer(&got(&owner, None), PACE, now));
        // The session is no longer quiet: nothing goes out, and the store is forgotten.
        assert!(sweep.next_request(&rooms, false, now).is_none());
        let r = sweep.next_request(&rooms, true, now).unwrap();
        assert!(is_check(&r), "the check is asked again, not the store");
        // The same for the check of a store just made.
        assert!(sweep.on_answer(&got(&owner, None), PACE, now));
        let r = sweep.next_request(&rooms, true, now).unwrap();
        assert!(!is_check(&r));
        assert!(sweep.on_answer(&stored_ok(&owner), PACE, now));
        assert!(sweep.next_request(&rooms, false, now).is_none());
        let r = sweep.next_request(&rooms, true, now).unwrap();
        assert!(is_check(&r));
    }

    #[test]
    fn a_different_key_is_left_alone_and_never_asked_about_again() {
        let (live, _) = room();
        let owner = live.stored.owner_vk;
        let rooms = [live];
        let mut sweep = KeySweep::default();
        let now = Instant::now();
        sweep.next_request(&rooms, true, now).unwrap();
        assert!(sweep.on_answer(&got(&owner, Some([7; 32])), PACE, now));
        assert!(sweep.next_request(&rooms, true, now + MAX * 4).is_none());
        assert!(!sweep.busy());
    }

    /// A room left and rejoined under a new identity in the same run is checked again, and a
    /// room no longer shown is forgotten.
    #[test]
    fn done_is_per_identity_and_forgotten_when_the_room_is_gone() {
        let before = room_of(1, 2);
        let owner = before.stored.owner_vk;
        let mut sweep = KeySweep::default();
        let now = Instant::now();
        let rooms = [before];
        sweep.next_request(&rooms, true, now).unwrap();
        sweep.on_answer(
            &got(
                &owner,
                Some(
                    rooms[0]
                        .stored
                        .self_sk
                        .as_ref()
                        .unwrap()
                        .verifying_key()
                        .to_bytes(),
                ),
            ),
            PACE,
            now,
        );
        assert!(sweep.next_request(&rooms, true, now).is_none(), "done");
        // The same room under a new identity.
        let after = [room_of(1, 3)];
        let r = sweep.next_request(&after, true, now).unwrap();
        assert!(is_check(&r), "asked again for the new identity");
        sweep.on_answer(&got(&owner, Some([7; 32])), PACE, now);
        assert!(
            sweep.next_request(&after, true, now).is_none(),
            "mismatch kept"
        );
        // Gone from the list, then back as before: forgotten, so asked again.
        assert!(sweep.next_request(&[], true, now).is_none());
        assert!(sweep.verified.is_empty() && sweep.mismatched.is_empty());
        assert!(sweep.next_request(&rooms, true, now).is_some());
    }

    /// The first room in the list always fails: the sweep still gets to the later, healthy room
    /// each time it resumes, and the failing one is retried on its own wait.
    #[test]
    fn a_room_that_always_fails_does_not_keep_the_others_from_being_asked() {
        let bad = room_of(1, 51);
        let good = room_of(2, 52);
        let (bad_owner, good_owner) = (bad.stored.owner_vk, good.stored.owner_vk);
        let good_key = good.stored.self_sk.clone().unwrap().verifying_key();
        let rooms = [bad, good];
        let mut sweep = KeySweep::default();
        let t0 = Instant::now();
        let asked = |sweep: &mut KeySweep, now: Instant| -> Option<VerifyingKey> {
            let r = sweep.next_request(&rooms, true, now)?;
            let ChatDelegateRequestMsg::GetPublicKey { room_key } = msg(&r) else {
                panic!("a check first");
            };
            Some(
                [bad_owner, good_owner]
                    .into_iter()
                    .find(|o| o.to_bytes() == room_key)
                    .unwrap(),
            )
        };
        // The bad room is first and asked first; it fails.
        assert_eq!(asked(&mut sweep, t0), Some(bad_owner));
        assert!(sweep.on_error("boom", PACE, t0));
        // When the sweep's wait is over, the good room is asked, not the bad one again.
        assert_eq!(asked(&mut sweep, t0 + FIRST), Some(good_owner));
        assert!(sweep.on_answer(
            &got(&good_owner, Some(good_key.to_bytes())),
            PACE,
            t0 + FIRST
        ));
        // The bad room is retried on its own wait (60 s after its first failure, so now), and
        // its second failure puts it 120 s back.
        let t1 = t0 + FIRST;
        assert_eq!(asked(&mut sweep, t1), Some(bad_owner));
        assert!(sweep.on_error("boom", PACE, t1));
        assert_eq!(
            asked(&mut sweep, t1 + FIRST * 2 - Duration::from_secs(1)),
            None
        );
        assert_eq!(asked(&mut sweep, t1 + FIRST * 2), Some(bad_owner));
    }

    /// A store the delegate says yes to but does not then hold counts as a failure.
    #[test]
    fn a_store_that_did_not_take_is_a_failure() {
        let (live, _) = room();
        let owner = live.stored.owner_vk;
        let rooms = [live];
        let mut sweep = KeySweep::default();
        let now = Instant::now();
        sweep.next_request(&rooms, true, now).unwrap();
        sweep.on_answer(&got(&owner, None), PACE, now);
        sweep.next_request(&rooms, true, now).unwrap();
        sweep.on_answer(&stored_ok(&owner), PACE, now);
        sweep.next_request(&rooms, true, now).unwrap();
        sweep.on_answer(&got(&owner, None), PACE, now);
        assert!(sweep.next_request(&rooms, true, now).is_none());
        assert!(sweep.next_request(&rooms, true, now + FIRST).is_some());
    }

    #[test]
    fn an_unanswered_request_expires_once_and_starts_over_on_a_new_connection() {
        let (live, _) = room();
        let rooms = [live];
        let mut sweep = KeySweep::default();
        let now = Instant::now();
        sweep.next_request(&rooms, true, now).unwrap();
        let wait = Duration::from_secs(20);
        assert!(!sweep.expire(wait, PACE, now + Duration::from_secs(19)));
        assert!(sweep.expire(wait, PACE, now + wait));
        assert!(!sweep.expire(wait, PACE, now + wait));
        assert!(!sweep.busy());
        // Counted as a failure: waits out the first backoff.
        assert!(sweep.next_request(&rooms, true, now + wait).is_none());
        assert!(
            sweep
                .next_request(&rooms, true, now + wait + FIRST)
                .is_some()
        );
    }

    /// A connection that drops with a request out, or a request that could not be sent, is a
    /// failure: the room is not asked again at once on the next connection.
    #[test]
    fn a_dropped_connection_or_a_send_error_is_a_failure() {
        let (live, _) = room();
        let rooms = [live];
        let now = Instant::now();

        let mut sweep = KeySweep::default();
        sweep.next_request(&rooms, true, now).unwrap();
        sweep.connection_lost(PACE, now);
        assert!(!sweep.busy());
        assert!(sweep.next_request(&rooms, true, now).is_none());
        assert!(sweep.next_request(&rooms, true, now + FIRST).is_some());

        let mut sweep = KeySweep::default();
        sweep.next_request(&rooms, true, now).unwrap();
        sweep.send_failed(PACE, now);
        assert!(sweep.next_request(&rooms, true, now).is_none());
        assert!(sweep.next_request(&rooms, true, now + FIRST).is_some());

        // A connection lost with nothing out is not a failure.
        let mut sweep = KeySweep::default();
        sweep.connection_lost(PACE, now);
        assert!(sweep.next_request(&rooms, true, now).is_some());
    }

    #[test]
    fn a_room_without_its_own_key_is_skipped() {
        let f = Fixture::new();
        let mut stored = f.stored(f.private_state());
        stored.self_sk = None;
        let rooms = [LiveRoom::new(stored)];
        let mut sweep = KeySweep::default();
        assert!(sweep.next_request(&rooms, true, Instant::now()).is_none());
    }

    // EnsureRoomSubscription: an owned room is registered with the delegate once its key is
    // the delegate's, once per room per connection.

    /// A sweep that may ask rooms to be followed (the generation watch confirmed the build).
    fn allowed() -> KeySweep {
        let mut sweep = KeySweep::default();
        sweep.allow_ensure(true);
        sweep
    }

    fn ensure_of(r: &ClientRequest<'_>) -> Option<([u8; 32], u64, [u8; 32])> {
        match msg(r) {
            ChatDelegateRequestMsg::EnsureRoomSubscription {
                room_owner_vk,
                request_id,
                contract_id,
            } => Some((room_owner_vk, request_id, contract_id)),
            _ => None,
        }
    }

    fn ensure_ok(owner: &VerifyingKey, request_id: u64) -> ChatDelegateResponseMsg {
        ChatDelegateResponseMsg::EnsureRoomSubscriptionResponse {
            room_owner_vk: owner.to_bytes(),
            request_id,
            result: Ok(()),
        }
    }

    /// Walk an owned room through its key check, as a delegate that already holds the key
    /// answers, and return the request that follows.
    fn owned_to_ensure(
        sweep: &mut KeySweep,
        rooms: &[LiveRoom],
        now: Instant,
    ) -> ClientRequest<'static> {
        let owner = rooms[0].stored.owner_vk;
        assert!(is_check(&sweep.next_request(rooms, true, now).unwrap()));
        assert!(sweep.on_answer(&got(&owner, Some(owner.to_bytes())), PACE, now));
        sweep
            .next_request(rooms, true, now)
            .expect("a request follows")
    }

    #[test]
    fn an_owned_room_whose_key_is_the_delegates_is_asked_to_be_followed() {
        let rooms = [room_of(9, 9)];
        let owner = rooms[0].stored.owner_vk;
        let mut sweep = allowed();
        let req = owned_to_ensure(&mut sweep, &rooms, Instant::now());
        let (vk, id, contract) = ensure_of(&req).expect("EnsureRoomSubscription");
        assert_eq!(vk, owner.to_bytes());
        assert_eq!(id, 1);
        assert_eq!(
            contract,
            crate::rooms::delegate::independent_contract_id(&owner)
        );
        assert!(sweep.busy());
    }

    /// A key that had to be stored first is asked about only after the check that follows the
    /// store confirmed it; a different key held means no ask.
    #[test]
    fn it_is_sent_only_after_the_key_is_verified() {
        let rooms = [room_of(9, 9)];
        let owner = rooms[0].stored.owner_vk;
        let now = Instant::now();
        let mut sweep = allowed();
        let req = sweep.next_request(&rooms, true, now).unwrap();
        assert!(is_check(&req));
        assert!(sweep.on_answer(&got(&owner, None), PACE, now));
        let req = sweep.next_request(&rooms, true, now).unwrap();
        assert!(matches!(
            msg(&req),
            ChatDelegateRequestMsg::StoreSigningKey { .. }
        ));
        assert!(sweep.on_answer(&stored_ok(&owner), PACE, now));
        let req = sweep.next_request(&rooms, true, now).unwrap();
        assert!(is_check(&req), "verify first");
        assert!(sweep.on_answer(&got(&owner, Some(owner.to_bytes())), PACE, now));
        let req = sweep.next_request(&rooms, true, now).unwrap();
        assert!(ensure_of(&req).is_some(), "ask after the verify");

        // A different key held: no ask, and nothing more.
        let mut sweep = allowed();
        sweep.next_request(&rooms, true, now).unwrap();
        assert!(sweep.on_answer(&got(&owner, Some([0xee; 32])), PACE, now));
        assert!(sweep.next_request(&rooms, true, now).is_none());
    }

    #[test]
    fn a_room_the_user_does_not_own_is_never_asked_to_be_followed() {
        let (live, ours) = room();
        let owner = live.stored.owner_vk;
        let rooms = [live];
        let mut sweep = allowed();
        let now = Instant::now();
        assert!(is_check(&sweep.next_request(&rooms, true, now).unwrap()));
        assert!(sweep.on_answer(
            &got(&owner, Some(ours.verifying_key().to_bytes())),
            PACE,
            now
        ));
        assert!(sweep.next_request(&rooms, true, now).is_none());
        sweep.connection_lost(PACE, now);
        assert!(sweep.next_request(&rooms, true, now).is_none());
    }

    #[test]
    fn an_owned_room_is_asked_once_whatever_the_answer() {
        let now = Instant::now();
        let rooms = [room_of(9, 9)];
        let owner = rooms[0].stored.owner_vk;
        for end in 0..3 {
            let mut sweep = allowed();
            let req = owned_to_ensure(&mut sweep, &rooms, now);
            let (_, id, _) = ensure_of(&req).unwrap();
            match end {
                0 => assert!(sweep.on_answer(&ensure_ok(&owner, id), PACE, now)),
                1 => assert!(sweep.on_answer(
                    &ChatDelegateResponseMsg::EnsureRoomSubscriptionResponse {
                        room_owner_vk: owner.to_bytes(),
                        request_id: id,
                        result: Err("no key".into()),
                    },
                    PACE,
                    now
                )),
                _ => assert!(sweep.on_error("boom", PACE, now)),
            }
            assert!(!sweep.busy());
            // Not again, however long it waits, and a refusal does not hold the sweep back.
            for s in [0, 60, 3600, 86_400] {
                assert!(
                    sweep
                        .next_request(&rooms, true, now + Duration::from_secs(s))
                        .is_none(),
                    "end {end}, after {s}s"
                );
            }
        }
    }

    /// An answer for another correlator or another room is not taken for the awaited one.
    #[test]
    fn another_rooms_or_requests_answer_is_not_taken() {
        let now = Instant::now();
        let rooms = [room_of(9, 9)];
        let owner = rooms[0].stored.owner_vk;
        let other = SigningKey::from_bytes(&[10; 32]).verifying_key();
        let mut sweep = allowed();
        let (_, id, _) = ensure_of(&owned_to_ensure(&mut sweep, &rooms, now)).unwrap();
        assert!(!sweep.on_answer(&ensure_ok(&owner, id + 1), PACE, now));
        assert!(!sweep.on_answer(&ensure_ok(&other, id), PACE, now));
        assert!(sweep.busy());
    }

    /// After a reconnect the room is asked again (as the web UI re-arms).
    #[test]
    fn it_is_asked_again_on_the_next_connection() {
        let now = Instant::now();
        let rooms = [room_of(9, 9)];
        let owner = rooms[0].stored.owner_vk;
        let mut sweep = allowed();
        let (_, id, _) = ensure_of(&owned_to_ensure(&mut sweep, &rooms, now)).unwrap();
        assert!(sweep.on_answer(&ensure_ok(&owner, id), PACE, now));
        assert!(sweep.next_request(&rooms, true, now).is_none());
        sweep.connection_lost(PACE, now);
        let again = sweep.next_request(&rooms, true, now).expect("asked again");
        let (_, id2, _) = ensure_of(&again).expect("an ask, not a key check");
        assert_eq!(id2, id + 1);
    }

    /// An ask that fails is never repeated this run, whatever follows: the sequences are the
    /// session's (an unanswered ask is expired and the connection kept; a failed send is
    /// followed by the connection's loss; an error answer, then a loss).
    #[test]
    fn a_failed_ask_is_never_repeated_and_never_drops_the_connection() {
        let now = Instant::now();
        let later = now + Duration::from_secs(3600);
        let rooms = [room_of(9, 9)];
        let wait = Duration::from_secs(20);

        // Unanswered: expired without dropping the connection; nothing repeats it.
        let mut sweep = allowed();
        ensure_of(&owned_to_ensure(&mut sweep, &rooms, now)).unwrap();
        assert!(
            !sweep.expire(wait, PACE, now + wait),
            "the connection stays"
        );
        assert!(!sweep.busy());
        assert!(sweep.next_request(&rooms, true, later).is_none());
        sweep.connection_lost(PACE, later);
        assert!(sweep.next_request(&rooms, true, later).is_none());

        // Unsendable, then the connection goes.
        let mut sweep = allowed();
        ensure_of(&owned_to_ensure(&mut sweep, &rooms, now)).unwrap();
        sweep.send_failed(PACE, now);
        sweep.connection_lost(PACE, now);
        assert!(sweep.next_request(&rooms, true, later).is_none());

        // An error answer, then a loss.
        let mut sweep = allowed();
        ensure_of(&owned_to_ensure(&mut sweep, &rooms, now)).unwrap();
        assert!(sweep.on_error("boom", PACE, now));
        sweep.connection_lost(PACE, now);
        assert!(sweep.next_request(&rooms, true, later).is_none());

        // Lost with the ask out.
        let mut sweep = allowed();
        ensure_of(&owned_to_ensure(&mut sweep, &rooms, now)).unwrap();
        sweep.connection_lost(PACE, now);
        assert!(sweep.next_request(&rooms, true, later).is_none());

        // Over a day of failing asks, one ask in all, and no sweep-wide wait is started.
        let mut sweep = allowed();
        let mut asks = 0;
        for second in (0..24 * 3600u64).step_by(7) {
            let t = now + Duration::from_secs(second);
            if let Some(r) = sweep.next_request(&rooms, true, t) {
                if ensure_of(&r).is_some() {
                    asks += 1;
                    sweep.expire(wait, PACE, t + wait);
                } else {
                    sweep.on_answer(
                        &got(
                            &rooms[0].stored.owner_vk,
                            Some(rooms[0].stored.owner_vk.to_bytes()),
                        ),
                        PACE,
                        t,
                    );
                }
            }
        }
        assert_eq!(asks, 1);
    }

    /// A key request that goes unanswered still drops the connection.
    #[test]
    fn an_unanswered_key_request_still_drops_the_connection() {
        let now = Instant::now();
        let rooms = [room_of(9, 9)];
        let mut sweep = allowed();
        assert!(is_check(&sweep.next_request(&rooms, true, now).unwrap()));
        let wait = Duration::from_secs(20);
        assert!(sweep.expire(wait, PACE, now + wait));
    }

    /// Without the watch's confirmation of this build's room contract, nothing is asked; with
    /// it, the room is asked at once.
    #[test]
    fn nothing_is_asked_until_the_room_contract_is_confirmed_current() {
        let now = Instant::now();
        let rooms = [room_of(9, 9)];
        let owner = rooms[0].stored.owner_vk;
        let mut sweep = KeySweep::default();
        assert!(is_check(&sweep.next_request(&rooms, true, now).unwrap()));
        assert!(sweep.on_answer(&got(&owner, Some(owner.to_bytes())), PACE, now));
        for s in [0, 10, 3600] {
            assert!(
                sweep
                    .next_request(&rooms, true, now + Duration::from_secs(s))
                    .is_none()
            );
        }
        sweep.allow_ensure(true);
        let req = sweep.next_request(&rooms, true, now).expect("asked");
        assert!(ensure_of(&req).is_some());
        // Withdrawn before it is sent: not sent.
        let mut sweep = KeySweep::default();
        sweep.allow_ensure(true);
        assert!(is_check(&sweep.next_request(&rooms, true, now).unwrap()));
        sweep.on_answer(&got(&owner, Some(owner.to_bytes())), PACE, now);
        sweep.allow_ensure(false);
        assert!(sweep.next_request(&rooms, true, now).is_none());
    }
}
