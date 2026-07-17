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
use crate::room_data::RoomData;
use dioxus::logger::tracing::{error, info};
use dioxus::prelude::*;
use freenet_scaffold::ComposableState;
use river_core::room_state::member::MemberId;
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo, MAX_DEPUTIES};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};

#[component]
pub fn DeputyButton(
    target: MemberId,
    viewer_has_authority: bool,
    target_is_my_deputy: bool,
    nickname: String,
) -> Element {
    let current_room_data_signal: Memo<Option<RoomData>> = use_memo(move || {
        CURRENT_ROOM
            .read()
            .owner_key
            .as_ref()
            .and_then(|key| ROOMS.try_read().ok()?.map.get(key).cloned())
    });

    // Anti-confusion: a viewer with an empty invite subtree can ban nobody, so
    // offering them "Deputize" advertises power they don't have. Also never
    // offer to deputize the room OWNER — they already have full authority and
    // the contract treats it as a no-op (belt-and-suspenders: the modal already
    // only mounts this for non-owner targets, #410 review round 1). Hide
    // entirely in either case.
    let target_is_owner = CURRENT_ROOM
        .read()
        .owner_key
        .map(|k| MemberId::from(&k) == target)
        .unwrap_or(false);
    if !viewer_has_authority || target_is_owner {
        return rsx! { "" };
    }

    // `add == true` deputizes, `add == false` revokes. Captures only Copy state
    // (a Memo and a MemberId), so the closure is Copy and usable from both
    // onclick handlers.
    let apply_change = move |add: bool| {
        let (Some(current_room), Some(room_data)) = (
            CURRENT_ROOM.read().owner_key,
            current_room_data_signal.read().as_ref().cloned(),
        ) else {
            return;
        };
        let self_sk = room_data.self_sk.clone();
        let self_id = MemberId::from(&self_sk.verifying_key());

        crate::util::defer(move || {
            let applied = ROOMS.with_mut(|rooms| {
                let Some(room_data_mut) = rooms.map.get_mut(&current_room) else {
                    return false;
                };

                // The viewer's current signed member_info (highest version).
                let Some(current_self) = room_data_mut
                    .room_state
                    .member_info
                    .member_info
                    .iter()
                    .filter(|i| i.member_info.member_id == self_id)
                    .max_by_key(|i| i.member_info.version)
                    .cloned()
                else {
                    error!("Cannot manage deputies: no member_info for self yet");
                    return false;
                };

                let mut deputies = current_self.member_info.deputies.clone();
                if add {
                    if deputies.contains(&target) {
                        return false; // already a deputy, nothing to publish
                    }
                    if deputies.len() >= MAX_DEPUTIES {
                        error!("Cannot deputize: already at the maximum of {MAX_DEPUTIES}");
                        return false;
                    }
                    deputies.push(target);
                } else if let Some(pos) = deputies.iter().position(|d| *d == target) {
                    deputies.remove(pos);
                } else {
                    return false; // not a deputy, nothing to publish
                }

                // Republish our own member_info at version+1, preserving the
                // (already-sealed) nickname; only `deputies` changes.
                let new_info = MemberInfo {
                    member_id: self_id,
                    version: current_self.member_info.version + 1,
                    preferred_nickname: current_self.member_info.preferred_nickname.clone(),
                    deputies,
                };
                let authorized = AuthorizedMemberInfo::new_with_member_key(new_info, &self_sk);

                // Re-add ourselves if we were pruned for inactivity — a
                // member_info-only UPDATE for a non-member would be rejected.
                let members_delta = room_data_mut.build_rejoin_delta().0;
                let parent = room_data_mut.room_state.clone();
                let delta = ChatRoomStateV1Delta {
                    member_info: Some(vec![authorized]),
                    members: members_delta,
                    ..Default::default()
                };
                if let Err(e) = room_data_mut.room_state.apply_delta(
                    &parent,
                    &ChatRoomParametersV1 {
                        owner: current_room,
                    },
                    &Some(delta),
                ) {
                    error!("Failed to apply deputy delta: {e:?}");
                    return false;
                }
                // apply_delta re-runs the public-only rebuild_actions_state,
                // wiping private edits/reactions; re-derive with decryption.
                // No-op on public rooms.
                room_data_mut.rebuild_private_actions_state();
                info!("Deputy change applied for {target:?} (deputize={add})");
                true
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
