//! Deputize / revoke-deputy action for the member-info modal (#410).
//!
//! Deputizing grants a member authority to ban within the VIEWER's invite
//! subtree. Unlike Ban (gated on the target being downstream of the viewer),
//! this is gated on the VIEWER holding authority (owner, or a non-empty invite
//! subtree) — the deputy can be any member. It republishes the viewer's OWN
//! signed `MemberInfo` at `version + 1` with the target added to / removed from
//! `deputies`, following the same synchronous self-sign + defer pattern as the
//! nickname edit (`nickname_field.rs`); no delegate round-trip is needed
//! because the viewer signs their own record with `self_sk`.

use crate::components::app::{CURRENT_ROOM, MEMBER_INFO_MODAL, ROOMS};
use dioxus::prelude::*;
use river_core::room_state::member::MemberId;

/// Whether the DeputyButton renders at all (vs hiding entirely).
///
/// - The room OWNER is never a deputy target (they already have full authority),
///   so always hide for them.
/// - Otherwise: **Deputize** (target is not yet my deputy) requires the viewer to
///   hold authority (a non-empty invite subtree) — offering it to a viewer who
///   can ban nobody advertises power they don't have. But **Revoke** of an
///   EXISTING deputy must ALWAYS be available, even when the viewer's subtree is
///   now empty. The exact case (#411 round 9 / Codex P2): a deputy bans the
///   appointer's LAST active descendant, so `viewer_has_authority` flips false;
///   without this the whole button (including Revoke) disappears and the
///   appointer can never un-deputize them precisely after that moderation
///   action. So an existing deputy shows the Revoke branch regardless of subtree.
fn deputy_button_visible(
    viewer_has_authority: bool,
    target_is_my_deputy: bool,
    target_is_owner: bool,
) -> bool {
    if target_is_owner {
        return false;
    }
    viewer_has_authority || target_is_my_deputy
}

#[component]
pub fn DeputyButton(
    target: MemberId,
    viewer_has_authority: bool,
    target_is_my_deputy: bool,
    nickname: String,
) -> Element {
    // Visibility gate (see `deputy_button_visible`): hide for the owner, and
    // hide the Deputize offer when the viewer has no authority — but ALWAYS keep
    // Revoke available for an existing deputy even with an empty subtree (#411
    // round 9). `try_read()` for signal safety (dioxus-signal-safety): on a
    // concurrent borrow, hide rather than panic.
    let Ok(owner_key) = CURRENT_ROOM.try_read().map(|c| c.owner_key) else {
        return rsx! { "" };
    };
    let target_is_owner = owner_key
        .map(|k| MemberId::from(&k) == target)
        .unwrap_or(false);
    if !deputy_button_visible(viewer_has_authority, target_is_my_deputy, target_is_owner) {
        return rsx! { "" };
    }

    // `add == true` deputizes, `add == false` revokes. The whole
    // member_info-republish + apply lives in `RoomData::apply_deputy_change`
    // (unit-testable, and it caches `self_member_info` — #411 round 6 B).
    // Captures only Copy state (a MemberId), so the closure is Copy and usable
    // from both onclick handlers.
    let apply_change = move |add: bool| {
        // Re-read the current room fresh at click time (try_read for signal
        // safety); bail gracefully if it can't be read or none is selected.
        let Ok(Some(current_room)) = CURRENT_ROOM.try_read().map(|c| c.owner_key) else {
            return;
        };

        crate::util::defer(move || {
            let applied = ROOMS.with_mut(|rooms| {
                rooms
                    .map
                    .get_mut(&current_room)
                    .map(|room_data| room_data.apply_deputy_change(target, add))
                    .unwrap_or(false)
            });

            if applied {
                crate::components::app::mark_needs_sync(current_room);
            }
            // Close the modal either way.
            MEMBER_INFO_MODAL.with_mut(|modal| {
                modal.member = None;
            });
        });
    };

    if target_is_my_deputy {
        rsx! {
            div {
                button {
                    "data-testid": "member-info-revoke-deputy-button",
                    class: "px-4 py-2 bg-surface hover:bg-surface-hover text-text font-medium rounded-lg transition-colors border border-border whitespace-nowrap",
                    onclick: move |_| apply_change(false),
                    "Revoke deputy"
                }
                p { class: "mt-1 text-xs text-text-muted",
                    "{nickname} can currently help moderate people you invited."
                }
            }
        }
    } else {
        rsx! {
            div {
                button {
                    "data-testid": "member-info-deputize-button",
                    class: "px-4 py-2 bg-surface hover:bg-surface-hover text-text font-medium rounded-lg transition-colors border border-border whitespace-nowrap",
                    onclick: move |_| apply_change(true),
                    "Deputize"
                }
                p { class: "mt-1 text-xs text-text-muted",
                    "Let {nickname} help moderate people you invited."
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::deputy_button_visible;

    /// #411 round 9 / Codex P2: an existing deputy must remain revocable even
    /// when the viewer's invite subtree is now empty; Deputize still needs
    /// authority; the owner is never a deputy target.
    #[test]
    fn deputy_button_gate() {
        // Empty subtree + target IS my deputy -> Revoke must stay shown.
        assert!(deputy_button_visible(false, true, false));
        // Empty subtree + NOT my deputy -> hidden (no Deputize without authority).
        assert!(!deputy_button_visible(false, false, false));
        // Has authority + not my deputy -> Deputize shown.
        assert!(deputy_button_visible(true, false, false));
        // Has authority + is my deputy -> shown (Revoke).
        assert!(deputy_button_visible(true, true, false));
        // Owner target -> always hidden, regardless of the other flags.
        assert!(!deputy_button_visible(true, false, true));
        assert!(!deputy_button_visible(true, true, true));
        assert!(!deputy_button_visible(false, true, true));
    }
}
