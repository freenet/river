//! Whether a conversation is in the foreground: the tab is visible, its
//! history has layout, and, for the room, no DM thread covers it. The read
//! rule asks this, for the room history and the DM thread alike
//! (`.claude/rules/history-scrolling.md`). Following an arrival does not: it
//! needs only layout.
//!
//! Other dialogs and popovers do not count: an arrival under them is read as
//! if they were not there.
//!
//! The read rule re-checks when either input changes without a scroll, a
//! resize or an observer report: its effects subscribe to
//! `document_title::DOCUMENT_VISIBLE` and, for the room,
//! `direct_messages::OPEN_DM_THREAD`.

/// Does a history container have layout? On a phone the chat panel is
/// `display:none` behind Rooms or Members, and every row then measures 0.
/// `MOBILE_VIEW` can't tell: on desktop all panels show whatever it says.
#[cfg(target_arch = "wasm32")]
pub fn has_layout(container: &web_sys::Element) -> bool {
    container.client_height() > 0
}

/// Is a DM thread open over the room? Peeked, not read: called from render and
/// raw DOM callbacks, which must not subscribe. A contended read counts as
/// open, the answer that does not read.
#[cfg(target_arch = "wasm32")]
fn dm_thread_open() -> bool {
    use dioxus::prelude::*;
    crate::components::direct_messages::OPEN_DM_THREAD
        .try_peek()
        .map_or(true, |thread| thread.is_some())
}

/// The tab is visible, `container` has layout, and, when `under_dm_thread`
/// (the room history), no DM thread is open.
#[cfg(target_arch = "wasm32")]
pub fn in_foreground(under_dm_thread: bool, container: &web_sys::Element) -> bool {
    crate::components::app::document_title::get_visibility_state()
        && has_layout(container)
        && !(under_dm_thread && dm_thread_open())
}
