//! A reply strip's jump: bring the quoted message's row into the room history's
//! view and highlight the whole row for two seconds.
//!
//! DOM only, no signals, so the strip's click needs nothing from the group's
//! props (#830). Only the history's `scrollTop` moves: `scrollIntoView` would
//! also scroll the shell, the header and `overflow: hidden` ancestors. The
//! jump is an ordinary scroll of the history, so the end hold, the reading
//! position and the follow rule treat it as a reader's own move
//! (`.claude/rules/history-scrolling.md`): one away from the end releases the
//! hold, and its settle records where the reader now is.
//!
//! A quoted message that is not rendered (outside the rendered range, or no
//! longer in the room) is left alone: there is no lookup or paging.

/// Space kept above a quoted message taller than the view, which is aligned to
/// the view's top rather than centred.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
const TALL_CLEARANCE_PX: f64 = 12.0;

/// How far to scroll the history (positive: down) to place a row spanning
/// `top..top + height` in a view spanning `view_top..view_top + view_height`:
/// centred when it fits with the clearance above and below, else its top
/// `TALL_CLEARANCE_PX` below the view's top.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
fn jump_shift(top: f64, height: f64, view_top: f64, view_height: f64) -> f64 {
    if height + 2.0 * TALL_CLEARANCE_PX > view_height {
        top - (view_top + TALL_CLEARANCE_PX)
    } else {
        (top + height / 2.0) - (view_top + view_height / 2.0)
    }
}

/// Jump the room history to the row `row_id` (`msg-…`) and highlight it.
/// Nothing happens when the row is not rendered in the history, or the
/// history has no layout.
#[cfg(target_arch = "wasm32")]
pub(super) fn jump_to_reply_target(row_id: &str) {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let (Some(row), Some(history)) = (
        document.get_element_by_id(row_id),
        super::history_scroll::ROOM_HISTORY.container(),
    ) else {
        return;
    };
    if !history.contains(Some(&row)) || !crate::components::foreground::has_layout(&history) {
        return;
    }
    let (r, view) = (
        row.get_bounding_client_rect(),
        history.get_bounding_client_rect(),
    );
    let shift = jump_shift(
        r.top(),
        r.height(),
        view.top() + history.client_top() as f64,
        history.client_height() as f64,
    )
    .round();
    if shift != 0.0 {
        // Instant, as every other move of the history is.
        crate::components::scroll_to_latest::scroll_instant_to(
            &history,
            history.scroll_top() as f64 + shift,
        );
    }
    restart_highlight(&row);
}

/// Natively there is no DOM.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn jump_to_reply_target(_row_id: &str) {}

/// The highlight, through the Web Animations API: the row's background at
/// full `--color-surface` from the first frame to the last, then nothing, so
/// whatever the row shows underneath (its own tint, the hover band) is back
/// at once, with no fade. A new highlight cancels the one still running,
/// on this row or another, so a second jump restarts it and only one row is
/// ever lit. The animation's id, `reply-highlight`, is what the browser tests
/// pause and seek.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(inline_js = "
let live = null;
export function restart_highlight(row) {
    if (live) {
        live.cancel();
        live = null;
    }
    if (typeof row.animate !== 'function') {
        return;
    }
    const surface = getComputedStyle(row).getPropertyValue('--color-surface').trim();
    if (!surface) {
        return;
    }
    live = row.animate(
        [{ backgroundColor: surface }, { backgroundColor: surface }],
        { duration: 2000, easing: 'linear', id: 'reply-highlight' },
    );
}
")]
extern "C" {
    fn restart_highlight(row: &web_sys::Element);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_that_fits_is_centred() {
        // A 100px row at 700..800 in a 400px view at 50..450: its centre (750)
        // goes to the view's (250).
        assert_eq!(jump_shift(700.0, 100.0, 50.0, 400.0), 500.0);
        // Above the view: a negative shift.
        assert_eq!(jump_shift(-300.0, 100.0, 50.0, 400.0), -500.0);
        // Already centred: nothing.
        assert_eq!(jump_shift(200.0, 100.0, 50.0, 400.0), 0.0);
    }

    #[test]
    fn a_row_taller_than_the_view_is_top_aligned_with_clearance() {
        // 900px in a 400px view: its top goes 12px below the view's top.
        assert_eq!(jump_shift(700.0, 900.0, 50.0, 400.0), 700.0 - 62.0);
        // Too tall to centre with the clearance on both sides counts too.
        assert_eq!(jump_shift(700.0, 380.0, 50.0, 400.0), 700.0 - 62.0);
        // The last height that still centres.
        assert_eq!(jump_shift(700.0, 376.0, 50.0, 400.0), 888.0 - 250.0);
    }
}
