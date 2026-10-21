//! Which display items the room history renders: the tail window, its growth
//! and trim, and its relocation by item identity. Pure arithmetic.

/// How many display items (message groups and event summaries) the conversation
/// renders when a room is opened.
///
/// Rendering an entire room at once is what makes a busy room unusable.
/// Profiling the live "Off Topic" room on 2026-07-26 (1133 messages, 136
/// members) measured 24,305 DOM nodes and a 1.65 GB WASM heap — and WASM never
/// returns linear memory to the OS, so that peak is permanent for the tab.
/// Rendering the tail the reader actually lands on, and backfilling only when
/// they scroll off the top of it, keeps both bounded by what has been looked at.
///
/// Counted in ITEMS, not messages: consecutive messages from one author share a
/// group, so this is a floor on messages shown, never a cap. A room with fewer
/// items than this renders exactly as it always did — no sentinel, no
/// backfill — so the common case carries none of the windowing's behaviour.
pub(super) const INITIAL_WINDOW_ITEMS: usize = 60;

/// How many more items each backfill reveals when the reader reaches the top of
/// the current window. Matching [`INITIAL_WINDOW_ITEMS`] means a reader paging
/// back through history grows the window at the rate they consume it.
pub(super) const WINDOW_GROWTH_ITEMS: usize = 60;

/// Hard ceiling on how many display items arrival-growth can accumulate.
///
/// Arrivals GROW the rendered window instead of sliding it — see
/// [`HistoryWindow::resolve_held`] — so a long session in a busy room would
/// otherwise re-accumulate exactly the unbounded render the window exists to
/// prevent (freenet/river#498). Four initial windows is the chosen bound:
/// deep enough that trims are rare (240 items is hours of a busy room, and the
/// bottom-settle trim usually fires long before), shallow enough that the
/// worst-case DOM stays ~4x the room-open cost rather than unbounded.
///
/// At the ceiling the range stops at its end and holds newer items back
/// (`HistoryWindow::has_newer`) below a parked reader. A reader at the end
/// follows the arrivals instead: the follow's request takes the latest range
/// (`resolve_rendered_range`).
///
/// The ceiling caps ARRIVAL growth only. A reader paging back through history
/// raises `window_items` explicitly, and that requested size always wins over
/// the ceiling — capping it would make the backfill sentinel a no-op past 240
/// items and dead-end the history (see the cap in
/// [`HistoryWindow::resolve_held`]).
pub(super) const WINDOW_ITEMS_CEILING: usize = INITIAL_WINDOW_ITEMS * 4;

/// How far into the rendered history the backfill trigger REACHES, in px.
///
/// The sentinel is a strip spanning `[0, BACKFILL_LEAD_PX]` from the top of the
/// history, not a marker at a point. Both halves of that matter:
///
/// * Not a 1px marker at the very top: it would only intersect at
///   `scrollTop == 0`, so the reader hits the end of the rendered history and
///   *then* watches it grow. Reaching a screenful in means the next page is
///   already there by the time they get to it, as `bottom-newer-sentinel`
///   does at the other end of a held range.
/// * Not a 1px marker at `top: BACKFILL_LEAD_PX` either. That was the first
///   attempt and it is worse than doing nothing: on any viewport SHORTER than
///   the lead, scrolling to the very top puts the marker BELOW the viewport, so
///   the reader who scrolls straight to the top — the exact case this exists
///   for — never triggers a backfill and the history dead-ends. A strip is
///   intersecting for every scroll position in the band, including 0, on every
///   viewport size.
pub(super) const BACKFILL_LEAD_PX: i32 = 800;

/// Where the history's tail window starts, and whether anything is behind it.
///
/// Split out from the render so the arithmetic is testable: getting it wrong
/// either renders an empty history (start past the end) or silently renders
/// everything (the blow-up the window exists to prevent), and neither is
/// visible in a unit test of the component.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct HistoryWindow {
    /// Index of the first display item to render.
    pub(super) start: usize,
    /// One past the last display item to render.
    pub(super) end: usize,
    /// Whether older items are held back — drives the backfill sentinel.
    pub(super) has_older: bool,
    /// Whether newer items are held back below the reader. Drives the
    /// newer-history sentinel and the catch-up button.
    pub(super) has_newer: bool,
}

/// Inputs for the range's lower edge. `Default` ends at the newest item.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(super) struct RangeHold {
    /// The relocated end of last render's range, if it stopped short of the
    /// newest item.
    pub(super) end: Option<usize>,
    /// The start never moves past this index, even above the ceiling: the
    /// reading item, less one for a group the view top cuts through.
    pub(super) keep: Option<usize>,
}

/// The identity of the window's head: which display item the last render
/// started at, remembered by KEY, with the index it held as a relocation hint.
///
/// The anchor is by IDENTITY, not position, because positions are not stable:
/// the room's cleanup (`post_apply_cleanup`) drains the oldest message once a
/// room exceeds `max_recent_messages` (the steady state of every busy room), so an arrival
/// shifts every index down by one. A positional anchor then renders a head one
/// item LATER in identity — the top rendered row is removed in the same patch
/// that appends the arrival, which is the #501 slide reproduced in content
/// space, and with scroll anchoring disabled it crawls a parked reader upward
/// one row per arrival. Anchoring on the head item's key holds the rendered
/// set fixed in identity no matter how the indices shift underneath it.
#[derive(Clone, PartialEq, Debug)]
pub(super) struct WindowAnchor {
    /// `display_item_key` of the first [`WINDOW_ANCHOR_KEYS`] rendered items,
    /// head first. The spares exist because the head key alone can vanish
    /// while its NEIGHBORS survive: an at-cap drain that consumes the head
    /// (or only its first message — a multi-message head group RE-KEYS, since
    /// a group's key is its first message's id), or a batched delta draining
    /// many items at once. The first spare still present tells us exactly
    /// where the surviving remainder of the old window now sits.
    pub(super) keys: Vec<String>,
    /// The index the head held last render — a hint so relocation is
    /// O(shift), not O(total), in the common case.
    pub(super) index: usize,
}

/// How many leading item keys the anchor remembers (head + spares).
///
/// Bounds how large a head-consuming removal can be precisely relocated: a
/// front drain that consumes the head and ALL spares falls back to index 0,
/// which for a front-contiguous drain — the only mechanism that can remove
/// this many contiguous leading items — is exactly the nearest surviving
/// item anyway.
pub(super) const WINDOW_ANCHOR_KEYS: usize = 8;

/// Where the anchored window starts after the item list changed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct RelocatedWindow {
    /// Start index for the resolve: the head's new position when it survived,
    /// otherwise derived from the first surviving spare (its position minus
    /// its offset in the anchor), otherwise 0.
    pub(super) start: usize,
    /// Whether the HEAD itself survived under its own key. False means the
    /// pre-patch head row is gone or re-keyed, so rendered content above (or
    /// at) the new head changed and the reader needs the measured
    /// reposition.
    pub(super) head_survived: bool,
}

/// Search for one anchored key after the item list changed.
///
/// Front prunes shift every surviving index DOWN, so the hint is checked
/// first, then the indices below it, then (for completeness — a merge can
/// insert older history above the head) the indices above. `None` means this
/// key is gone entirely.
pub(super) fn relocate_anchor(
    total: usize,
    hint: usize,
    is_anchor: impl Fn(usize) -> bool,
) -> Option<usize> {
    if hint < total && is_anchor(hint) {
        return Some(hint);
    }
    if let Some(i) = (0..hint.min(total)).rev().find(|&i| is_anchor(i)) {
        return Some(i);
    }
    (hint.saturating_add(1)..total).find(|&i| is_anchor(i))
}

/// Re-locate the anchored window: the head by its own key, else by the first
/// surviving spare, else index 0.
///
/// The index-0 fallback is exact for a front-contiguous drain that consumed
/// the head and every spare — the oldest remaining item IS then the nearest
/// survivor. It is a DEGRADED answer for the other way to lose that many
/// contiguous leading items (a ban purge or bulk delete of >=
/// [`WINDOW_ANCHOR_KEYS`] display items above the reader): the window jumps
/// to the front of the room, bounded only by the ceiling. Rare, and it fails
/// toward rendering MORE history rather than losing the reader's place.
///
/// The head reposition (`correct_view_after_patch`) compensates the reader for
/// the top-contiguous removals this relocation deals in.
pub(super) fn relocate_window(
    total: usize,
    anchor: &WindowAnchor,
    key_at: impl Fn(usize, &str) -> bool,
) -> RelocatedWindow {
    for (spare, key) in anchor.keys.iter().enumerate() {
        let located = relocate_anchor(total, anchor.index + spare, |i| key_at(i, key));
        if let Some(i) = located {
            return RelocatedWindow {
                start: i.saturating_sub(spare),
                head_survived: spare == 0,
            };
        }
    }
    RelocatedWindow {
        start: 0,
        head_survived: false,
    }
}

/// Re-locate a held range's end: one past its last item, or past the first
/// surviving spare before it (keys newest first). A found spare is the last
/// surviving item, so unlike the head it needs no offset. `None` when every
/// key is gone.
pub(super) fn relocate_tail(
    total: usize,
    anchor: &WindowAnchor,
    key_at: impl Fn(usize, &str) -> bool,
) -> Option<usize> {
    anchor.keys.iter().enumerate().find_map(|(spare, key)| {
        relocate_anchor(total, anchor.index.saturating_sub(spare), |i| {
            key_at(i, key)
        })
        .map(|i| i + 1)
    })
}

/// Margin over the backfill strip's reach before a trim is allowed. Covers
/// what the measurement leaves out: padding and the date separator the first
/// retained item gains.
#[cfg(any(target_arch = "wasm32", test))]
const TRIM_HEADROOM_PX: f64 = 200.0;

/// Would a trim leaving `retained_tail_px` of history put the backfill
/// sentinel strip inside the bottom viewport?
///
/// At the bottom, the sentinel strip `[0, BACKFILL_LEAD_PX]` intersects the
/// viewport iff `content_height < client_height + BACKFILL_LEAD_PX`. A tail
/// short enough for that (browser zoom-out, a tall portrait monitor over a
/// modest window) would re-fire the backfill the moment the trim lands:
/// trim → backfill → restore → settle at the bottom → trim, a silent render
/// loop at full speed (#505). The settle handler then skips the
/// trim and the window stays grown, bounded by the ceiling.
///
/// The tail is measured. An average-row-height estimate looped on rooms whose
/// older rows are much taller than the newest ones.
///
/// Only the wasm settle handler calls this; natively only the unit tests do.
#[cfg(any(target_arch = "wasm32", test))]
pub(super) fn trim_would_rearm_backfill(retained_tail_px: i32, client_height: i32) -> bool {
    (retained_tail_px as f64) < client_height as f64 + BACKFILL_LEAD_PX as f64 + TRIM_HEADROOM_PX
}

/// The requested window size after one backfill growth step.
///
/// Grows from the RENDERED size, not the requested size. Arrivals grow an
/// anchored window past what was requested (`rendered = total - start` exceeds
/// `requested` by the number of arrivals), and a growth step computed from the
/// stale requested size can resolve to a start the anchor already renders —
/// zero new rows, no DOM change, so the sentinel's IntersectionObserver never
/// re-fires and paging dead-ends (#505). Growing from the
/// rendered size guarantees every step reveals `WINDOW_GROWTH_ITEMS` more
/// items than are currently on screen.
pub(super) fn grown_window(requested: usize, rendered: usize) -> usize {
    rendered.max(requested) + WINDOW_GROWTH_ITEMS
}

impl HistoryWindow {
    /// Test shorthand for a range with no held end; see [`Self::resolve_held`].
    #[cfg(test)]
    fn resolve(total_items: usize, window: usize, anchor: Option<usize>) -> Self {
        Self::resolve_held(total_items, window, anchor, RangeHold::default())
    }

    /// Resolve which display items the history renders.
    ///
    /// `anchor` is the start index the PREVIOUS render used (`None` when the
    /// room was just opened, or after a trim). It is what keeps the window
    /// from SLIDING on arrival: without it, every new message advanced `start`
    /// by one, dropping the oldest rendered row in the same patch that
    /// appended the new one at the bottom (freenet/river#501). With scroll
    /// anchoring disabled on the container, removing content above the
    /// viewport visibly shifts the reader. So:
    ///
    /// * `start` NEVER moves forward from the anchor on an ordinary render:
    ///   arrivals grow the rendered count instead (start fixed, new items
    ///   appended below).
    /// * `start` moves BACK (upward, revealing older items) when the reader
    ///   backfills — `window` grew, so the plain tail start is earlier than
    ///   the anchor.
    /// * [`WINDOW_ITEMS_CEILING`] caps arrival growth. Past it the end stops
    ///   instead, wherever the reader is, and newer items are held back.
    ///   `max(window, ...)` keeps the ceiling from ever capping a
    ///   reader-requested backfill.
    ///
    /// `hold` adds the lower edge:
    ///
    /// * A held end moves only when the caller extends it (newer paging); the
    ///   ceiling then slides the start.
    /// * The start never passes `keep`, not even at the ceiling.
    ///
    /// `window` counts back from the end, so backfill works the same either way.
    ///
    /// Trimming back toward [`INITIAL_WINDOW_ITEMS`] is NOT done here — it is
    /// an explicit event (a settle landing at the bottom, or a room switch)
    /// that clears the anchor and resets `window`, because a trim is only
    /// invisible when the view is at the bottom, where the browser's scrollTop
    /// clamp keeps the tail glued in place.
    pub(super) fn resolve_held(
        total_items: usize,
        window: usize,
        anchor: Option<usize>,
        hold: RangeHold,
    ) -> Self {
        // A held end can never pass the newest item, and never empties the
        // range: if every item it held is gone, fall back to the newest.
        let mut end = match hold.end {
            Some(e) if e > 0 => e.min(total_items),
            _ => total_items,
        };
        // `max(1)` so an item is always on screen: a zero window would
        // render an empty history that the reader has no way to scroll into.
        let base = end.saturating_sub(window.max(1));
        // Grow, never slide: keep the anchored start unless a backfill asked
        // for an even earlier one. `min` also repairs an anchor past the end
        // (messages pruned out from under it).
        let mut start = match anchor {
            None => base,
            Some(prev) => prev.min(base),
        };
        // The ceiling on arrival growth; never on reader-requested backfill.
        // One growth step of grace above the requested size, so a range
        // backfilled to the ceiling still renders the next arrivals before its
        // end holds (#505).
        let cap = (window + WINDOW_GROWTH_ITEMS)
            .max(WINDOW_ITEMS_CEILING)
            .max(1);
        if end - start > cap {
            if hold.end.is_none() {
                end = start + cap;
            } else {
                start = end - cap;
            }
        }
        if let Some(keep) = hold.keep {
            start = start.min(keep.min(end.saturating_sub(1)));
        }
        Self {
            start,
            end,
            has_older: start > 0,
            has_newer: end < total_items,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The newest item is on screen for ANY window size — including the
    /// degenerate zero, which would otherwise render a history the reader
    /// cannot scroll into (the window only ever grows upward from the bottom).
    #[test]
    fn the_window_always_renders_the_newest_item() {
        for total in 1..200usize {
            for window in [0, 1, 2, 59, 60, 61, 199, 200, 5000] {
                for anchor in [None, Some(0), Some(30), Some(total)] {
                    let w = HistoryWindow::resolve(total, window, anchor);
                    assert!(
                        w.start < total,
                        "total {total}, window {window}, anchor {anchor:?}: \
                         start {} skips every item",
                        w.start
                    );
                }
            }
        }
    }

    /// A room that fits in the window renders exactly as it did before the
    /// window existed: every item, and no backfill sentinel to observe.
    #[test]
    fn a_room_inside_the_window_is_rendered_whole() {
        for total in 0..=INITIAL_WINDOW_ITEMS {
            let w = HistoryWindow::resolve(total, INITIAL_WINDOW_ITEMS, None);
            assert_eq!(
                w.start, 0,
                "total {total} should render from the first item"
            );
            assert!(!w.has_older, "total {total} has nothing to backfill");
        }
    }

    /// Past the window, only the tail renders and the sentinel appears. The
    /// counts here are the live "Off Topic" room's shape.
    #[test]
    fn a_room_past_the_window_renders_only_its_tail() {
        let w = HistoryWindow::resolve(1133, INITIAL_WINDOW_ITEMS, None);
        assert_eq!(w.start, 1133 - INITIAL_WINDOW_ITEMS);
        assert!(w.has_older, "1133 items must offer a backfill");

        // Each backfill reveals exactly one growth step more... (the anchor is
        // carried the way the render carries it, and moving it BACK for a
        // backfill is allowed — only forward movement is the #501 slide)
        let grown = HistoryWindow::resolve(
            1133,
            INITIAL_WINDOW_ITEMS + WINDOW_GROWTH_ITEMS,
            Some(w.start),
        );
        assert_eq!(w.start - grown.start, WINDOW_GROWTH_ITEMS);

        // ...until the window covers the room, at which point the sentinel goes.
        let all = HistoryWindow::resolve(1133, 1133, Some(grown.start));
        assert_eq!(all.start, 0);
        assert!(!all.has_older);
    }

    /// Arrivals GROW an anchored window instead of sliding it (#501): the start
    /// index stays put as the total climbs, so nothing is removed above the
    /// viewport in the same patch that appends below it.
    #[test]
    fn arrivals_grow_an_anchored_window_instead_of_sliding_it() {
        let opened = HistoryWindow::resolve(100, INITIAL_WINDOW_ITEMS, None);
        assert_eq!(opened.start, 40);

        let mut anchor = Some(opened.start);
        for arrivals in 1..=50usize {
            let w = HistoryWindow::resolve(100 + arrivals, INITIAL_WINDOW_ITEMS, anchor);
            assert_eq!(
                w.start, opened.start,
                "arrival {arrivals} slid the window instead of growing it"
            );
            anchor = Some(w.start);
        }
    }

    /// The trim — the settle handler clearing the anchor and resetting the
    /// requested window once the reader's own settle lands at the bottom —
    /// resolves back to the plain tail.
    #[test]
    fn a_trim_resolves_back_to_the_plain_tail() {
        // 40 arrivals grew the anchored window to 100 rendered items...
        let grown = HistoryWindow::resolve(140, INITIAL_WINDOW_ITEMS, Some(40));
        assert_eq!(grown.start, 40);
        // ...and the trim (anchor cleared, window back to INITIAL) collapses
        // it to the newest INITIAL_WINDOW_ITEMS.
        let trimmed = HistoryWindow::resolve(140, INITIAL_WINDOW_ITEMS, None);
        assert_eq!(trimmed.start, 140 - INITIAL_WINDOW_ITEMS);
        assert!(trimmed.has_older);
    }

    /// The ceiling bounds what arrival growth can accumulate — a reader idle
    /// through a very long burst must not re-accumulate the unbounded render
    /// the window exists to prevent — but it NEVER caps a reader-requested
    /// backfill, which would dead-end paging through history.
    #[test]
    fn the_ceiling_caps_arrival_growth_but_never_reader_backfill() {
        // Anchored at 100 while the room ran away to 1000 items: the end stops
        // at the ceiling and the rest is held back.
        let w = HistoryWindow::resolve(1000, INITIAL_WINDOW_ITEMS, Some(100));
        assert_eq!((w.start, w.end), (100, 100 + WINDOW_ITEMS_CEILING));
        assert!(w.has_older && w.has_newer);

        // A reader who backfilled PAST the ceiling keeps everything they asked
        // for: the requested window wins over the ceiling.
        let requested = WINDOW_ITEMS_CEILING + WINDOW_GROWTH_ITEMS;
        let w = HistoryWindow::resolve(1000, requested, Some(1000 - requested));
        assert_eq!(w.start, 1000 - requested);

        // Arrival-grace headroom: a range backfilled TO the ceiling still
        // renders the next few arrivals — the cap sits one growth step above
        // the request (#505).
        let at_ceiling = WINDOW_ITEMS_CEILING;
        let anchor = 1000 - at_ceiling;
        let w = HistoryWindow::resolve(1010, at_ceiling, Some(anchor));
        assert_eq!(
            (w.start, w.end),
            (anchor, 1010),
            "10 arrivals within the grace must render without holding the end"
        );
    }

    /// Anchor carrying the head + spare keys the way the render does.
    fn anchor_of(keys: &[String], start: usize) -> WindowAnchor {
        WindowAnchor {
            keys: keys[start..]
                .iter()
                .take(WINDOW_ANCHOR_KEYS)
                .cloned()
                .collect(),
            index: start,
        }
    }

    /// #505: the anchor is re-located by IDENTITY. An at-cap room
    /// prunes its oldest message per arrival, shifting every index down — the
    /// walk below is that steady state, and the head must stay the same ITEM
    /// (its index marching toward 0), not the same index.
    #[test]
    fn the_anchor_follows_the_item_through_at_cap_pruning() {
        // A 100-item room at cap: items are identified by these keys.
        let mut keys: Vec<String> = (0..100).map(|i| format!("m{i}")).collect();
        let opened = HistoryWindow::resolve(keys.len(), INITIAL_WINDOW_ITEMS, None);
        assert_eq!(opened.start, 40);
        let head_key = keys[opened.start].clone();
        let mut anchor = anchor_of(&keys, opened.start);

        for arrival in 0..40usize {
            // apply_delta at cap: drain the oldest, append the new.
            keys.remove(0);
            keys.push(format!("new{arrival}"));
            let relocated = relocate_window(keys.len(), &anchor, |i, key| keys[i] == key);
            assert!(relocated.head_survived, "arrival {arrival}: head survives");
            let start =
                HistoryWindow::resolve(keys.len(), INITIAL_WINDOW_ITEMS, Some(relocated.start))
                    .start;
            assert_eq!(
                keys[start], head_key,
                "arrival {arrival}: the window head changed identity — the \
                 #501 slide reproduced in content space"
            );
            assert_eq!(start, opened.start - (arrival + 1), "start marches down");
            anchor = anchor_of(&keys, start);
        }

        // One more prune consumes the head itself: the head key is gone, but
        // the FIRST SPARE (the item right after the old head) survives at
        // index 0, so the relocation lands on the nearest surviving item and
        // reports the head as removed — which is what arms the measured
        // reposition for a parked reader.
        keys.remove(0);
        keys.push("new40".into());
        let relocated = relocate_window(keys.len(), &anchor, |i, key| keys[i] == key);
        assert!(!relocated.head_survived, "the head itself was pruned");
        assert_eq!(
            relocated.start, 0,
            "the first surviving spare pins the window to the nearest \
             surviving item"
        );
    }

    /// #505: a MULTI-message head group RE-KEYS when an
    /// at-cap drain consumes its first message (a group's key is its first
    /// message's id). The old key is gone while the group itself survives —
    /// the spares must still pin the window to the survivors.
    #[test]
    fn a_rekeyed_multi_message_head_group_is_still_anchored() {
        // Window head at index 0: a group keyed by its first message "m0",
        // followed by neighbors. The at-cap drain consumes "m0"; the group
        // survives RE-KEYED as "m1". (Modelled at the key level — exactly
        // what `display_item_key` exposes to this machinery.)
        let anchor = WindowAnchor {
            keys: vec!["m0".into(), "b".into(), "c".into(), "d".into()],
            index: 0,
        };
        let post_keys = ["m1", "b", "c", "d", "e"];
        let relocated = relocate_window(post_keys.len(), &anchor, |i, key| post_keys[i] == key);
        assert!(
            !relocated.head_survived,
            "the re-keyed head must count as removed — its row is replaced \
             and its height changed"
        );
        assert_eq!(
            relocated.start, 0,
            "the first spare (\"b\", found at index 1, offset 1 in the \
             anchor) pins the window back to the re-keyed group"
        );
    }

    /// A batched drain that consumes the head AND several spares (the
    /// `appendMessages` burst crossing the anchor) still lands on the first
    /// surviving spare, offset back to where the head would have been.
    #[test]
    fn a_batched_drain_past_the_head_lands_on_the_first_surviving_spare() {
        // 100 items, window head at 25 with spares 25..33.
        let keys: Vec<String> = (0..100).map(|i| format!("m{i}")).collect();
        let anchor = anchor_of(&keys, 25);
        // A 30-item drain + 30 arrivals: items 0..30 gone, head (25) and
        // spares 25..29 with it; spare "m30" (offset 5) survives at index 0.
        let post_keys: Vec<String> = (30..100)
            .map(|i| format!("m{i}"))
            .chain((0..30).map(|i| format!("new{i}")))
            .collect();
        let relocated = relocate_window(post_keys.len(), &anchor, |i, key| post_keys[i] == key);
        assert!(!relocated.head_survived);
        assert_eq!(
            relocated.start, 0,
            "spare m30 found at index 0, offset 5 in the anchor → start 0: \
             the whole surviving remainder of the old window stays rendered"
        );

        // And when the drain consumes every spare too, the only safe answer
        // for a front-contiguous drain is index 0 — the oldest survivor.
        let post_keys: Vec<String> = (40..100)
            .map(|i| format!("m{i}"))
            .chain((0..40).map(|i| format!("new{i}")))
            .collect();
        let relocated = relocate_window(post_keys.len(), &anchor, |i, key| post_keys[i] == key);
        assert!(!relocated.head_survived);
        assert_eq!(relocated.start, 0);
    }

    /// The case where the spares are load-bearing and no fallback can stand
    /// in for them: the head alone vanishes MID-window (its whole group
    /// deleted), the neighbors survive in place. The first spare pins the
    /// window one slot back from where it sits — a head-only relocation
    /// would fall back to index 0 and blow the window open across the whole
    /// room (ceiling-bounded, but a ~180-item over-render and a torn view).
    #[test]
    fn a_mid_window_head_deletion_reanchors_on_the_next_survivor() {
        let keys: Vec<String> = (0..100).map(|i| format!("m{i}")).collect();
        let anchor = anchor_of(&keys, 40);
        // The head item "m40" is deleted outright; everything else survives,
        // shifted down by one from index 41 on.
        let post_keys: Vec<String> = keys
            .iter()
            .filter(|k| k.as_str() != "m40")
            .cloned()
            .collect();
        let relocated = relocate_window(post_keys.len(), &anchor, |i, key| post_keys[i] == key);
        assert!(!relocated.head_survived);
        assert_eq!(
            relocated.start, 39,
            "spare m41 (offset 1) found at index 40 → start 39: the window \
             re-anchors one slot back, not at the front of the room"
        );
    }

    /// Skip the trim when the measured tail would leave the backfill strip in
    /// view; trim and backfill would otherwise loop (#505).
    #[test]
    fn the_trim_skips_when_the_tail_would_rearm_the_sentinel() {
        let reach = |client: i32| client + BACKFILL_LEAD_PX + TRIM_HEADROOM_PX as i32;
        // Exactly at the reach clears it; one px short does not.
        assert!(!trim_would_rearm_backfill(reach(900), 900));
        assert!(trim_would_rearm_backfill(reach(900) - 1, 900));
    }

    /// Item keys `m0..m{total}` for the range tests.
    fn item_keys(total: usize) -> Vec<String> {
        (0..total).map(|i| format!("m{i}")).collect()
    }

    /// The tail anchor the render stores for a held range.
    fn tail_of(keys: &[String], end: usize) -> WindowAnchor {
        WindowAnchor {
            keys: keys[..end]
                .iter()
                .rev()
                .take(WINDOW_ANCHOR_KEYS)
                .cloned()
                .collect(),
            index: end - 1,
        }
    }

    /// Past the ceiling, the range holds its end instead of sliding its start,
    /// wherever the reader is, and later arrivals don't grow it (#732).
    #[test]
    fn the_range_holds_its_end_at_the_ceiling() {
        let opened = HistoryWindow::resolve(201, INITIAL_WINDOW_ITEMS, None);
        // 200 arrivals in one patch, start anchored.
        let held = HistoryWindow::resolve(401, INITIAL_WINDOW_ITEMS, Some(opened.start));
        assert_eq!(
            held.start, opened.start,
            "the start must not slide past the reader"
        );
        assert_eq!(held.end - held.start, WINDOW_ITEMS_CEILING);
        assert!(held.has_newer, "the newest items are withheld");

        // More arrivals: the held end stays on its item, so the DOM stops growing.
        let again = HistoryWindow::resolve_held(
            501,
            INITIAL_WINDOW_ITEMS,
            Some(held.start),
            RangeHold {
                end: Some(held.end),
                ..RangeHold::default()
            },
        );
        assert_eq!((again.start, again.end), (held.start, held.end));

        // Below the ceiling nothing is held.
        let small = HistoryWindow::resolve(230, INITIAL_WINDOW_ITEMS, Some(opened.start));
        assert_eq!((small.start, small.end), (opened.start, 230));
        assert!(!small.has_newer);
    }

    /// An at-cap drain shifts every index; a held range keeps both edges on
    /// their items.
    #[test]
    fn a_held_range_follows_its_items_through_pruning() {
        let mut keys = item_keys(400);
        let (start, end) = (100, 100 + WINDOW_ITEMS_CEILING);
        let head = anchor_of(&keys, start);
        let tail = tail_of(&keys, end);
        let (first, last) = (keys[start].clone(), keys[end - 1].clone());
        // A batch of 40 arrivals drains the 40 oldest items.
        keys.drain(..40);
        keys.extend((0..40).map(|i| format!("new{i}")));
        let relocated = relocate_window(keys.len(), &head, |i, k| keys[i] == k);
        let held_end = relocate_tail(keys.len(), &tail, |i, k| keys[i] == k);
        assert_eq!(held_end, Some(end - 40));
        let w = HistoryWindow::resolve_held(
            keys.len(),
            INITIAL_WINDOW_ITEMS,
            Some(relocated.start),
            RangeHold {
                end: held_end,
                keep: None,
            },
        );
        assert_eq!(keys[w.start], first);
        assert_eq!(keys[w.end - 1], last);
        assert!(w.has_newer);

        // The last held item deleted outright: its spare places the end.
        let gone = keys[w.end - 1].clone();
        keys.retain(|k| *k != gone);
        assert_eq!(
            relocate_tail(
                keys.len(),
                &tail_of_keys(&[gone, keys[w.end - 2].clone()]),
                |i, k| keys[i] == k
            ),
            Some(w.end - 1)
        );
        assert_eq!(
            relocate_tail(keys.len(), &tail_of_keys(&["nope".into()]), |i, k| keys[i]
                == k),
            None
        );
    }

    /// A tail anchor from explicit keys (newest first), hint past the end.
    fn tail_of_keys(keys: &[String]) -> WindowAnchor {
        WindowAnchor {
            keys: keys.to_vec(),
            index: 10_000,
        }
    }

    /// A held range pages both ways. A newer page slides the start behind the
    /// reader, but never past `keep`.
    #[test]
    fn a_held_range_pages_older_and_newer() {
        let total = 1000;
        let (start, end) = (300, 300 + WINDOW_ITEMS_CEILING);
        let held = |end, keep| RangeHold {
            end: Some(end),
            keep,
        };
        // Older: one growth step from the rendered size reveals exactly one page.
        let requested = grown_window(INITIAL_WINDOW_ITEMS, end - start);
        let older = HistoryWindow::resolve_held(total, requested, Some(start), held(end, None));
        assert_eq!((older.start, older.end), (start - WINDOW_GROWTH_ITEMS, end));

        // Newer: the end moves one page down and the ceiling slides the start.
        let newer = HistoryWindow::resolve_held(
            total,
            INITIAL_WINDOW_ITEMS,
            Some(start),
            held(end + WINDOW_GROWTH_ITEMS, None),
        );
        assert_eq!(newer.end, end + WINDOW_GROWTH_ITEMS);
        assert_eq!(newer.end - newer.start, WINDOW_ITEMS_CEILING);

        // The reader's row bounds that slide: the cap gives way, not the row.
        let keep = start + 20;
        let kept = HistoryWindow::resolve_held(
            total,
            INITIAL_WINDOW_ITEMS,
            Some(start),
            held(end + WINDOW_GROWTH_ITEMS, Some(keep)),
        );
        assert_eq!(kept.start, keep);
        assert!(kept.end - kept.start > WINDOW_ITEMS_CEILING);

        // Paging newer until the end reaches the newest item ends the hold.
        let last = HistoryWindow::resolve_held(
            total,
            INITIAL_WINDOW_ITEMS,
            Some(900),
            held(total + 30, None),
        );
        assert_eq!(last.end, total);
        assert!(!last.has_newer);

        // A held end with nothing left falls back to the newest, not to empty.
        let empty =
            HistoryWindow::resolve_held(total, INITIAL_WINDOW_ITEMS, Some(900), held(0, None));
        assert_eq!(empty.end, total);
    }

    /// `relocate_anchor` search order and fallbacks.
    #[test]
    fn relocate_anchor_finds_shifted_heads_and_reports_lost_ones() {
        let keys = ["a", "b", "c", "d", "e"];
        let find =
            |key: &'static str, hint: usize| relocate_anchor(keys.len(), hint, |i| keys[i] == key);
        // Unshifted: found at the hint.
        assert_eq!(find("c", 2), Some(2));
        // Front prune shifted it down: found below the hint.
        assert_eq!(find("c", 3), Some(2));
        // Older history merged in above: found above the hint.
        assert_eq!(find("c", 0), Some(2));
        // Hint out of range entirely: still found.
        assert_eq!(find("e", 400), Some(4));
        // Gone: None, and `relocate_window` moves on to the next spare.
        assert_eq!(find("zz", 2), None);
        assert_eq!(relocate_anchor(0, 0, |_| true), None, "empty list");
    }

    /// #505: one backfill growth step must always reveal more than
    /// what is RENDERED — growing from the stale requested size can resolve to
    /// a start the anchor already renders (zero new rows, sentinel never
    /// re-fires, paging dead-ends). The trace: open at 1000
    /// items, 61 arrivals, page up.
    #[test]
    fn a_growth_step_reveals_rows_even_after_arrivals_grew_the_window() {
        let opened = HistoryWindow::resolve(1000, INITIAL_WINDOW_ITEMS, None);
        assert_eq!(opened.start, 940);
        // 61 arrivals: the anchored start holds, rendered = 121.
        let grown = HistoryWindow::resolve(1061, INITIAL_WINDOW_ITEMS, Some(opened.start));
        assert_eq!(grown.start, opened.start);
        let rendered = 1061 - grown.start;
        assert_eq!(rendered, 121);

        // The regression this pins: growing from the REQUESTED size reveals
        // nothing (min(anchor, 1061-120) = anchor).
        let dead = HistoryWindow::resolve(
            1061,
            INITIAL_WINDOW_ITEMS + WINDOW_GROWTH_ITEMS,
            Some(grown.start),
        );
        assert_eq!(
            dead.start, grown.start,
            "premise: the naive growth really does dead-end — if this stops \
             holding, the whole scenario needs rebuilding"
        );

        // The fix: grow from the rendered size.
        let requested = grown_window(INITIAL_WINDOW_ITEMS, rendered);
        assert_eq!(requested, rendered + WINDOW_GROWTH_ITEMS);
        let paged = HistoryWindow::resolve(1061, requested, Some(grown.start));
        assert!(
            paged.start < grown.start,
            "a growth step must strictly decrease start"
        );
        assert_eq!(
            grown.start - paged.start,
            WINDOW_GROWTH_ITEMS,
            "and by exactly one growth step"
        );
    }
}
