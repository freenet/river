mod ban_button;
mod deputy_button;
mod invited_by_field;
mod nickname_field;

use crate::components::app::{CURRENT_ROOM, MEMBER_INFO_MODAL, ROOMS};
use crate::components::direct_messages::{open_dm_thread, open_invite_via_dm_picker};
use crate::components::members::member_info_modal::ban_button::BanButton;
use crate::components::members::member_info_modal::deputy_button::DeputyButton;
use crate::components::members::member_info_modal::invited_by_field::InvitedByField;
use crate::components::members::member_info_modal::nickname_field::NicknameField;
use crate::util::ecies::unseal_bytes_with_secrets;
use dioxus::logger::tracing::*;
use dioxus::prelude::*;
use river_core::room_state::member::MemberId;
use river_core::room_state::ChatRoomParametersV1;

/// Whether `viewer` may ban `target` — the Ban-button gate (#410 / #411 round 4
/// D). Uses [`MembersV1::is_ban_authorized`] (owner / invite-ancestor / deputy),
/// NOT bare invite-chain ancestry, so a DEPUTY sees the Ban action for members in
/// their deputizer's subtree. Bare downstream ancestry (`is_downstream`, still
/// used for the "🔑 Invited by You" relationship tag) would have hidden it.
fn viewer_can_ban(
    members: &river_core::room_state::member::MembersV1,
    member_info: &river_core::room_state::member_info::MemberInfoV1,
    viewer: MemberId,
    target: MemberId,
    owner_id: MemberId,
) -> bool {
    let members_by_id = members.members_by_member_id();
    river_core::room_state::member::MembersV1::is_ban_authorized(
        viewer,
        target,
        &members_by_id,
        member_info,
        owner_id,
    )
}

#[component]
pub fn MemberInfoModal() -> Element {
    // Memos
    let current_room_data_signal = use_memo(move || {
        CURRENT_ROOM
            .read()
            .owner_key
            .as_ref()
            .and_then(|key| ROOMS.try_read().ok()?.map.get(key).cloned())
    });
    let self_member_id: Memo<Option<MemberId>> = use_memo(move || {
        ROOMS
            .try_read()
            .ok()?
            .map
            .get(&CURRENT_ROOM.read().owner_key?)
            .map(|r| MemberId::from(&r.self_sk.verifying_key()))
    });

    // Memoized values
    let owner_key_signal = use_memo(move || CURRENT_ROOM.read().owner_key);

    // Effect to handle closing the modal based on a specific condition

    // Event handlers
    let handle_close_modal = {
        move |_| {
            crate::util::defer(move || {
                MEMBER_INFO_MODAL.with_mut(|signal| {
                    signal.member = None;
                });
            });
        }
    };

    // Room state - create a longer-lived binding
    let current_room_data = current_room_data_signal.read();
    let room_state = match current_room_data.as_ref() {
        Some(state) => state,
        None => {
            return rsx! { div { "Room state not available" } };
        }
    };

    // Resolve `self_member_id` once at the top so the later
    // `self_member_id` sites are panic-safe under a
    // concurrent ROOMS-write race (Skeptical M2 on PR #260). The
    // pre-existing code unwraps in three places; this single
    // early-return covers all of them.
    let self_member_id: MemberId = match self_member_id() {
        Some(id) => id,
        None => return rsx! {},
    };

    // Count rooms other than the current one — used to gate the
    // "Share invite" button so it doesn't lead to an empty picker
    // (Skeptical L1 on PR #260).
    let other_rooms_count = ROOMS
        .try_read()
        .map(|r| {
            let current = CURRENT_ROOM.read().owner_key;
            r.map.keys().filter(|k| Some(**k) != current).count()
        })
        .unwrap_or(0);

    // Extract member info and members list
    let member_info_v1 = &room_state.room_state.member_info;
    let members_list = &room_state.room_state.members.members;

    let modal_content = if let Some(member_id) = MEMBER_INFO_MODAL.read().member {
        // Find the CANONICAL AuthorizedMemberInfo for the given member_id
        // (highest member_info_rank: version, then signature bytes) — not a
        // bare first-match. `verify` accepts duplicate member_info records
        // per member_id (migration safety), so a first-match `.find()` can
        // read a losing (e.g. revoked) record (freenet/river#411 round 8).
        let member_info = match member_info_v1.canonical(member_id) {
            Some(mi) => mi,
            None => {
                error!("Member info not found for member {member_id}");
                return rsx! {
                    div {
                        class: "p-4 bg-red-500/10 border border-red-500/20 rounded-lg text-red-400",
                        "Member information is missing or corrupted"
                    }
                };
            }
        };

        // Try to find the AuthorizedMember for the given member_id
        let member = members_list.iter().find(|m| m.member.id() == member_id);

        // Determine if the member is the room owner
        let is_owner = owner_key_signal
            .as_ref()
            .is_some_and(|k| MemberId::from(&*k) == member_id);

        // Only show error if member isn't found AND isn't the owner
        if member.is_none() && !is_owner {
            error!("Member {member_id} not found in members list and is not owner");
            return rsx! {
                div {
                    class: "p-4 bg-red-500/10 border border-red-500/20 rounded-lg text-red-400",
                    "Member not found in room members list"
                }
            };
        }

        // Determine if the member is downstream of the current user in the invite chain
        let is_downstream = member
            .and_then(|m| {
                owner_key_signal.as_ref().map(|owner| {
                    let params = ChatRoomParametersV1 { owner: *owner };
                    // Get the invite chain for this member
                    let invite_chain = room_state.room_state.members.get_invite_chain(m, &params);

                    // `self_member_id` (a `MemberId`, resolved at modal-top
                    // with an early-return) is captured by this closure.
                    // Member is downstream if:
                    // 1. Current user is owner (owner can ban anyone), or
                    // 2. Current user appears in their invite chain (upstream of target)
                    invite_chain.is_ok_and(|chain| {
                        self_member_id == CURRENT_ROOM.read().owner_id().unwrap()
                            || chain.iter().any(|m| m.member.id() == self_member_id)
                    })
                })
            })
            .unwrap_or(false);

        // Ban authority (#410 / #411 round 4 D). The Ban button is gated on REAL
        // ban authority — owner / invite-ancestor / deputy — via
        // `is_ban_authorized`, NOT bare downstream ancestry (`is_downstream`), so a
        // deputy sees Ban for members in their deputizer's subtree. `is_downstream`
        // is still used above for the "🔑 Invited by You" relationship tag, which
        // is a different meaning and must not change.
        let can_ban = owner_key_signal
            .as_ref()
            .map(|owner| {
                viewer_can_ban(
                    &room_state.room_state.members,
                    &room_state.room_state.member_info,
                    self_member_id,
                    member_id,
                    MemberId::from(&*owner),
                )
            })
            .unwrap_or(false);

        info!(
            "Rendering MemberInfoModal for member_id: {:?} is_owner: {:?} is_downstream: {:?} can_ban: {:?}",
            member_id, is_owner, is_downstream, can_ban
        );

        // Get the inviter's nickname and ID
        let (invited_by, inviter_id) = match (member, is_owner) {
            (_, true) => ("N/A (Room Owner)".to_string(), None),
            (Some(m), false) => {
                let inviter_id = m.member.invited_by;
                let nickname = member_info_v1
                    .canonical(inviter_id)
                    .map(|mi| {
                        match unseal_bytes_with_secrets(
                            &mi.member_info.preferred_nickname,
                            &room_state.secrets,
                        ) {
                            Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                            Err(_) => mi.member_info.preferred_nickname.to_string_lossy(),
                        }
                    })
                    .unwrap_or_else(|| "Unknown".to_string());
                (nickname, Some(inviter_id))
            }
            _ => ("Unknown".to_string(), None),
        };

        // Deputy authority gating (#410). Unlike Ban (gated on the TARGET being
        // downstream), Deputize is gated on the VIEWER holding authority: the
        // owner, or a member whose own invite subtree is non-empty. The deputy
        // can be any member, so this shows in any member's modal (except self /
        // owner as targets); it is hidden entirely for a viewer with an empty
        // subtree to avoid advertising power they don't have.
        let viewer_has_authority = {
            let viewer_is_owner = owner_key_signal
                .as_ref()
                .is_some_and(|k| MemberId::from(&*k) == self_member_id);
            if viewer_is_owner {
                true
            } else if let Some(owner) = owner_key_signal.as_ref() {
                let params = ChatRoomParametersV1 { owner: *owner };
                room_state.room_state.members.members.iter().any(|m| {
                    m.member.id() != self_member_id
                        && room_state
                            .room_state
                            .members
                            .get_invite_chain(m, &params)
                            .map(|chain| chain.iter().any(|a| a.member.id() == self_member_id))
                            .unwrap_or(false)
                })
            } else {
                false
            }
        };
        // Whether the target is currently one of the VIEWER's own deputies.
        let target_is_my_deputy = room_state
            .room_state
            .member_info
            .deputies_of(self_member_id)
            .contains(&member_id);

        // Viewer-relevant deputizer names for the target, for the 🛡 legend
        // chip below (freenet/river#451). Computed with the SAME shared helpers
        // the member-list row uses, so the modal shows the shield under exactly
        // the same condition — and with the same "appointed by …" tooltip — as
        // the row does. Empty ⇒ this member does not show the shield here.
        let deputizer_names: Vec<String> = owner_key_signal
            .as_ref()
            .map(|owner| {
                let owner_id = MemberId::from(&*owner);
                let member_info_all = &room_state.room_state.member_info;
                let deputizers_of = super::build_deputizers_of(member_info_all);
                let viewer_relevant = super::viewer_relevant_deputizer_set(
                    &room_state.room_state.members,
                    owner_id,
                    self_member_id,
                );
                super::relevant_deputizer_names(
                    member_info_all,
                    &room_state.secrets,
                    &deputizers_of,
                    &viewer_relevant,
                    owner_id,
                    self_member_id,
                    member_id,
                )
            })
            .unwrap_or_default();
        let deputy_tooltip = format!("Deputy (appointed by {})", deputizer_names.join(", "));
        // Decrypted display nickname for the target (for the deputy action copy).
        let target_nickname = match unseal_bytes_with_secrets(
            &member_info.member_info.preferred_nickname,
            &room_state.secrets,
        ) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
            Err(_) => member_info.member_info.preferred_nickname.to_string_lossy(),
        };

        // Get the member ID string to display
        let member_id_str = member_id.to_string();

        rsx! {
            // Modal backdrop
            div {
                class: "fixed inset-0 z-50 flex items-center justify-center",
                tabindex: "0",
                onmounted: move |cx| {
                    let element = cx.data();
                    wasm_bindgen_futures::spawn_local(async move {
                        let _ = element.set_focus(true).await;
                    });
                },
                onkeydown: move |evt: KeyboardEvent| {
                    if evt.key() == Key::Escape || evt.key() == Key::Enter {
                        evt.prevent_default();
                        crate::util::defer(move || {
                            MEMBER_INFO_MODAL.with_mut(|signal| {
                                signal.member = None;
                            });
                        });
                    }
                },
                // Overlay
                div {
                    class: "absolute inset-0 bg-black/50",
                    onclick: handle_close_modal
                }
                // Modal content
                div {
                    "data-testid": "member-info-modal",
                    class: "relative z-10 w-full max-w-md mx-4 bg-panel rounded-xl shadow-xl border border-border",
                    div {
                        class: "p-6",
                        h1 { class: "text-xl font-semibold text-text mb-4", "Member Info" }

                        // Show tags for owner, self, and relationships
                        div { class: "flex flex-wrap gap-2 mb-4",
                            if is_owner {
                                span {
                                    class: "inline-flex items-center px-2.5 py-0.5 rounded-full text-sm font-medium bg-blue-500/20 text-blue-400",
                                    "👑 Room Owner"
                                }
                            }
                            if member_id == self_member_id {
                                span {
                                    class: "inline-flex items-center px-2.5 py-0.5 rounded-full text-sm font-medium bg-cyan-500/20 text-cyan-400",
                                    "⭐ You"
                                }
                            }
                            if is_downstream {
                                span {
                                    class: "inline-flex items-center px-2.5 py-0.5 rounded-full text-sm font-medium bg-green-500/20 text-green-400",
                                    "🔑 Invited by You"
                                }
                            }
                            // Check if this member invited the current user
                            if let Some(self_member) = members_list.iter().find(|m| m.member.id() == self_member_id) {
                                if self_member.member.invited_by == member_id {
                                    span {
                                        class: "inline-flex items-center px-2.5 py-0.5 rounded-full text-sm font-medium bg-yellow-500/20 text-yellow-400",
                                        "🎪 Invited You"
                                    }
                                }
                            }
                            // Deputy shield — mirrors the member-list 🛡 badge
                            // (freenet/river#451). Shown under the same
                            // viewer-relevant condition as the row, with the
                            // appointer names in the tooltip.
                            if !deputizer_names.is_empty() {
                                span {
                                    "data-testid": "member-info-deputy-tag",
                                    class: "inline-flex items-center px-2.5 py-0.5 rounded-full text-sm font-medium bg-purple-500/20 text-purple-400",
                                    title: "{deputy_tooltip}",
                                    "🛡 Deputy"
                                }
                            }
                        }

                        NicknameField {
                            member_info: member_info.clone()
                        }

                        div {
                            class: "mb-4",
                            label { class: "block text-sm font-medium text-text-muted mb-2", "Member ID" }
                            input {
                                "data-testid": "member-info-id-input",
                                class: "w-full px-3 py-2 bg-surface border border-border rounded-lg text-text font-mono text-sm",
                                value: "{member_id_str}",
                                readonly: true
                            }
                        }

                        // Member-action buttons — skip for self (no self-DMs).
                        // Side-by-side flex row, equal-weight styling, short
                        // labels: neither action is "primary" over the
                        // other so giving one an accent colour and the
                        // other surface (as we had) reads as arbitrary.
                        // Both now use the surface style with a hover
                        // accent border. Ban remains separate below
                        // because it's destructive — different styling
                        // is intentional there.
                        if member_id != self_member_id {
                            {
                                let dm_room = owner_key_signal.unwrap();
                                let share_button_enabled = other_rooms_count > 0;
                                rsx! {
                                    div { class: "mb-4 flex gap-2",
                                        button {
                                            "data-testid": "member-info-dm-button",
                                            class: "flex-1 px-3 py-1.5 bg-surface hover:bg-surface-hover text-text text-sm font-medium rounded-lg transition-colors border border-border",
                                            "aria-label": "Send direct message",
                                            onclick: move |_| {
                                                crate::util::defer(move || {
                                                    MEMBER_INFO_MODAL.with_mut(|signal| {
                                                        signal.member = None;
                                                    });
                                                });
                                                open_dm_thread(dm_room, member_id);
                                            },
                                            "DM"
                                        }
                                        button {
                                            "data-testid": "member-info-share-invite-button",
                                            class: format!(
                                                "flex-1 px-3 py-1.5 text-sm font-medium rounded-lg transition-colors border border-border {}",
                                                if share_button_enabled {
                                                    "bg-surface hover:bg-surface-hover text-text"
                                                } else {
                                                    "bg-surface text-text-muted opacity-60 cursor-not-allowed"
                                                }
                                            ),
                                            disabled: !share_button_enabled,
                                            "aria-label": if share_button_enabled {
                                                "Share an invite to one of your other rooms via direct message"
                                            } else {
                                                "Share invite is disabled — you are not a member of any other rooms"
                                            },
                                            title: if share_button_enabled {
                                                "Generate an invite to one of your other rooms and drop it in a DM here"
                                            } else {
                                                "You aren't a member of any other rooms yet"
                                            },
                                            onclick: move |_| {
                                                if share_button_enabled {
                                                    open_invite_via_dm_picker(dm_room, member_id);
                                                }
                                            },
                                            "Share invite"
                                        }
                                    }
                                }
                            }
                        }

                        if !is_owner {
                            InvitedByField {
                                invited_by: invited_by.clone(),
                                inviter_id: inviter_id,
                            }

                            // Ban + Deputize sit in one row (Ban on the left,
                            // Deputize on the right), matching the DM /
                            // Share-invite button row above.
                            div { class: "mt-4 flex items-start gap-3",
                                BanButton {
                                    member_to_ban: member_id,
                                    can_ban: can_ban,
                                    nickname: member_info.member_info.preferred_nickname.clone()
                                }

                                // Deputize / revoke-deputy (#410). Any non-owner
                                // member (except self) may be deputized; the action
                                // hides itself when the viewer lacks authority.
                                if member_id != self_member_id {
                                    DeputyButton {
                                        target: member_id,
                                        viewer_has_authority: viewer_has_authority,
                                        target_is_my_deputy: target_is_my_deputy,
                                        nickname: target_nickname.clone(),
                                    }
                                }
                            }
                        }
                    }
                    // Close button
                    button {
                        "data-testid": "member-info-close-button",
                        class: "absolute top-3 right-3 p-1 text-text-muted hover:text-text transition-colors",
                        onclick: handle_close_modal,
                        "✕"
                    }
                }
            }
        }
    } else {
        rsx! {}
    };

    modal_content
}

#[cfg(test)]
mod ban_gate_tests {
    use super::viewer_can_ban;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;
    use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersV1};
    use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo, MemberInfoV1};

    fn member(
        sk: &SigningKey,
        inviter_id: MemberId,
        inviter_sk: &SigningKey,
        owner_id: MemberId,
    ) -> AuthorizedMember {
        AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: inviter_id,
                member_vk: sk.verifying_key(),
            },
            inviter_sk,
        )
    }

    fn info(sk: &SigningKey, deputies: Vec<MemberId>) -> AuthorizedMemberInfo {
        let id: MemberId = sk.verifying_key().into();
        let mut mi = MemberInfo::new_public(id, 0, "n".to_string());
        mi.deputies = deputies;
        AuthorizedMemberInfo::new_with_member_key(mi, sk)
    }

    /// #411 round 4 D: the Ban-button gate uses `is_ban_authorized`, so a deputy
    /// sees Ban for a target in their deputizer's subtree — which the old bare
    /// downstream-ancestry gate (`is_downstream`) would have hidden.
    #[test]
    fn deputy_can_ban_but_unrelated_cannot() {
        let owner = SigningKey::generate(&mut OsRng);
        let d = SigningKey::generate(&mut OsRng); // owner's global-mod deputy
        let u = SigningKey::generate(&mut OsRng); // unrelated member
        let v = SigningKey::generate(&mut OsRng); // target (owner's invitee)
        let owner_id: MemberId = owner.verifying_key().into();
        let d_id: MemberId = d.verifying_key().into();
        let u_id: MemberId = u.verifying_key().into();
        let v_id: MemberId = v.verifying_key().into();

        let members = MembersV1 {
            members: vec![
                member(&d, owner_id, &owner, owner_id),
                member(&u, owner_id, &owner, owner_id),
                member(&v, owner_id, &owner, owner_id),
            ],
        };
        let member_info = MemberInfoV1 {
            member_info: vec![
                info(&owner, vec![d_id]), // owner deputizes D (global mod)
                info(&d, vec![]),
                info(&u, vec![]),
                info(&v, vec![]),
            ],
        };

        // Owner can ban anyone.
        assert!(viewer_can_ban(
            &members,
            &member_info,
            owner_id,
            v_id,
            owner_id
        ));
        // D (owner's deputy) can ban V even though D is NOT an ancestor of V, so
        // the old `is_downstream` gate would have hidden the Ban button.
        assert!(viewer_can_ban(&members, &member_info, d_id, v_id, owner_id));
        // An unrelated member cannot ban V.
        assert!(!viewer_can_ban(
            &members,
            &member_info,
            u_id,
            v_id,
            owner_id
        ));
        // Nobody can ban the owner.
        assert!(!viewer_can_ban(
            &members,
            &member_info,
            d_id,
            owner_id,
            owner_id
        ));
    }

    /// Source-grep pin for freenet/river#451: the modal's icon legend must
    /// render the 🛡 deputy chip, driven by the SAME shared helper the
    /// member-list row uses. The reported bug was that the row showed the
    /// shield but this modal did not; without this pin a future refactor could
    /// silently drop the chip again, or reintroduce a private, drifting copy of
    /// the viewer-relevance logic instead of the shared helper.
    #[test]
    fn modal_renders_deputy_shield_via_shared_helper() {
        let source = include_str!("member_info_modal.rs");
        let prod = &source[..source
            .find("#[cfg(test)]")
            .expect("member_info_modal.rs should have a #[cfg(test)] block")];

        assert!(
            prod.contains("🛡 Deputy"),
            "the member-info modal must render the 🛡 deputy legend chip (#451)"
        );
        assert!(
            prod.contains("relevant_deputizer_names"),
            "the deputy chip must be driven by the shared `relevant_deputizer_names` \
             helper so it cannot drift from the member-list row (#451)"
        );
    }
}
