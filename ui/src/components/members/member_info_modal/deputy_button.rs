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

#[component]
pub fn DeputyButton(
    target: MemberId,
    viewer_has_authority: bool,
    target_is_my_deputy: bool,
    nickname: String,
) -> Element {
    // Anti-confusion: a viewer with an empty invite subtree can ban nobody, so
    // offering them "Deputize" advertises power they don't have. Also never
    // offer to deputize the room OWNER — they already have full authority and
    // the contract treats it as a no-op (belt-and-suspenders: the modal already
    // only mounts this for non-owner targets, #410 review round 1). Hide
    // entirely in either case. `try_read()` for signal safety
    // (dioxus-signal-safety): on a concurrent borrow, hide rather than panic.
    let Ok(owner_key) = CURRENT_ROOM.try_read().map(|c| c.owner_key) else {
        return rsx! { "" };
    };
    let target_is_owner = owner_key
        .map(|k| MemberId::from(&k) == target)
        .unwrap_or(false);
    if !viewer_has_authority || target_is_owner {
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
