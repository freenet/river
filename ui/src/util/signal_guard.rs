//! Keeping a fallible-read memo reactive (freenet/river#555).
//!
//! ## The defect this exists to prevent
//!
//! A `use_memo` whose closure early-returns on a contended `try_read()` can lose
//! the subscription to the very signal it depends on, and then never re-evaluate.
//! Two facts from the Dioxus 0.7.9 source combine to cause it:
//!
//! 1. **A memo rebuilds its dependency set on every pass.** `Memo` recomputes via
//!    `rc.reset_and_run_in(&mut f)` (`dioxus-signals-0.7.9/src/memo.rs:62`), and
//!    `reset_and_run_in` is `clear_subscribers()` followed by `run_in(f)`
//!    (`dioxus-core-0.7.9/src/reactive_context.rs:195-198`). Whatever the closure
//!    does not read on a given pass is not a dependency after that pass.
//!
//! 2. **`try_read()` does not subscribe when it fails.**
//!    `Signal::try_read_unchecked` propagates the borrow error with `?` *before*
//!    reaching `reactive_context.subscribe(...)`
//!    (`dioxus-signals-0.7.9/src/signal.rs:409` vs `:413`).
//!
//! So a pass that hits `Err` and returns early ends up subscribed only to
//! whatever it *did* read successfully — and if that is nothing, to nothing at
//! all. The memo is then permanently stuck: no write to the signal can wake it.
//!
//! What actually holds the borrow across a recompute is NOT established, and the
//! explanation in `.claude/rules/dioxus-signal-safety.md` ("a write's Drop fires
//! subscriber notifications synchronously, and those notifications poll memos
//! that `try_read()` the signal still being written") does not hold in 0.7.9:
//! `WriteLock`'s borrow field drops before its `SignalSubscriberDrop`
//! (`write.rs:164`), `update_subscribers` takes a fresh read
//! (`signal.rs:260`, which would panic on every write otherwise), and
//! `mark_dirty` for a memo only sets a flag and sends on a channel
//! (`memo.rs:53`) — the recompute happens later in a task. So the notification
//! path is not the contending writer.
//!
//! Contention is nonetheless real and observed: `signal.rs:262` notes outright
//! that "mark_dirty can run user code", and this repo has a recorded instance
//! (`sync_info.rs`'s `rooms_awaiting_subscription` bails on a contended `ROOMS`
//! because `process_rooms()` gets driven during a write-guard drop). river#499
//! (DM rail collapsed to empty) and river#555 (conversation showed "No messages
//! yet" in a full room until reload) are the two user-visible instances.
//!
//! Treat the trigger as INFERRED, not diagnosed. A single `Err` is enough to
//! latch a memo permanently, so this guard is worth having either way — but
//! naming the contending writer is still open work, and until it is named the
//! nudge frequency in the field is unknown.
//!
//! ## The pattern
//!
//! Generalised from the per-file version `dm_rail_section.rs` grew for #499:
//!
//! ```ignore
//! let thing = use_memo(move || {
//!     crate::util::signal_guard::anchor();          // FIRST, before any try_read
//!     let Ok(rooms) = ROOMS.try_read() else {
//!         crate::util::signal_guard::schedule_nudge();
//!         return None;                              // or a last-good value
//!     };
//!     ...
//! });
//! ```
//!
//! [`anchor`] is an infallible read of a signal nothing contends, so the memo
//! always retains at least one dependency. [`schedule_nudge`] bumps that signal
//! one macrotask later, so the memo re-polls in a borrow-free context instead of
//! waiting for some unrelated user action to happen to write a signal it reads.
//!
//! The tick is deliberately **shared** across call sites. A nudge from one memo
//! re-evaluates every anchored memo, which costs one cheap pass each and makes
//! recovery global; per-site ticks would buy nothing but duplication.
//!
//! `dm_rail_section.rs` keeps its own `RAIL_REBUILD_TICK` and is left alone: it
//! works, it is covered by its own pin tests, and rewiring it would put a
//! working recovery path at risk for tidiness alone.

use dioxus::prelude::*;

/// Shared anchor signal. Read infallibly by [`anchor`]; bumped by
/// [`schedule_nudge`]. Only ever written from inside `crate::util::defer`, so a
/// reader never meets a live write guard.
static REBUILD_TICK: GlobalSignal<u64> = Global::new(|| 0);

thread_local! {
    /// One queued bump at a time. Without this, every degraded memo in a render
    /// would queue its own bump and each bump would re-evaluate every anchored
    /// memo.
    static NUDGE_PENDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Register an infallible dependency for the calling memo.
///
/// Call this **before the first fallible read** in any memo that `try_read()`s a
/// signal. It guarantees the memo cannot come out of a contended pass with zero
/// subscriptions, and it is the channel [`schedule_nudge`] uses to wake it.
///
/// Uses `read()`, not `peek()`: `peek` deliberately does not subscribe, which
/// would make this a no-op.
pub fn anchor() {
    let _ = *REBUILD_TICK.read();
}

/// Schedule a deferred re-evaluation of every anchored memo.
///
/// Call from each degraded (contended-read) branch. The bump runs inside
/// `crate::util::defer` — the clean execution context the signal-safety rules
/// require for a signal mutation — so the retried pass reads in a borrow-free
/// context and schedules no further nudge. If contention somehow persists, that
/// pass queues exactly one more; the loop ends as soon as a pass reads cleanly.
pub fn schedule_nudge() {
    if NUDGE_PENDING.with(|c| c.replace(true)) {
        return; // a bump is already queued
    }
    crate::util::defer(|| {
        NUDGE_PENDING.with(|c| c.set(false));
        REBUILD_TICK.with_mut(|t| *t = t.wrapping_add(1));
    });
}

#[cfg(test)]
mod tests {
    /// Every call site that `try_read()`s a signal inside a `use_memo` must read
    /// the anchor before its first fallible read, and must nudge on every
    /// degraded branch. Source-scraped rather than run, because exercising the
    /// real behaviour needs a Dioxus runtime and a rendered component tree; this
    /// mirrors `dm_rail_section.rs`'s
    /// `rail_builders_read_infallible_guard_before_first_fallible_read`.
    ///
    /// Anchored on the API surface (`signal_guard::anchor`, `try_read(`) rather
    /// than on variable names, so a rename cannot silently disarm it.
    const GUARDED_MEMO_SITES: &[(&str, &str)] = &[
        (
            "conversation.rs message_groups",
            include_str!("../components/conversation.rs"),
        ),
        (
            "members.rs members",
            include_str!("../components/members.rs"),
        ),
        (
            "member_info_modal.rs",
            include_str!("../components/members/member_info_modal.rs"),
        ),
        (
            "ban_button.rs",
            include_str!("../components/members/member_info_modal/ban_button.rs"),
        ),
        (
            "invite_member_modal.rs",
            include_str!("../components/members/invite_member_modal.rs"),
        ),
        (
            "room_list.rs room_items",
            include_str!("../components/room_list.rs"),
        ),
        (
            "edit_room_modal.rs editing_room",
            include_str!("../components/room_list/edit_room_modal.rs"),
        ),
        (
            "dm_thread_modal.rs view",
            include_str!("../components/direct_messages/dm_thread_modal.rs"),
        ),
    ];

    /// Same requirement as [`GUARDED_MEMO_SITES`], for `use_effect(...)`
    /// bodies (freenet/river#559). `use_effect` rebuilds its dependency set
    /// on every pass via the same `reset_and_run_in` primitive `Memo` uses
    /// (`dioxus-hooks-0.7.9/src/use_effect.rs:34`), so an effect that
    /// early-returns on a contended `try_read()` and reads nothing else is
    /// subscribed to nothing and dead for the rest of the session.
    const GUARDED_EFFECT_SITES: &[(&str, &str)] = &[
        ("app.rs", include_str!("../components/app.rs")),
        (
            "members.rs ExportIdentityModal",
            include_str!("../components/members.rs"),
        ),
        (
            "dm_thread_modal.rs DM_DRAFT merge",
            include_str!("../components/direct_messages/dm_thread_modal.rs"),
        ),
    ];

    /// Cut production source at the test module so a needle appearing only in a
    /// test cannot satisfy the pin. Splits on `mod tests`, not
    /// `#[cfg(test)]`, because attributes also decorate non-test items.
    fn production_only(src: &str) -> &str {
        match src.find("\nmod tests") {
            Some(i) => &src[..i],
            None => src,
        }
    }

    /// Drop `//` comments before scanning. Without this the pin matches its own
    /// explanatory prose: a comment that mentions `try_read(` reads as a fallible
    /// read occurring before the anchor, and the pin fails on correct code.
    fn strip_line_comments(src: &str) -> String {
        src.lines()
            .map(|l| match l.find("//") {
                Some(i) => &l[..i],
                None => l,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Body of each `<prefix>(...)` call (e.g. `use_memo(` or `use_effect(`),
    /// delimited by balancing the parenthesis the call opens. Whole-file
    /// scanning is not good enough: these files also `try_read()` from event
    /// handlers, which is legitimate and unrelated, and a naive "first
    /// try_read in the file" check flags it.
    /// Scans BYTES, not chars: these files contain non-ASCII (em dashes and the
    /// like), so char indices and byte offsets diverge, and mixing them silently
    /// slices the wrong region — which made an earlier version of this pin report
    /// "no memo reads fallibly" for a file with four of them. `(` and `)` are
    /// ASCII, so byte scanning is exact here.
    fn hook_bodies<'a>(src: &'a str, prefix: &str) -> Vec<(usize, &'a str)> {
        let bytes = src.as_bytes();
        let mut out = Vec::new();
        let mut search = 0usize;
        while let Some(rel) = src[search..].find(prefix) {
            let open = search + rel + prefix.len() - 1; // byte offset of '('
            let start_line = src[..open].matches('\n').count() + 1;
            let mut depth = 0i32;
            let mut end = None;
            for i in open..bytes.len() {
                match bytes[i] {
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = Some(i);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if let Some(end) = end {
                out.push((start_line, &src[open..=end]));
            }
            search = open + 1;
        }
        out
    }

    fn memo_bodies(src: &str) -> Vec<(usize, &str)> {
        hook_bodies(src, "use_memo(")
    }

    /// Same shape as [`memo_bodies`], for `use_effect(...)` (freenet/river#559:
    /// `use_effect` rebuilds its dependency set on every pass via the same
    /// `reset_and_run_in` primitive as `Memo`, so it has the identical
    /// contended-`try_read` latch risk).
    fn effect_bodies(src: &str) -> Vec<(usize, &str)> {
        hook_bodies(src, "use_effect(")
    }

    /// Every memo that reads a signal fallibly must read the anchor FIRST and
    /// must nudge on contention. Checked per memo body, not per file: each memo
    /// rebuilds its own dependency set, so an anchor in a sibling memo protects
    /// nothing.
    #[test]
    fn every_fallible_memo_anchors_before_its_first_try_read_and_nudges() {
        let mut checked = 0usize;
        for (name, src) in GUARDED_MEMO_SITES {
            let prod = strip_line_comments(production_only(src));
            let bodies = memo_bodies(&prod);
            assert!(
                !bodies.is_empty(),
                "{name}: no use_memo found; the brace matcher or the file changed \
                 shape. Fix the pin rather than deleting it."
            );
            let mut fallible_in_file = 0usize;
            for (line, body) in bodies {
                let Some(first_try) = body.find("try_read(") else {
                    continue;
                };
                fallible_in_file += 1;
                checked += 1;
                let anchor = body.find("signal_guard::anchor").unwrap_or_else(|| {
                    panic!(
                        "{name}: use_memo at line {line} reads a signal with \
                         try_read() but never calls signal_guard::anchor(). A \
                         contended pass can leave it with zero subscriptions and \
                         it will never re-evaluate (freenet/river#555)."
                    )
                });
                assert!(
                    anchor < first_try,
                    "{name}: use_memo at line {line} calls \
                     signal_guard::anchor() AFTER its first try_read( — on the \
                     Err path the anchor is never reached, so it registers \
                     nothing (freenet/river#555)."
                );
                // One nudge per fallible read, not one per memo. The anchor keeps
                // the memo alive, but a contended read still drops THAT signal's
                // subscription, so each fallible read needs its own retry. A
                // per-body `contains` check passed happily while two secondary
                // reads (OUTBOUND_DMS, SYNC_INFO) had no nudge at all -- human
                // review caught those, which is precisely what this pin is for.
                let reads = body.matches("try_read(").count();
                let nudges = body.matches("signal_guard::schedule_nudge").count();
                assert!(
                    nudges >= reads,
                    "{name}: use_memo at line {line} has {reads} fallible \
                     read(s) but only {nudges} schedule_nudge() call(s). Every \
                     read that can fail needs a nudge on its own failure \
                     branch, or that signal's subscription is dropped with \
                     nothing to restore it (freenet/river#555)."
                );
            }
            assert!(
                fallible_in_file > 0,
                "{name}: listed in GUARDED_MEMO_SITES but no use_memo reads \
                 fallibly. Remove the entry rather than leaving a vacuous pin."
            );
        }
        // EXACT count, not a floor. There are 12 fallible memos across the 8
        // files (conversation.rs alone has 4, member_info_modal.rs 2). A floor of
        // 8 left exactly the slack this assertion exists to remove: the matcher
        // could stop finding all four conversation.rs bodies -- the file that
        // caused #555 -- and still pass.
        assert_eq!(
            checked, 12,
            "expected to check exactly the 12 known fallible memos, checked \
             {checked}. If you added or removed a fallible memo, update this \
             number deliberately; if you did not, the matcher has stopped \
             finding memo bodies and this pin has gone vacuous."
        );
    }

    /// Same requirement as [`every_fallible_memo_anchors_before_its_first_try_read_and_nudges`],
    /// for `use_effect(...)` bodies (freenet/river#559). Checked per effect
    /// body, not per file: each effect rebuilds its own dependency set, so an
    /// anchor in a sibling effect (or in a memo elsewhere in the same file)
    /// protects nothing.
    #[test]
    fn every_fallible_effect_anchors_before_its_first_try_read_and_nudges() {
        let mut checked = 0usize;
        for (name, src) in GUARDED_EFFECT_SITES {
            let prod = strip_line_comments(production_only(src));
            let bodies = effect_bodies(&prod);
            assert!(
                !bodies.is_empty(),
                "{name}: no use_effect found; the brace matcher or the file \
                 changed shape. Fix the pin rather than deleting it."
            );
            let mut fallible_in_file = 0usize;
            for (line, body) in bodies {
                let Some(first_try) = body.find("try_read(") else {
                    continue;
                };
                fallible_in_file += 1;
                checked += 1;
                let anchor = body.find("signal_guard::anchor").unwrap_or_else(|| {
                    panic!(
                        "{name}: use_effect at line {line} reads a signal with \
                         try_read() but never calls signal_guard::anchor(). A \
                         contended pass can leave it with zero subscriptions and \
                         it will never re-run for the rest of the session \
                         (freenet/river#559)."
                    )
                });
                assert!(
                    anchor < first_try,
                    "{name}: use_effect at line {line} calls \
                     signal_guard::anchor() AFTER its first try_read( — on the \
                     Err path the anchor is never reached, so it registers \
                     nothing (freenet/river#559)."
                );
                let reads = body.matches("try_read(").count();
                let nudges = body.matches("signal_guard::schedule_nudge").count();
                assert!(
                    nudges >= reads,
                    "{name}: use_effect at line {line} has {reads} fallible \
                     read(s) but only {nudges} schedule_nudge() call(s). Every \
                     read that can fail needs a nudge on its own failure \
                     branch, or that signal's subscription is dropped with \
                     nothing to restore it (freenet/river#559)."
                );
            }
            assert!(
                fallible_in_file > 0,
                "{name}: listed in GUARDED_EFFECT_SITES but no use_effect reads \
                 fallibly. Remove the entry rather than leaving a vacuous pin."
            );
        }
        // EXACT count, not a floor — see the sibling memo test's comment for
        // why: a floor would stay green even if the matcher stopped finding
        // some of app.rs's five fallible effects. Seven fallible use_effect
        // reads across the three files (app.rs has five, members.rs and
        // dm_thread_modal.rs one each) is the full set freenet/river#559
        // identified.
        assert_eq!(
            checked, 7,
            "expected to check exactly the 7 known fallible effects, checked \
             {checked}. If you added or removed a fallible effect, update this \
             number deliberately; if you did not, the matcher has stopped \
             finding effect bodies and this pin has gone vacuous."
        );
    }

    /// The anchor and the nudge must reference the SAME signal, or the guard is
    /// decorative: every anchored memo keeps a subscription that nothing ever
    /// writes, which is exactly the permanent latch of #555. Deleting the
    /// `with_mut` line left the whole suite green before this test existed.
    #[test]
    fn schedule_nudge_writes_the_signal_anchor_reads() {
        let src = production_only(include_str!("signal_guard.rs"));
        let nudge = src
            .split("pub fn schedule_nudge()")
            .nth(1)
            .expect("schedule_nudge() should be defined");
        // Body of the fn: up to the start of the next item.
        let body = &nudge[..nudge.find("\n}").unwrap_or(nudge.len())];
        assert!(
            body.contains("REBUILD_TICK.with_mut"),
            "schedule_nudge() must write REBUILD_TICK -- the very signal \
             anchor() subscribes to. Without that write the anchor keeps a \
             subscription nothing ever fires, so a contended memo stays latched \
             forever (freenet/river#555)."
        );
    }

    #[test]
    fn anchor_uses_read_not_peek() {
        // `peek()` explicitly does not subscribe, so an anchor built on it would
        // compile, look right, and protect nothing.
        let src = production_only(include_str!("signal_guard.rs"));
        let anchor_fn = src
            .split("pub fn anchor()")
            .nth(1)
            .expect("anchor() should be defined");
        let body = &anchor_fn[..anchor_fn.find('}').unwrap_or(anchor_fn.len())];
        assert!(
            body.contains("REBUILD_TICK.read()"),
            "anchor() must read() the tick to register a subscription"
        );
        assert!(
            !body.contains("peek"),
            "anchor() must not use peek(): peek does not subscribe, which would \
             make the guard silently useless"
        );
    }
}
