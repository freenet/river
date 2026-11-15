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
//! **A key that is there is never replaced.** The delegate overwrites without comparing
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
//! itself.
//!
//! **One request out at a time, and never beside another.** Delegate answers carry no request
//! id. The sweep sends only when no task works and no read is out, a task or read starts only
//! when the sweep has nothing out, and an answer is taken only if it is the kind awaited for
//! the room awaited. A request unanswered for the delegate deadline drops the connection, as
//! for a read.
//!
//! **Failures retry later, never in a loop.** An error answer (`DelegateError::Other` from
//! the real handlers, `Missing` before the store is registered again), a refusal, an answer
//! that does not confirm the key, or no answer, counts against that room. It is asked again
//! after a wait that starts at [`super::session::Pace::key_retry_first`] and doubles to
//! `key_retry_max`, so a delegate that keeps refusing is asked about once per cap. Nothing is
//! shown: the app works without the key.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use ed25519_dalek::VerifyingKey;
use freenet_stdlib::client_api::ClientRequest;
use river_core::chat_delegate::ChatDelegateResponseMsg;

use super::delegate;
use super::room::LiveRoom;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Ask which key the delegate holds.
    Check,
    /// It holds none: store ours.
    Store,
    /// Stored: ask again.
    Verify,
}

/// What the sweep is doing now, for one room.
#[derive(Clone, Copy, Debug)]
struct Current {
    room: VerifyingKey,
    phase: Phase,
    /// When the request for this phase went out, if it has.
    sent: Option<Instant>,
}

/// Failures so far for a room, and when it may be asked again.
#[derive(Clone, Copy, Debug)]
struct Backoff {
    failures: u32,
    not_before: Instant,
}

#[derive(Default)]
pub struct KeySweep {
    current: Option<Current>,
    /// Rooms whose key the delegate holds, this run.
    verified: HashSet<VerifyingKey>,
    /// Rooms the delegate holds a different key for: left alone, named once.
    mismatched: HashSet<VerifyingKey>,
    backoff: HashMap<VerifyingKey, Backoff>,
}

/// How long to wait after `failures` failures in a row: `first` doubling, up to `max`.
pub fn wait_after(failures: u32, first: Duration, max: Duration) -> Duration {
    first
        .saturating_mul(1u32 << failures.saturating_sub(1).min(16))
        .min(max)
}

impl KeySweep {
    /// Whether a request is out: nothing else may be sent to the delegate meanwhile.
    pub fn busy(&self) -> bool {
        self.current.is_some_and(|c| c.sent.is_some())
    }

    /// The connection is gone, and with it whatever was out; the room's step starts again from
    /// its check.
    pub fn connection_lost(&mut self) {
        self.current = None;
    }

    /// The next request, if the sweep may send one now (`idle`: no task works and no read is
    /// out, on a complete read of the rooms) and a room needs it.
    pub fn next_request(
        &mut self,
        rooms: &[LiveRoom],
        idle: bool,
        now: Instant,
    ) -> Option<ClientRequest<'static>> {
        if !idle || self.busy() {
            return None;
        }
        if self.current.is_none() {
            let room = rooms.iter().find_map(|r| {
                let vk = r.stored.owner_vk;
                let due = self.backoff.get(&vk).is_none_or(|b| b.not_before <= now);
                (r.stored.self_sk.is_some()
                    && due
                    && !self.verified.contains(&vk)
                    && !self.mismatched.contains(&vk))
                .then_some(vk)
            })?;
            self.current = Some(Current {
                room,
                phase: Phase::Check,
                sent: None,
            });
        }
        let cur = self.current.as_mut()?;
        let Some(sk) = rooms
            .iter()
            .find(|r| r.stored.owner_vk == cur.room)
            .and_then(|r| r.stored.self_sk.as_ref())
        else {
            // The room is gone from the list meanwhile.
            self.current = None;
            return None;
        };
        let request = match cur.phase {
            Phase::Check | Phase::Verify => delegate::public_key(&cur.room),
            Phase::Store => delegate::store_signing_key(&cur.room, sk),
        };
        cur.sent = Some(now);
        Some(request)
    }

    /// A chat delegate answer. True when it was the awaited one.
    pub fn on_answer(
        &mut self,
        answer: &ChatDelegateResponseMsg,
        rooms: &[LiveRoom],
        pace: (Duration, Duration),
        now: Instant,
    ) -> bool {
        let Some(cur) = self.current.filter(|c| c.sent.is_some()) else {
            return false;
        };
        let ours = rooms
            .iter()
            .find(|r| r.stored.owner_vk == cur.room)
            .and_then(|r| r.stored.self_sk.as_ref())
            .map(|sk| sk.verifying_key().to_bytes());
        match (cur.phase, answer) {
            (
                Phase::Check | Phase::Verify,
                ChatDelegateResponseMsg::GetPublicKeyResponse {
                    room_key,
                    public_key,
                },
            ) if *room_key == cur.room.to_bytes() => match (cur.phase, public_key) {
                (Phase::Check, None) => self.go(Phase::Store),
                (_, Some(held)) if Some(*held) == ours => self.done(cur.room),
                (Phase::Check, Some(_)) => {
                    let name = rooms
                        .iter()
                        .find(|r| r.stored.owner_vk == cur.room)
                        .map_or_else(String::new, LiveRoom::name);
                    eprintln!(
                        "River: the delegate holds a different signing key for the room \
                         \"{name}\"; leaving it for River in your browser to replace"
                    );
                    self.current = None;
                    self.mismatched.insert(cur.room);
                }
                // Stored, but not what the delegate now holds.
                (_, _) => self.fail(cur.room, pace, now),
            },
            (
                Phase::Store,
                ChatDelegateResponseMsg::StoreSigningKeyResponse { room_key, result },
            ) if *room_key == cur.room.to_bytes() => match result {
                Ok(()) => self.go(Phase::Verify),
                Err(e) => {
                    eprintln!("River: the delegate refused a room's signing key: {e}");
                    self.fail(cur.room, pace, now)
                }
            },
            _ => return false,
        }
        true
    }

    /// An error answer to the awaited request. True when it was taken (the caller checked that
    /// nothing else could have caused it).
    pub fn on_error(&mut self, why: &str, pace: (Duration, Duration), now: Instant) -> bool {
        let Some(cur) = self.current.filter(|c| c.sent.is_some()) else {
            return false;
        };
        eprintln!("River: asking the delegate about a room's signing key failed: {why}");
        self.fail(cur.room, pace, now);
        true
    }

    /// Whether the awaited request has gone unanswered past `wait`, so the connection must be
    /// dropped (its answer must not arrive late and be taken for another request's).
    pub fn expire(&mut self, wait: Duration, pace: (Duration, Duration), now: Instant) -> bool {
        let Some(cur) = self.current else {
            return false;
        };
        match cur.sent {
            Some(at) if now.saturating_duration_since(at) >= wait => {
                self.fail(cur.room, pace, now);
                true
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

    fn done(&mut self, room: VerifyingKey) {
        self.current = None;
        self.backoff.remove(&room);
        self.verified.insert(room);
    }

    fn fail(&mut self, room: VerifyingKey, (first, max): (Duration, Duration), now: Instant) {
        self.current = None;
        let failures = self.backoff.get(&room).map_or(0, |b| b.failures) + 1;
        self.backoff.insert(
            room,
            Backoff {
                failures,
                not_before: now + wait_after(failures, first, max),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rooms::room_tests::Fixture;
    use freenet_stdlib::client_api::DelegateRequest;
    use freenet_stdlib::prelude::InboundDelegateMsg;
    use river_core::chat_delegate::ChatDelegateRequestMsg;

    const FIRST: Duration = Duration::from_secs(60);
    const MAX: Duration = Duration::from_secs(30 * 60);
    const PACE: (Duration, Duration) = (FIRST, MAX);

    fn room() -> (LiveRoom, ed25519_dalek::SigningKey) {
        let f = Fixture::new();
        let stored = f.stored(f.private_state());
        (LiveRoom::new(stored), f.me.clone())
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
    /// number in a day: 7 asks in the first 61 minutes (at 0, 1, 3, 7, 15, 31 and 61 minutes), then
    /// one every 30 minutes.
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
        let other = ed25519_dalek::SigningKey::from_bytes(&[9; 32]).verifying_key();
        assert!(!sweep.on_answer(&got(&other, None), &rooms, PACE, now));
        assert!(sweep.on_answer(&got(&owner, None), &rooms, PACE, now));
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
        assert!(sweep.on_answer(
            &ChatDelegateResponseMsg::StoreSigningKeyResponse {
                room_key: owner.to_bytes(),
                result: Ok(())
            },
            &rooms,
            PACE,
            now
        ));
        let r = sweep.next_request(&rooms, true, now).unwrap();
        assert!(matches!(
            msg(&r),
            ChatDelegateRequestMsg::GetPublicKey { .. }
        ));
        assert!(sweep.on_answer(
            &got(&owner, Some(sk.verifying_key().to_bytes())),
            &rooms,
            PACE,
            now
        ));
        // Done for this run.
        assert!(sweep.next_request(&rooms, true, now + MAX * 4).is_none());
    }

    #[test]
    fn a_different_key_is_left_alone_and_never_asked_about_again() {
        let (live, _) = room();
        let owner = live.stored.owner_vk;
        let rooms = [live];
        let mut sweep = KeySweep::default();
        let now = Instant::now();
        sweep.next_request(&rooms, true, now).unwrap();
        assert!(sweep.on_answer(&got(&owner, Some([7; 32])), &rooms, PACE, now));
        assert!(sweep.next_request(&rooms, true, now + MAX * 4).is_none());
        assert!(!sweep.busy());
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
        sweep.on_answer(&got(&owner, None), &rooms, PACE, now);
        sweep.next_request(&rooms, true, now).unwrap();
        sweep.on_answer(
            &ChatDelegateResponseMsg::StoreSigningKeyResponse {
                room_key: owner.to_bytes(),
                result: Ok(()),
            },
            &rooms,
            PACE,
            now,
        );
        sweep.next_request(&rooms, true, now).unwrap();
        sweep.on_answer(&got(&owner, None), &rooms, PACE, now);
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
        sweep.connection_lost();
        assert!(!sweep.busy());
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
}
