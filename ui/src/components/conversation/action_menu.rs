//! The touch action menu: ONE `popover="auto"` for the whole conversation.
//!
//! Each message's kebab is a `popovertarget` invoker for it, so the browser opens
//! and closes it, light-dismisses it on an outside tap or Esc, keeps only one open,
//! and makes the tapped kebab its implicit anchor. It lives in the top layer, so no
//! later message group paints over it. Rust keeps one thing: [`MenuTarget`].

use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::{
    FaEllipsisVertical, FaPenToSquare, FaReply, FaTrashCan,
};
use dioxus_free_icons::Icon;
use river_core::room_state::message::MessageId;
use wasm_bindgen::JsCast;

use super::ReplyContext;

/// DOM id of the menu, and the value every kebab names in `popovertarget`.
pub(super) const ACTION_MENU_ID: &str = "message-action-menu";

/// Which message the open menu acts on. Set by the kebab tap, cleared when the menu closes.
#[derive(Clone, PartialEq)]
pub(super) struct MenuTarget {
    pub(super) message_id: MessageId,
    /// The row's `msg.id`, which `edit_trigger` and the kebab's `aria-expanded` match on.
    pub(super) dom_id: String,
    pub(super) is_self: bool,
    pub(super) reply: ReplyContext,
    pub(super) edit_text: String,
}

fn menu_element() -> Option<web_sys::HtmlElement> {
    web_sys::window()?
        .document()?
        .get_element_by_id(ACTION_MENU_ID)?
        .dyn_into()
        .ok()
}

/// Whether the menu is showing, read from the DOM, which owns that state.
fn menu_is_open() -> bool {
    menu_element().is_some_and(|el| el.matches(":popover-open").unwrap_or(false))
}

/// Close the menu if it is open; its `toggle` handler then clears the target. A DOM call, not a signal write.
pub(super) fn close_action_menu() {
    if let Some(el) = menu_element().filter(|el| el.matches(":popover-open").unwrap_or(false)) {
        let _ = el.hide_popover();
    }
}

#[component]
pub(super) fn ActionMenu(
    target: Signal<Option<MenuTarget>>,
    on_reply: EventHandler<ReplyContext>,
    edit_trigger: Signal<Option<(String, String)>>,
    on_request_delete: EventHandler<MessageId>,
) -> Element {
    let is_self = target.read().as_ref().is_some_and(|t| t.is_self);
    rsx! {
        div {
            id: ACTION_MENU_ID,
            "data-testid": "message-action-menu",
            popover: "auto",
            // No display utility on the root: it would show a closed popover.
            class: "bg-panel text-text rounded-lg shadow-lg border border-border py-1 min-w-[8rem]",
            // Fires on every open and close, however it happened; a close clears the target.
            ontoggle: move |_| {
                if !menu_is_open() {
                    let mut target = target;
                    crate::util::defer(move || target.set(None));
                }
            },
            div { class: "flex flex-col",
                button {
                    r#type: "button",
                    // Closes the menu natively, after `onclick` has read the target.
                    popovertarget: ACTION_MENU_ID,
                    popovertargetaction: "hide",
                    class: "flex items-center gap-2 px-3 py-2 text-sm text-text hover:bg-surface text-left",
                    onclick: move |_| {
                        if let Some(t) = target.peek().clone() {
                            crate::util::defer(move || on_reply.call(t.reply));
                        }
                    },
                    Icon { icon: FaReply, width: 14, height: 14 }
                    "Reply"
                }
                if is_self {
                    button {
                        r#type: "button",
                        popovertarget: ACTION_MENU_ID,
                        popovertargetaction: "hide",
                        class: "flex items-center gap-2 px-3 py-2 text-sm text-text hover:bg-surface text-left",
                        onclick: move |_| {
                            if let Some(t) = target.peek().clone() {
                                let mut edit_trigger = edit_trigger;
                                crate::util::defer(move || edit_trigger.set(Some((t.dom_id, t.edit_text))));
                            }
                        },
                        Icon { icon: FaPenToSquare, width: 14, height: 14 }
                        "Edit"
                    }
                    button {
                        r#type: "button",
                        popovertarget: ACTION_MENU_ID,
                        popovertargetaction: "hide",
                        class: "flex items-center gap-2 px-3 py-2 text-sm text-red-500 hover:bg-error-bg text-left",
                        onclick: move |_| {
                            if let Some(t) = target.peek().clone() {
                                crate::util::defer(move || on_request_delete.call(t.message_id));
                            }
                        },
                        Icon { icon: FaTrashCan, width: 14, height: 14 }
                        "Delete"
                    }
                }
            }
        }
    }
}

/// A message's kebab: an invoker for the shared menu.
///
/// Its own component so opening the menu re-renders only these buttons.
#[component]
pub(super) fn KebabButton(
    target_for_me: MenuTarget,
    menu_target: Signal<Option<MenuTarget>>,
) -> Element {
    let open_for_me = menu_target
        .read()
        .as_ref()
        .is_some_and(|t| t.dom_id == target_for_me.dom_id);
    rsx! {
        button {
            r#type: "button",
            class: "flex items-center justify-center w-8 h-8 rounded-full bg-panel shadow-md border border-border text-text-muted",
            "aria-label": "Message actions",
            "aria-haspopup": "menu",
            "aria-expanded": "{open_for_me}",
            "data-testid": "message-kebab",
            popovertarget: ACTION_MENU_ID,
            onclick: move |_| {
                // The browser toggles after this handler, so only an opening tap names the target.
                if !menu_is_open() {
                    let t = target_for_me.clone();
                    let mut menu_target = menu_target;
                    crate::util::defer(move || menu_target.set(Some(t)));
                }
            },
            Icon { icon: FaEllipsisVertical, width: 16, height: 16 }
        }
    }
}
