//! Keeping keyboard focus on a message's reaction line when a render moves it.
//!
//! A message's chips are keyed siblings, each in one wrapper with its reactor
//! list. When the standing order changes, Dioxus moves the wrappers that fall
//! out of the longest run still in order, and moving a node detaches it: a
//! focused chip, or a focused reactor list inside the wrapper, loses focus to
//! `body`. Escape is handled on the chip and on the list, so from `body` it
//! no longer closes an open list.
//!
//! The group's render, where the DOM still shows the previous render, notes
//! which of its chips or lists holds focus. Once that render's edits are in
//! the DOM, focus goes back to the same element if the edits dropped it.
//! Focus the reader has put anywhere else is left where it is. An element
//! these edits removed gets nothing from `keep_across_patch`: where focus
//! goes after the reader's own removal is `focus_reaction_line`'s, below.
//!
//! Nothing is noted while the document itself is unfocused (the reader is in
//! the gateway's shell, or another window): putting focus back then could
//! pull the keyboard into River on another member's reaction. Nor when a
//! group moves without rendering, moved by the conversation's own keyed
//! list: the history's order does not move a group that stays.
//!
//! DOM only, no signals, and no render of its own (#830).

/// The id of the element to put focus back on if this render's edits drop
/// it: one of this group's reaction chips, or the reactor list focus is
/// inside (`enclosing_list_id`; the list itself, or a button in it, which
/// has no id). `None` when focus is not on this group's reaction line.
///
/// A chip's and a list's id is a prefix, then `{row id}:{emoji}`; `row_ids`
/// are the ids of the group's messages, read only for an id on some
/// reaction line.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
fn held_focus_id<'a>(
    active_id: &str,
    enclosing_list_id: Option<&str>,
    row_ids: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    let (id, prefix) = match enclosing_list_id {
        Some(list) => (list, super::reaction_owners_dialog_id("")),
        None => (active_id, super::reaction_chip_id("")),
    };
    let key = id.strip_prefix(prefix.as_str())?;
    // The row id up to its colon: `m1` does not claim `m12:👍`.
    row_ids
        .into_iter()
        .any(|row| {
            key.strip_prefix(row)
                .is_some_and(|rest| rest.starts_with(':'))
        })
        .then(|| id.to_string())
}

/// Called from a group's render, where the DOM still shows the previous
/// render. If focus is on this group's reaction line, queue putting it back
/// once this render's edits are in the DOM, should they have dropped it.
/// Queued from render, so it never runs for a group that has unmounted
/// (Dioxus drops a removed scope's queued effects).
#[cfg(target_arch = "wasm32")]
pub(super) fn keep_across_patch<'a>(row_ids: impl IntoIterator<Item = &'a str>) {
    let Some(document) = web_sys::window().and_then(|window| window.document()) else {
        return;
    };
    let Some(active) = focused_element(&document) else {
        return;
    };
    // The reader is not in this document: take no focus into it.
    if !document.has_focus().unwrap_or(false) {
        return;
    }
    let active_id = active.id();
    let list_id = active
        .closest(REACTION_OWNERS_SELECTOR)
        .ok()
        .flatten()
        .map(|list| list.id());
    let Some(id) = held_focus_id(&active_id, list_id.as_deref(), row_ids) else {
        return;
    };
    // Focus was on the list's action, its one button, which has no id.
    let on_action = list_id.is_some() && active_id != id;
    dioxus::core::queue_effect(move || {
        // Still focused, or the reader has put focus elsewhere: leave it.
        if focused_element(&document).is_some() {
            return;
        }
        // Removed by these edits: nothing to go back to.
        let Some(element) = document.get_element_by_id(&id) else {
            return;
        };
        let action = on_action
            .then(|| element.query_selector("button").ok().flatten())
            .flatten();
        super::focus_without_scrolling(action.as_ref().unwrap_or(&element));
    });
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn keep_across_patch<'a>(_row_ids: impl IntoIterator<Item = &'a str>) {}

/// Whether the element with this id holds the document's focus.
pub(super) fn element_has_focus(id: &str) -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|window| window.document())
            .and_then(|document| document.active_element())
            .is_some_and(|active| active.id() == id)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = id;
        false
    }
}

/// Whether a click came from a pointer press (mouse, touch, pen): its
/// `detail`, the press count, is at least 1. A click a key press makes
/// (Enter or Space on a button), one assistive technology sends, or one
/// from script has `detail` 0. A pointer press focuses a button in Chromium
/// and Firefox without showing it, so a focus it leaves must not be handed
/// to another chip, where a later Enter or Space would react unseen.
pub(super) fn click_is_pointer_press(
    evt: &dioxus::prelude::Event<dioxus::prelude::MouseData>,
) -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        evt.data()
            .downcast::<web_sys::MouseEvent>()
            .is_some_and(|mouse| mouse.detail() >= 1)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = evt;
        false
    }
}

/// Move focus onto the reaction line of the message in row `row_id`: the
/// first chip that takes it, leaving out the chip with id `except_chip_id`
/// (one about to be removed), else the message's footer React, which every
/// message has. Each control is tried until one actually holds focus (a row
/// being edited has no React). If none does, or the row is gone, focus stays
/// where the browser put it. Never scrolling: only an arrival moves the
/// history (history-scrolling.md).
///
/// Chips are told apart by their `id` attribute, compared as a string: an
/// emoji is peer-supplied and never goes into a selector. Whether focus
/// should move at all is the caller's to decide.
pub(super) fn focus_reaction_line(row_id: &str, except_chip_id: Option<&str>) {
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::JsCast;
        let Some(document) = web_sys::window().and_then(|w| w.document()) else {
            return;
        };
        let Some(row) = document.get_element_by_id(&format!("msg-{row_id}")) else {
            return;
        };
        for selector in [
            REACTION_CHIP_SELECTOR,
            super::mobile_surface::REACT_SELECTOR,
        ] {
            let Ok(controls) = row.query_selector_all(selector) else {
                continue;
            };
            for index in 0..controls.length() {
                let Some(control) = controls
                    .item(index)
                    .and_then(|node| node.dyn_into::<web_sys::Element>().ok())
                else {
                    continue;
                };
                if except_chip_id.is_some_and(|id| control.id() == id) {
                    continue;
                }
                if super::focus_without_scrolling(&control)
                    && document.active_element().as_ref() == Some(&control)
                {
                    return;
                }
            }
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    let _ = (row_id, except_chip_id);
}

/// Every reaction chip.
#[cfg(target_arch = "wasm32")]
const REACTION_CHIP_SELECTOR: &str = "[data-testid=\"reaction-chip\"]";

/// Every open reactor list.
#[cfg(target_arch = "wasm32")]
const REACTION_OWNERS_SELECTOR: &str = "[data-testid=\"reaction-owners\"]";

/// The element that holds focus. `None` for `body`, which is where focus
/// falls when the focused element is detached.
#[cfg(target_arch = "wasm32")]
fn focused_element(document: &web_sys::Document) -> Option<web_sys::Element> {
    use wasm_bindgen::JsCast;
    let active = document.active_element()?;
    let is_body = document
        .body()
        .is_some_and(|body| body.unchecked_ref::<web_sys::Element>() == &active);
    (!is_body).then_some(active)
}

#[cfg(test)]
mod tests {
    use super::super::{reaction_chip_id, reaction_owners_dialog_id};
    use super::held_focus_id;

    const ROWS: [&str; 2] = ["m1", "m2"];

    #[test]
    fn a_focused_chip_of_the_group_is_restored() {
        let chip = reaction_chip_id("m2:👍");
        assert_eq!(held_focus_id(&chip, None, ROWS), Some(chip.clone()));
    }

    #[test]
    fn a_chip_of_another_group_is_not() {
        assert_eq!(held_focus_id(&reaction_chip_id("m3:👍"), None, ROWS), None);
    }

    #[test]
    fn focus_inside_a_list_of_the_group_restores_the_list() {
        let list = reaction_owners_dialog_id("m1:🎉");
        // The list itself, then its action button, which has no id.
        assert_eq!(held_focus_id(&list, Some(&list), ROWS), Some(list.clone()));
        assert_eq!(held_focus_id("", Some(&list), ROWS), Some(list.clone()));
    }

    #[test]
    fn a_list_of_another_group_is_not() {
        let list = reaction_owners_dialog_id("m3:🎉");
        assert_eq!(held_focus_id("", Some(&list), ROWS), None);
    }

    #[test]
    fn focus_off_the_reaction_line_is_not() {
        for id in ["", "message-input", "msg-m1", "m1:👍"] {
            assert_eq!(held_focus_id(id, None, ROWS), None, "{id:?}");
        }
    }

    #[test]
    fn a_row_id_that_prefixes_another_does_not_claim_its_chip() {
        assert_eq!(
            held_focus_id(&reaction_chip_id("m12:👍"), None, ["m1"]),
            None
        );
        // An emoji is peer-supplied text and may hold a colon.
        let chip = reaction_chip_id("m1:a:b");
        assert_eq!(held_focus_id(&chip, None, ["m1"]), Some(chip.clone()));
    }
}
