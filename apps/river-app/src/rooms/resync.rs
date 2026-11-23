//! Reading a room again on a live connection (freenet/river#971).
//!
//! A subscription delivers each change of a room, but a notification can be lost: dropped by
//! the node when this client's channel is full, lost between the node's read of the state and
//! its registering of the listener, or lost to a transport fault. The node sends the whole state
//! with every notification, so the next change of the room repairs a lost one; a quiet room
//! stays wrong until then, however long that is. The web UI reads the open room again after 60
//! seconds of silence and every room when its page becomes visible (`connection_watchdog.rs`,
//! freenet/river#382). This does the same, with three differences:
//!
//! - **Silence is per room.** The open room is read again when the node has said nothing about
//!   THAT room for [`Pace::resync_idle`], not when the whole connection is silent: another
//!   busy room does not keep a stalled open room stale. The quiet clock restarts when River
//!   asks, so the open room is read again at most once per `resync_idle`.
//! - **Coming to the front** (the window gains focus) reads again every room the node has said
//!   nothing about for [`Pace::resync_focus`], once each. A window that flips focus often reads
//!   nothing more, since each room's quiet clock was just restarted. There is no wake-from-sleep
//!   signal in the app: after a sleep the ping either fails (a new connection subscribes, and so
//!   reads, every room afresh) or the idle rule reads the open room.
//! - **A merge that fails** (a notification, or the answer to a read or subscribe) is followed by
//!   a read of that room, again after [`Pace::resubscribe_first`] doubling to
//!   [`Pace::resubscribe_max`] with +-20% jitter while it keeps failing, and from the start once
//!   one works.
//!
//! What bounds the load: at most one read out per room. A room is not read while its subscribe
//! is out or waiting to be asked again (that ask reads it), while it owes the answer to an
//! UPDATE or a create or join holds it (an error that names no room could then be either's;
//! see [`super::session`]), and no read is sent while the stored rooms are being read, the
//! signing key sweep has a request out, or a create or join has one out, for the same reason.
//!
//! A read the node does not answer is given up after [`Pace::resync_wait`], which only frees the
//! room for the next read. It NEVER reconnects, and it is not an answer owed in the sense of
//! [`super::outbox`]'s reconnect rule (`wants_reconnect`, `owed_limit`): that rule exists because
//! an UPDATE's answer names only the room, so a room that owes one cannot send another on the
//! same connection. A read's answer names its room and merges idempotently, so a late one is
//! harmless. Whether the node is there at all is the ping's job ([`Pace::idle_check`],
//! [`Pace::pong_wait`]), and a node that answers pings but not reads would not answer them any
//! better on a new connection. So an unanswered read costs one read per room per
//! `max(resync_idle, resync_wait)`, and reconnects nobody.
//!
//! The reads go out as GETs that do not subscribe. The node's answer to one looks exactly like
//! its answer to a subscribe (a `GetResponse`), so the two are never out at once for one room:
//! no read while a subscribe is out or due, and no subscribe asked while a read is out. The one
//! overlap left is a read given up after `resync_wait` whose answer comes later still, after a
//! subscribe for the same room was asked; that answer is taken for the subscribe's. The room
//! merges the state either way; if the subscribe in fact fails, its error names it on screen,
//! and if it never answers, the idle rule keeps the open room read.
//!
//! [`Pace::resync_idle`]: super::session::Pace
//! [`Pace::resync_focus`]: super::session::Pace
//! [`Pace::resync_wait`]: super::session::Pace
//! [`Pace::resubscribe_first`]: super::session::Pace
//! [`Pace::resubscribe_max`]: super::session::Pace
//! [`Pace::idle_check`]: super::session::Pace
//! [`Pace::pong_wait`]: super::session::Pace

use std::collections::HashMap;
use std::time::{Duration, Instant};

use freenet_stdlib::prelude::ContractInstanceId;
use rand::Rng;

/// How many merges in a row may fail with the same error before the room is no longer read
/// again for it on this connection: the node sends the same whole state each time, so a
/// failure that repeats is one a read cannot heal. The room stays named as not updated; a
/// notification still merges, a different failure or a merge that works starts over, and a new
/// connection reads every room afresh.
pub(super) const HEAL_GIVE_UP: u32 = 3;

/// The pacing [`Resyncs`] needs, taken from the session's `Pace`.
#[derive(Clone, Copy, Debug)]
pub(super) struct ResyncPace {
    pub idle: Duration,
    pub focus: Duration,
    /// Coming to the front spreads its reads over up to this long, at random, so a window that
    /// comes to the front does not send one read per room in the same instant.
    pub spread: Duration,
    pub heal_first: Duration,
    pub heal_max: Duration,
}

/// A room whose last merge failed.
struct Heal {
    /// Reads asked to heal it so far.
    asked: u32,
    /// When to read it again; `None` once given up ([`HEAL_GIVE_UP`]).
    at: Option<Instant>,
    /// The last merge's error, and how many merges in a row failed with it.
    error: String,
    same: u32,
}

/// The rooms read again on one connection. A new connection subscribes to, and so reads, every
/// room: it starts with a fresh one.
pub(super) struct Resyncs {
    /// Reads out, with when they were asked.
    out: HashMap<ContractInstanceId, Instant>,
    /// When the node last said anything about each room, or River last asked it to: the room's
    /// quiet clock. A room with none counts from when the connection was set up.
    heard: HashMap<ContractInstanceId, Instant>,
    heal: HashMap<ContractInstanceId, Heal>,
    /// Rooms to read again because the window came to the front, each with when its read may go.
    wanted: HashMap<ContractInstanceId, Instant>,
    started: Instant,
}

impl Resyncs {
    pub fn new(now: Instant) -> Self {
        Resyncs {
            out: HashMap::new(),
            heard: HashMap::new(),
            heal: HashMap::new(),
            wanted: HashMap::new(),
            started: now,
        }
    }

    /// The node said something about the room (a notification, or an answer to a subscribe).
    pub fn heard(&mut self, id: &ContractInstanceId, now: Instant) {
        self.heard.insert(*id, now);
    }

    /// A `GetResponse` for the room. True when it answers a read of River's (which is then no
    /// longer out); false when it is a subscribe's answer.
    pub fn answered(&mut self, id: &ContractInstanceId, now: Instant) -> bool {
        self.heard(id, now);
        self.out.remove(id).is_some()
    }

    pub fn is_out(&self, id: &ContractInstanceId) -> bool {
        self.out.contains_key(id)
    }

    pub fn any_out(&self) -> bool {
        !self.out.is_empty()
    }

    /// The one read out, when exactly one is.
    pub fn lone(&self) -> Option<ContractInstanceId> {
        match self.out.keys().collect::<Vec<_>>()[..] {
            [id] => Some(*id),
            _ => None,
        }
    }

    /// A read failed (the node's error, or nothing found). The room is free for the next read,
    /// by the usual rules.
    pub fn failed(&mut self, id: &ContractInstanceId) {
        self.out.remove(id);
    }

    /// The outcome of merging what the node sent about the room. A failure schedules a read,
    /// after a wait that doubles with each read already asked to heal it, unless it is the
    /// [`HEAL_GIVE_UP`]th in a row with the same error; a success forgets earlier failures.
    pub fn merged(
        &mut self,
        id: ContractInstanceId,
        result: &Result<(), String>,
        now: Instant,
        pace: &ResyncPace,
    ) {
        let error = match result {
            Ok(()) => {
                self.heal.remove(&id);
                return;
            }
            Err(e) => e,
        };
        let heal = self.heal.entry(id).or_insert_with(|| Heal {
            asked: 0,
            at: None,
            error: String::new(),
            same: 0,
        });
        if heal.error == *error {
            heal.same = heal.same.saturating_add(1);
        } else {
            heal.error = error.clone();
            heal.same = 1;
        }
        heal.at = (heal.same < HEAL_GIVE_UP).then(|| now + heal_wait(heal.asked, pace));
    }

    /// The window came to the front: every room in `rooms` is to be read again, each at a random
    /// moment within `pace.spread`, if the node has said nothing about it for `pace.focus` by
    /// then.
    pub fn focus(&mut self, rooms: &[ContractInstanceId], now: Instant, pace: &ResyncPace) {
        for id in rooms {
            self.wanted
                .entry(*id)
                .or_insert_with(|| now + pace.spread.mul_f64(rand::thread_rng().gen_range(0.0..=1.0)));
        }
    }

    /// Give up reads out for `wait` or longer.
    pub fn expire(&mut self, wait: Duration, now: Instant) {
        self.out
            .retain(|_, asked| now.saturating_duration_since(*asked) < wait);
    }

    /// Forget rooms no longer shown.
    pub fn keep_only(&mut self, shown: &[ContractInstanceId]) {
        self.out.retain(|id, _| shown.contains(id));
        self.heard.retain(|id, _| shown.contains(id));
        self.heal.retain(|id, _| shown.contains(id));
        self.wanted.retain(|id, _| shown.contains(id));
    }

    fn quiet(&self, id: &ContractInstanceId, now: Instant) -> Duration {
        now.saturating_duration_since(*self.heard.get(id).unwrap_or(&self.started))
    }

    /// The rooms of `rooms` to read now, each marked out and its quiet clock restarted: those
    /// whose failed merge is due for a read, those wanted by coming to the front whose moment
    /// has come and still quiet for `pace.focus`, and the open room `viewing` when quiet for
    /// `pace.idle`. Never a room with a read out, nor one `blocked` says may not be read now.
    pub fn due(
        &mut self,
        viewing: Option<ContractInstanceId>,
        rooms: &[ContractInstanceId],
        blocked: &dyn Fn(&ContractInstanceId) -> bool,
        now: Instant,
        pace: &ResyncPace,
    ) -> Vec<ContractInstanceId> {
        // A room wanted by coming to the front but heard from by its moment is not read again.
        let fresh: Vec<ContractInstanceId> = self
            .wanted
            .iter()
            .filter(|(id, at)| **at <= now && self.quiet(id, now) < pace.focus)
            .map(|(id, _)| *id)
            .collect();
        for id in fresh {
            self.wanted.remove(&id);
        }
        let mut due = Vec::new();
        for id in rooms {
            if self.out.contains_key(id) || blocked(id) {
                continue;
            }
            let heal = self
                .heal
                .get(id)
                .is_some_and(|h| h.at.is_some_and(|at| at <= now));
            let wanted = self.wanted.get(id).is_some_and(|at| *at <= now);
            let idle = viewing.as_ref() == Some(id) && self.quiet(id, now) >= pace.idle;
            if !(heal || wanted || idle) {
                continue;
            }
            // A heal read schedules the next one now, not when its answer merges: one that fails
            // or goes unanswered must not be asked again at once, on every tick. A read for
            // another reason leaves the heal's schedule as it is.
            if heal && let Some(h) = self.heal.get_mut(id) {
                h.asked = h.asked.saturating_add(1);
                h.at = Some(now + heal_wait(h.asked, pace));
            }
            if wanted {
                self.wanted.remove(id);
            }
            self.out.insert(*id, now);
            self.heard.insert(*id, now);
            due.push(*id);
        }
        due
    }
}

/// How long after a failed merge the room is read again, `asked` reads in: `heal_first`
/// doubling to `heal_max`, jittered.
fn heal_wait(asked: u32, pace: &ResyncPace) -> Duration {
    jittered(
        pace.heal_first
            .saturating_mul(1 << asked.min(16))
            .min(pace.heal_max),
    )
}

/// `wait` +-20%, so rooms that failed together are not read again together.
fn jittered(wait: Duration) -> Duration {
    wait.mul_f64(rand::thread_rng().gen_range(0.8..=1.2))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> ContractInstanceId {
        ContractInstanceId::new([n; 32])
    }

    fn pace() -> ResyncPace {
        ResyncPace {
            idle: Duration::from_secs(60),
            focus: Duration::from_secs(30),
            spread: Duration::ZERO,
            heal_first: Duration::from_secs(5),
            heal_max: Duration::from_secs(300),
        }
    }

    const FREE: &dyn Fn(&ContractInstanceId) -> bool = &|_| false;

    #[test]
    fn the_open_room_is_read_once_per_idle_and_never_twice_at_once() {
        let t0 = Instant::now();
        let mut r = Resyncs::new(t0);
        let rooms = [id(1), id(2)];
        let p = pace();
        assert!(r.due(Some(id(1)), &rooms, FREE, t0, &p).is_empty());
        let t1 = t0 + p.idle;
        assert_eq!(r.due(Some(id(1)), &rooms, FREE, t1, &p), [id(1)]);
        // Out: not again, however long it stays out.
        assert!(
            r.due(Some(id(1)), &rooms, FREE, t1 + p.idle * 3, &p)
                .is_empty()
        );
        // Given up: the quiet clock restarted when it was asked.
        r.expire(Duration::from_secs(45), t1 + Duration::from_secs(45));
        assert!(!r.is_out(&id(1)));
        assert_eq!(r.due(Some(id(1)), &rooms, FREE, t1 + p.idle, &p), [id(1)]);
        // Answered: the next one waits a whole idle from the answer.
        let t2 = t1 + p.idle + Duration::from_secs(1);
        assert!(r.answered(&id(1), t2));
        assert!(
            r.due(
                Some(id(1)),
                &rooms,
                FREE,
                t2 + p.idle - Duration::from_millis(1),
                &p
            )
            .is_empty()
        );
    }

    #[test]
    fn a_blocked_room_waits_and_a_subscribe_answer_is_not_a_read() {
        let t0 = Instant::now();
        let mut r = Resyncs::new(t0);
        let p = pace();
        let blocked: &dyn Fn(&ContractInstanceId) -> bool = &|i| *i == id(1);
        assert!(
            r.due(Some(id(1)), &[id(1)], blocked, t0 + p.idle, &p)
                .is_empty()
        );
        assert!(!r.answered(&id(1), t0 + p.idle), "nothing was out");
    }

    #[test]
    fn coming_to_the_front_reads_each_quiet_room_once() {
        let t0 = Instant::now();
        let mut r = Resyncs::new(t0);
        let p = pace();
        let rooms = [id(1), id(2)];
        let t1 = t0 + p.focus;
        r.heard(&id(2), t1 - Duration::from_secs(1));
        r.focus(&rooms, t1, &p);
        assert_eq!(r.due(None, &rooms, FREE, t1, &p), [id(1)]);
        r.answered(&id(1), t1);
        // Room 2 was heard from lately and is dropped, not kept for later.
        assert!(r.due(None, &rooms, FREE, t1 + p.focus * 2, &p).is_empty());
        r.focus(&rooms, t1, &p);
        assert!(r.due(None, &rooms, FREE, t1, &p).is_empty(), "just heard");
    }

    #[test]
    fn a_failed_merge_is_read_again_with_a_growing_jittered_wait() {
        let t0 = Instant::now();
        let mut r = Resyncs::new(t0);
        let p = pace();
        let rooms = [id(1)];
        r.merged(id(1), &Err("bad".into()), t0, &p);
        assert!(
            r.due(None, &rooms, FREE, t0 + p.heal_first.mul_f64(0.79), &p)
                .is_empty()
        );
        assert_eq!(
            r.due(None, &rooms, FREE, t0 + p.heal_first.mul_f64(1.21), &p),
            [id(1)]
        );
        let t1 = t0 + p.heal_first * 2;
        r.answered(&id(1), t1);
        r.merged(id(1), &Err("bad".into()), t1, &p);
        let second = p.heal_first * 2;
        assert!(
            r.due(None, &rooms, FREE, t1 + second.mul_f64(0.79), &p)
                .is_empty()
        );
        assert_eq!(
            r.due(None, &rooms, FREE, t1 + second.mul_f64(1.21), &p),
            [id(1)]
        );
        // Reads that fail with an error are not asked again at once, and the wait keeps
        // growing, to the cap.
        let mut t = t1 + second.mul_f64(1.21);
        r.failed(&id(1));
        assert!(r.due(None, &rooms, FREE, t, &p).is_empty(), "not at once");
        let mut waits = Vec::new();
        for _ in 0..12 {
            let next = (1..)
                .map(|ms| t + Duration::from_millis(ms * 50))
                .find(|at| !r.due(None, &rooms, FREE, *at, &p).is_empty())
                .unwrap();
            waits.push(next - t);
            r.failed(&id(1));
            t = next;
        }
        assert!(waits[1] > waits[0].mul_f64(1.2), "{waits:?}");
        assert!(
            waits.iter().all(|w| *w <= p.heal_max.mul_f64(1.21)),
            "{waits:?}"
        );
        assert!(
            waits.last().unwrap() >= &p.heal_max.mul_f64(0.79),
            "{waits:?}"
        );
        // A merge that works forgets it.
        r.answered(&id(1), t1);
        r.merged(id(1), &Ok(()), t1, &p);
        assert!(
            r.due(None, &rooms, FREE, t1 + p.heal_max * 10, &p)
                .is_empty()
        );
    }

    #[test]
    fn rooms_no_longer_shown_are_forgotten() {
        let t0 = Instant::now();
        let mut r = Resyncs::new(t0);
        let p = pace();
        r.merged(id(1), &Err("bad".into()), t0, &p);
        r.focus(&[id(1)], t0, &p);
        r.due(None, &[id(1)], FREE, t0, &p);
        r.heard(&id(1), t0);
        r.keep_only(&[id(2)]);
        assert!(!r.is_out(&id(1)) && !r.heard.contains_key(&id(1)));
        assert!(
            r.due(None, &[id(1)], FREE, t0 + p.heal_max * 2, &p)
                .is_empty()
        );
    }

    #[test]
    fn coming_to_the_front_spreads_its_reads() {
        let t0 = Instant::now();
        let mut r = Resyncs::new(t0);
        let p = ResyncPace {
            spread: Duration::from_secs(10),
            ..pace()
        };
        let rooms: Vec<ContractInstanceId> = (1..=20).map(id).collect();
        let t1 = t0 + p.focus;
        r.focus(&rooms, t1, &p);
        let at_once = r.due(None, &rooms, FREE, t1, &p).len();
        assert!(at_once < 20, "all {at_once} at once");
        let by_end = r.due(None, &rooms, FREE, t1 + p.spread, &p).len();
        assert_eq!(at_once + by_end, 20, "each read once, within the spread");
    }

    #[test]
    fn a_failure_that_repeats_is_read_again_a_few_times_then_left() {
        let t0 = Instant::now();
        let mut r = Resyncs::new(t0);
        let p = pace();
        let rooms = [id(1)];
        let same: Result<(), String> = Err("refused".into());
        let mut t = t0;
        let mut reads = 0;
        for _ in 0..10 {
            r.merged(id(1), &same, t, &p);
            t += p.heal_max * 2;
            if r.due(None, &rooms, FREE, t, &p).is_empty() {
                break;
            }
            reads += 1;
            r.answered(&id(1), t);
        }
        assert_eq!(reads, HEAL_GIVE_UP - 1, "read after each of the failures but the last");
        // A different failure starts over; so does a merge that works.
        r.merged(id(1), &Err("another".into()), t, &p);
        assert_eq!(r.due(None, &rooms, FREE, t + p.heal_max * 2, &p), [id(1)]);
        r.answered(&id(1), t);
        r.merged(id(1), &Ok(()), t, &p);
        assert!(r.heal.is_empty());
    }

    #[test]
    fn an_idle_or_focus_read_leaves_the_heal_schedule_alone() {
        let t0 = Instant::now();
        let mut r = Resyncs::new(t0);
        let p = pace();
        let rooms = [id(1)];
        r.merged(id(1), &Err("bad".into()), t0, &p);
        // The open room falls quiet before its heal is due: that read is not a heal.
        let quiet = t0 + p.idle;
        r.heal.get_mut(&id(1)).unwrap().at = Some(quiet + p.heal_first);
        assert_eq!(r.due(Some(id(1)), &rooms, FREE, quiet, &p), [id(1)]);
        assert_eq!(r.heal[&id(1)].asked, 0);
    }
}

