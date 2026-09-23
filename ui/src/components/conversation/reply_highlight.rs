//! Jump-to-original for a reply's quote strip: scroll the quoted message's row
//! into view and fill its background with the bubble grey for 2s, switching on
//! and off at once, with no fade (main.css `.msg-row.reply-highlight`).
//!
//! The highlight's lifetime is the CSS animation `replyHighlight` in
//! `assets/main.css`, not a timer. A document-level `animationend` listener
//! takes the class off again, so a later jump can restart it.

use std::sync::atomic::{AtomicBool, Ordering};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

/// The class that draws the band. Styled in `assets/main.css`.
const HIGHLIGHT_CLASS: &str = "reply-highlight";
/// Must match the `@keyframes` name in `assets/main.css`.
const HIGHLIGHT_ANIMATION: &str = "replyHighlight";

static END_LISTENER_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Scroll the message row with DOM id `row_id` (`msg-{id}`) into view and
/// (re)start its highlight. The row is the band's only container: a group's
/// first row also holds its author name, so the band covers that too.
pub(super) fn jump_to_reply_target(row_id: &str) {
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let Some(row) = doc.get_element_by_id(row_id) else {
        return;
    };
    install_end_listener(&doc);

    // Stop whatever highlight is still running, here or on another message.
    // Without this, a second jump to the same message would change nothing,
    // because the class would already be on.
    if let Ok(lit) = doc.query_selector_all(&format!(".{HIGHLIGHT_CLASS}")) {
        for i in 0..lit.length() {
            if let Some(el) = lit
                .item(i)
                .and_then(|n| n.dyn_into::<web_sys::Element>().ok())
            {
                let _ = el.class_list().remove_1(HIGHLIGHT_CLASS);
            }
        }
    }

    // Reading layout forces a style flush, so the browser sees the class come
    // off before it goes back on and starts the animation again from 0.
    // Adding the class never changes layout, so the rect stays valid.
    let rect = row.get_bounding_client_rect();
    let _ = row.class_list().add_1(HIGHLIGHT_CLASS);

    scroll_region_into_view(&row, rect.top(), rect.bottom());
}

/// Room left above and below the row when scrolling to it, so the band's
/// rounded corners stay in view.
const SCROLL_MARGIN_PX: f64 = 12.0;

/// Scroll the history so the row (`top` to `bottom`, viewport coordinates)
/// sits in the middle of the view, or at its top when it is taller than the
/// view, which `scrollIntoView` cannot express: centring a tall row would push
/// its author name off the top.
fn scroll_region_into_view(row: &web_sys::Element, top: f64, bottom: f64) {
    let Some(scroller) = row.closest("#chat-scroll-container").ok().flatten() else {
        row.scroll_into_view();
        return;
    };
    let view = scroller.get_bounding_client_rect();
    let delta = if bottom - top + 2.0 * SCROLL_MARGIN_PX <= view.height() {
        (top + bottom) / 2.0 - (view.top() + view.height() / 2.0)
    } else {
        top - SCROLL_MARGIN_PX - view.top()
    };
    scroller.scroll_by_with_x_and_y(0.0, delta);
}

/// One listener for the whole document, installed on the first jump. It takes
/// the class off when the band's animation finishes.
///
/// Taking the class off is not a visible change: the animation ends with a
/// short tail that already shows the row's own background (main.css), so
/// there is no colour difference for `.msg-row`'s hover transition to fade.
///
/// `animationend` bubbles, so a document listener sees every row. The
/// animation name check keeps any other animation from clearing the class.
fn install_end_listener(doc: &web_sys::Document) {
    if END_LISTENER_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let cb = Closure::wrap(Box::new(move |evt: web_sys::Event| {
        let Some(anim) = evt.dyn_ref::<web_sys::AnimationEvent>() else {
            return;
        };
        if anim.animation_name() != HIGHLIGHT_ANIMATION {
            return;
        }
        if let Some(el) = evt
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        {
            let _ = el.class_list().remove_1(HIGHLIGHT_CLASS);
        }
    }) as Box<dyn FnMut(web_sys::Event)>);
    let _ = doc.add_event_listener_with_callback("animationend", cb.as_ref().unchecked_ref());
    // Lives for the page: the conversation view is never torn down.
    cb.forget();
}
