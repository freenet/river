// The functions here are wired into the conversation by a later change (Task 4 of
// the history-scroll-anchor plan).
#![allow(dead_code)]

use super::BOTTOM_THRESHOLD_PX;

/// How many rows above the newest visible one are remembered as fallbacks, for
/// when the anchor row itself is deleted or windowed out before the restore.
pub(super) const ANCHOR_FALLBACK_ROWS: usize = 4;

/// The most a `scroll` event may move `scrollTop` and still be read as the
/// browser's clamp after a layout change rather than the reader.
///
/// Equal to `BOTTOM_THRESHOLD_PX`: a clamp that stays inside it can't take a
/// pinned reader out of the pin, so misreading one is harmless either way.
pub(super) const LAYOUT_SHIFT_ALLOWANCE_PX: i32 = 100;

/// The three numbers that change when the history is laid out again.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub(super) struct LayoutSig {
    pub scroll_height: i32,
    pub client_height: i32,
    pub client_width: i32,
}

/// Who a `scroll` event belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ScrollCause {
    /// The reader moved the view: it becomes the new anchor.
    Reader,
    /// A layout change moved it (a browser clamp): put the anchor back.
    Layout,
}

/// Indices of up to `n` rows, newest first, starting at the newest row that
/// intersects the viewport `[view_top, view_bottom]`. The rows above it are
/// fallbacks and need not be visible. Empty if no row is visible.
///
/// `rows` are `(top, bottom)` in document order, so their tops are monotonic
/// and the newest row starting above the viewport's bottom edge is a binary
/// search away. Touching an edge is not intersecting it.
pub(super) fn newest_visible_rows(
    rows: &[(i32, i32)],
    view_top: i32,
    view_bottom: i32,
    n: usize,
) -> Vec<usize> {
    let started = rows.partition_point(|&(top, _)| top < view_bottom);
    let Some(newest) = started.checked_sub(1) else {
        return Vec::new();
    };
    if n == 0 || rows[newest].1 <= view_top {
        return Vec::new();
    }
    (0..=newest).rev().take(n).collect()
}

/// How far to scroll so a row whose top sat `saved_gap` above the viewport's
/// bottom, and now sits `current_gap` above it, is back where it was.
pub(super) fn restore_delta(saved_gap: i32, current_gap: i32) -> i32 {
    saved_gap - current_gap
}

/// Read a `scroll` event as a layout change's doing or the reader's.
///
/// It is layout only if the layout changed since it was last recorded AND
/// `scrollTop` moved no more than `LAYOUT_SHIFT_ALLOWANCE_PX`.
pub(super) fn classify_scroll(
    recorded: LayoutSig,
    now: LayoutSig,
    recorded_top: i32,
    now_top: i32,
) -> ScrollCause {
    if recorded != now && (now_top - recorded_top).abs() <= LAYOUT_SHIFT_ALLOWANCE_PX {
        ScrollCause::Layout
    } else {
        ScrollCause::Reader
    }
}

/// Whether a reader `distance_from_bottom` px above the end is still following.
pub(super) fn is_pinned(distance_from_bottom: f64) -> bool {
    distance_from_bottom <= BOTTOM_THRESHOLD_PX
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEW: (i32, i32) = (0, 500);

    fn sig(scroll_height: i32, client_height: i32, client_width: i32) -> LayoutSig {
        LayoutSig {
            scroll_height,
            client_height,
            client_width,
        }
    }

    #[test]
    fn newest_visible_rows_picks_the_last_row_intersecting_the_viewport() {
        // Rows straddling the top edge, inside, and straddling the bottom edge.
        let rows = [
            (-150, -50),
            (-50, 100),
            (100, 300),
            (300, 450),
            (450, 650),
            (650, 800),
        ];
        assert_eq!(newest_visible_rows(&rows, VIEW.0, VIEW.1, 1), vec![4]);

        // The newest row ends inside the view, so the ones below it are off screen.
        assert_eq!(newest_visible_rows(&rows[..4], VIEW.0, VIEW.1, 1), vec![3]);

        // Only a row straddling the top edge is visible.
        let above = [(-300, -200), (-200, 20), (600, 700)];
        assert_eq!(newest_visible_rows(&above, VIEW.0, VIEW.1, 1), vec![1]);

        // A row taller than the viewport, covering both edges.
        let tall = [(-100, 900)];
        assert_eq!(newest_visible_rows(&tall, VIEW.0, VIEW.1, 1), vec![0]);

        assert_eq!(
            newest_visible_rows(&[], VIEW.0, VIEW.1, 1),
            Vec::<usize>::new()
        );
    }

    #[test]
    fn newest_visible_rows_returns_up_to_n_fallbacks_above_it() {
        let rows = [
            (0, 100),
            (100, 200),
            (200, 300),
            (300, 400),
            (400, 500),
            (500, 600),
        ];
        // Row 4 is the newest visible one; the n rows are it and those above it.
        assert_eq!(newest_visible_rows(&rows, 0, 450, 3), vec![4, 3, 2]);
        // Fewer rows exist than were asked for.
        assert_eq!(newest_visible_rows(&rows[..2], 0, 450, 5), vec![1, 0]);
        // The fallbacks need not be visible themselves.
        assert_eq!(newest_visible_rows(&rows, 250, 450, 4), vec![4, 3, 2, 1]);
        assert_eq!(newest_visible_rows(&rows, 0, 450, 0), Vec::<usize>::new());
    }

    #[test]
    fn a_row_touching_the_edge_by_one_pixel_is_not_visible() {
        // Its bottom edge is the viewport's top edge: no overlap at all.
        assert!(newest_visible_rows(&[(-100, 0)], 0, 500, 1).is_empty());
        // Its top edge is the viewport's bottom edge.
        assert_eq!(
            newest_visible_rows(&[(0, 100), (500, 600)], 0, 500, 1),
            vec![0]
        );
        // One pixel of overlap is visible.
        assert_eq!(newest_visible_rows(&[(-100, 1)], 0, 500, 1), vec![0]);
        assert_eq!(newest_visible_rows(&[(499, 600)], 0, 500, 1), vec![0]);
    }

    #[test]
    fn restore_delta_moves_the_row_back_to_its_gap() {
        // The row's top sat 200px above the viewport's bottom. Content above it grew
        // by 40, so it is now 160 above the bottom: scroll down 40 to bring it back.
        assert_eq!(restore_delta(200, 160), 40);
        // Content above it shrank by 40: the row is 240 above the bottom, scroll up.
        assert_eq!(restore_delta(200, 240), -40);
        assert_eq!(restore_delta(200, 200), 0);
    }

    #[test]
    fn a_small_move_with_a_changed_layout_is_layout() {
        assert_eq!(
            classify_scroll(sig(3000, 600, 1000), sig(3400, 600, 380), 2000, 1992),
            ScrollCause::Layout
        );
    }

    #[test]
    fn a_large_move_with_a_changed_layout_is_the_reader() {
        assert_eq!(
            classify_scroll(sig(3000, 600, 1000), sig(3400, 600, 380), 2000, 1800),
            ScrollCause::Reader
        );
    }

    #[test]
    fn any_move_with_an_unchanged_layout_is_the_reader() {
        let same = sig(3000, 600, 1000);
        assert_eq!(classify_scroll(same, same, 2000, 1999), ScrollCause::Reader);
        assert_eq!(classify_scroll(same, same, 2000, 2000), ScrollCause::Reader);
        assert_eq!(classify_scroll(same, same, 2000, 100), ScrollCause::Reader);
    }

    #[test]
    fn the_allowance_boundary_is_layout() {
        let (a, b) = (sig(3000, 600, 1000), sig(3400, 600, 1000));
        assert_eq!(
            classify_scroll(a, b, 2000, 2000 + LAYOUT_SHIFT_ALLOWANCE_PX),
            ScrollCause::Layout
        );
        assert_eq!(
            classify_scroll(a, b, 2000, 2000 - LAYOUT_SHIFT_ALLOWANCE_PX),
            ScrollCause::Layout
        );
        assert_eq!(
            classify_scroll(a, b, 2000, 2000 + LAYOUT_SHIFT_ALLOWANCE_PX + 1),
            ScrollCause::Reader
        );
    }

    #[test]
    fn the_allowance_matches_the_pin_threshold() {
        assert_eq!(
            LAYOUT_SHIFT_ALLOWANCE_PX as f64,
            super::super::BOTTOM_THRESHOLD_PX
        );
    }

    #[test]
    fn pinned_at_and_under_the_threshold_only() {
        assert!(is_pinned(0.0));
        assert!(is_pinned(99.9));
        assert!(is_pinned(100.0));
        assert!(!is_pinned(100.1));
        assert!(!is_pinned(101.0));
    }
}
