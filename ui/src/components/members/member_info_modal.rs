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
use crate::components::members::{ban_gate, BanGate};
use crate::util::display_name::display_nickname;
use dioxus::logger::tracing::*;
use dioxus::prelude::*;
use river_core::room_state::member::MemberId;
use river_core::room_state::ChatRoomParametersV1;

#[component]
pub fn MemberInfoModal() -> Element {
    // Memos
    // Both memos below read ROOMS fallibly, and this modal is mounted for the
    // whole app session (app.rs renders it unconditionally), so a contended pass
    // that dropped their ROOMS subscription would strand them for good. The
    // anchor keeps a dependency the nudge can wake. See
    // `crate::util::signal_guard` and freenet/river#555.
    // Read CURRENT_ROOM (infallible) BEFORE the fallible ROOMS read in both memos
    // below. That ordering is load-bearing on its own -- it leaves a backup
    // subscription if the anchor/nudge channel is ever broken -- and the anchor
    // then covers the case the ordering cannot: a contended pass still drops the
    // ROOMS subscription, so without a nudge the memo waits for an unrelated
    // CURRENT_ROOM change. See `crate::util::signal_guard` and freenet/river#555.
    let current_room_data_signal = use_memo(move || {
        crate::util::signal_guard::anchor();
        let key = CURRENT_ROOM.read().owner_key?;
        let Ok(rooms) = ROOMS.try_read() else {
            crate::util::signal_guard::schedule_nudge();
            return None;
        };
        rooms.map.get(&key).cloned()
    });
    let self_member_id: Memo<Option<MemberId>> = use_memo(move || {
        // This was the worse of the two before the fix: `ROOMS.try_read().ok()?`
        // came FIRST, so a contended pass returned before CURRENT_ROOM was read
        // and left the memo with ZERO subscriptions, in a modal that never
        // unmounts.
        crate::util::signal_guard::anchor();
        let key = CURRENT_ROOM.read().owner_key?;
        let Ok(rooms) = ROOMS.try_read() else {
            crate::util::signal_guard::schedule_nudge();
            return None;
        };
        rooms
            .map
            .get(&key)
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

        // Ban authority (#410 / #411 round 4 D / #478). The Ban button is gated on
        // REAL ban authority — owner / invite-ancestor / deputy — via
        // `is_ban_authorized`, NOT bare downstream ancestry (`is_downstream`), so a
        // deputy sees Ban for members in their deputizer's subtree. `is_downstream`
        // is still used above for the "🔑 Invited by You" relationship tag, which
        // is a different meaning and must not change.
        //
        // `ban_gate` folds in the "you may not ban yourself out of the room" rule
        // and distinguishes its two negative answers, so the ONE case where the
        // action is taken away from someone who would otherwise have had it —
        // banning yourself, or banning an ancestor whose cascade sweeps you up —
        // gets a visible explanation instead of a mysteriously absent button.
        let gate = owner_key_signal
            .as_ref()
            .map(|owner| {
                ban_gate(
                    &room_state.room_state.members,
                    &room_state.room_state.member_info,
                    self_member_id,
                    member_id,
                    MemberId::from(&*owner),
                )
            })
            .unwrap_or(BanGate::NoAuthority);
        let can_ban = gate == BanGate::Allowed;
        let ban_refusal = match gate {
            BanGate::WouldRemoveViewer(reason) => Some(reason),
            BanGate::Allowed | BanGate::NoAuthority => None,
        };

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
                        display_nickname(&mi.member_info.preferred_nickname, &room_state.secrets)
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

        // The 🛡 legend chip below (freenet/river#451). Computed with the SAME
        // shared helper the member-list row and the conversation's author line
        // use, so the modal shows the shield under exactly the same condition,
        // and with the same tooltip, as they do. `None` means no shield here.
        //
        // Single-target variant: this component is not memoised, and the sweep
        // decrypts every deputy's appointer nicknames.
        let deputy_badge: Option<super::DeputyBadge> =
            owner_key_signal.as_ref().and_then(|owner| {
                super::deputy_badge_for_viewer(
                    &room_state.room_state.members,
                    &room_state.room_state.member_info,
                    &room_state.secrets,
                    MemberId::from(&*owner),
                    self_member_id,
                    member_id,
                )
            });
        let deputy_tooltip = deputy_badge
            .as_ref()
            .map(super::DeputyBadge::tooltip)
            .unwrap_or_default();
        // Decrypted display nickname for the target (for the deputy action copy).
        let target_nickname = display_nickname(
            &member_info.member_info.preferred_nickname,
            &room_state.secrets,
        );

        // The ⚠ impersonation warning (freenet/river#489), through the SAME
        // entry point the member-list row and the conversation's author line
        // use, so this surface cannot show a badge the others do not — the
        // cross-surface drift #451 fixed for the shield.
        //
        // Fed `target_nickname`, which is the text this modal actually renders.
        // Checking the raw sealed nickname would compare a string nobody sees.
        //
        // ## Why this pays for the whole badge map
        //
        // `impersonation_checker_for_viewer` takes the badge map, so this builds
        // the map `deputy_badge_for_viewer` above deliberately avoids: in a
        // private room that is an ECIES unseal per appointer. It is worth it
        // here, because the alternative is not "a cheaper warning" but "no
        // explanation at all on a phone". A `title=` tooltip NEVER FIRES ON
        // TOUCH, so on mobile the ⚠ beside a name is a bare glyph, and the
        // reader's natural next move — tapping the name, which opens THIS modal
        // — used to explain nothing. This modal is opened by a deliberate click
        // and renders one member; it is not a per-keystroke path.
        //
        // The shield above still uses the single-target helper: it is pinned by
        // `modal_renders_deputy_shield_via_shared_helper` and by two Playwright
        // specs, and re-deriving it from this map would put that behaviour on a
        // different code path for no gain.
        let impersonation = owner_key_signal.as_ref().and_then(|owner| {
            let owner_id = MemberId::from(&*owner);
            let deputy_badges = super::deputy_badges_for_viewer(
                &room_state.room_state.members,
                &room_state.room_state.member_info,
                &room_state.secrets,
                owner_id,
                self_member_id,
            );
            let checker = super::impersonation_checker_for_viewer(
                &room_state.room_state.member_info,
                &room_state.secrets,
                owner_id,
                &deputy_badges,
            );
            // BOTH ids below are `member_id`, the member whose modal this is —
            // never `self_member_id`. The 4th argument never suppresses the
            // badge; it picks which of the two true sentences the tooltip
            // states (see `ImpersonationWarning`), so passing the VIEWER's
            // privilege there would tell a moderator looking at an impostor
            // "Name conflict … Both are real", exonerating the impostor to
            // exactly the people who would act on it. The argument list is
            // kept contiguous, with no comment inside it, so the pin in
            // `modal_passes_the_target_id_and_the_targets_own_privilege` can
            // match it whitespace-insensitively.
            super::impersonation_warning_for_display(
                &checker,
                member_id,
                &target_nickname,
                super::privilege_in_view(member_id, owner_id, &deputy_badges),
            )
        });
        let impersonation_tooltip = impersonation
            .as_ref()
            .map(crate::util::confusable::ImpersonationWarning::tooltip)
            .unwrap_or_default();
        // The chip's label is the tooltip's OWN leading clause (everything
        // before the first colon: "Impersonation warning" or "Name conflict").
        // Deriving it means this surface cannot invent wording that drifts from
        // the single definition in `ImpersonationWarning::tooltip`, and it
        // follows the tooltip's privileged/unprivileged branch for free. Safe to
        // split on: `tooltip_contains_no_nickname_content` pins that no nickname
        // reaches this string, so the colon is always ours.
        let impersonation_label = impersonation_tooltip
            .split(':')
            .next()
            .unwrap_or_default()
            .to_string();

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
                            // same tooltip. The tooltip counts the appointers
                            // rather than naming them; the names themselves are
                            // listed below.
                            if deputy_badge.is_some() {
                                span {
                                    "data-testid": "member-info-deputy-tag",
                                    class: "inline-flex items-center px-2.5 py-0.5 rounded-full text-sm font-medium bg-purple-500/20 text-purple-400",
                                    title: "{deputy_tooltip}",
                                    "aria-label": "{deputy_tooltip}",
                                    "🛡 Deputy"
                                }
                            }
                            // The ⚠ impersonation warning (#489). Deliberately
                            // NOT mutually exclusive with the 🛡 above: two
                            // deputies whose names collide each carry a real
                            // shield AND a real warning, and suppressing the
                            // warning for a badged member would hand a
                            // deputised sockpuppet exactly the immunity the
                            // per-name exemption exists to deny. The tooltip,
                            // not the badge, is what changes in that case.
                            if impersonation.is_some() {
                                span {
                                    "data-testid": "member-info-impersonation-tag",
                                    class: "inline-flex items-center px-2.5 py-0.5 rounded-full text-sm font-medium bg-amber-500/20 text-amber-400",
                                    title: "{impersonation_tooltip}",
                                    "aria-label": "{impersonation_tooltip}",
                                    "{crate::util::confusable::WARNING_GLYPH} {impersonation_label}"
                                }
                            }
                        }

                        // The warning, SPELLED OUT. This is the reason the modal
                        // was wired up at all: `title=` never fires on touch, so
                        // on a phone every other surface can only show the bare
                        // ⚠ glyph. Rendering the same sentence as visible text
                        // makes this the one place a phone user can find out
                        // what the badge means.
                        //
                        // The imitated NAME is shown here and nowhere else, as
                        // its own element. That is safe for the same reason
                        // `appointer_names` is: a nickname is attacker-chosen
                        // and a comma inside it (`Bob, the room owner, Carol`)
                        // forges structure in a flat string, but cannot span two
                        // DOM nodes. Do NOT join it into the sentence above, and
                        // do NOT move it into the tooltip — see
                        // `ImpersonationWarning::tooltip`.
                        if let Some(warning) = impersonation.as_ref() {
                            div {
                                "data-testid": "member-info-impersonation-explanation",
                                class: "mb-4 p-3 rounded-lg bg-amber-500/10 border border-amber-500/20 text-sm text-amber-200",
                                p { class: "mb-1", "{impersonation_tooltip}" }
                                div {
                                    span { class: "mr-1 text-text-muted", "Resembles:" }
                                    // `max-w-full break-words`: this is an
                                    // attacker-chosen nickname, so it can be
                                    // long and contain no spaces at all.
                                    // Without them it overflows the modal's
                                    // `max-w-md` at 320px and pushes the panel
                                    // wider than the viewport.
                                    span {
                                        "data-testid": "member-info-impersonated-name",
                                        class: "inline-block max-w-full break-words px-2 py-0.5 rounded bg-surface border border-border text-text",
                                        "{warning.impersonated.display_name}"
                                    }
                                }
                            }
                        }

                        // The appointers, by name. This is the ONE surface that
                        // shows them, and it renders each as its own element on
                        // purpose: a `title=` attribute is a flat string, so a
                        // nickname of `Bob, the room owner, Carol` inside one
                        // reads as three appointers including a role label
                        // River never granted. One node per appointer makes the
                        // boundaries real rather than punctuation, so a comma
                        // inside a name cannot span two of them.
                        //
                        // Do NOT collapse this into a joined string, and do NOT
                        // move these names into the tooltip above. See
                        // `DeputyBadge::appointer_phrase`.
                        if let Some(badge) = deputy_badge.as_ref() {
                            div {
                                "data-testid": "member-info-deputy-appointers",
                                class: "mb-4 text-sm text-text-muted",
                                span { class: "mr-1", "Deputized by:" }
                                for name in badge.appointer_names() {
                                    span {
                                        class: "inline-block px-2 py-0.5 mr-1 mb-1 rounded bg-surface border border-border text-text",
                                        "{name}"
                                    }
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
                            // BOTH actions are gated on the target not being
                            // yourself. Ban used to sit OUTSIDE this guard, so a
                            // deputy opening their own profile saw an enabled
                            // "Ban User" — and the resulting self-ban is
                            // contract-VALID and cascades to their whole invite
                            // subtree (freenet/river#478).
                            if member_id != self_member_id {
                                div { class: "mt-4 flex items-start gap-3",
                                    // #478, transitive case: the Ban action is
                                    // ALSO withheld when the cascade would sweep
                                    // the viewer up — i.e. the target is one of
                                    // the viewer's own invite ancestors. Same
                                    // damage as a self-ban, different route, so
                                    // it is the same rule (`ban_gate`), not a
                                    // second special case. `ban_refusal` is
                                    // `Some` only when the viewer would OTHERWISE
                                    // have had the action, which is why it also
                                    // carries the text: a moderator whose Ban
                                    // button vanished is owed a reason.
                                    if ban_refusal.is_none() {
                                        BanButton {
                                            member_to_ban: member_id,
                                            can_ban: can_ban,
                                            // The DECRYPTED, sanitised name — the same
                                            // value `DeputyButton` gets. This used to
                                            // pass the raw `SealedBytes`, which Dioxus
                                            // silently coerced through `Display` (i.e.
                                            // `to_string_lossy`): the ban dialog showed
                                            // an unsanitised nickname in the one place
                                            // a moderator is judging authority, and
                                            // showed "[Encrypted: N bytes, vN]" instead
                                            // of a name in private rooms.
                                            nickname: target_nickname.clone()
                                        }
                                    }
                                    if let Some(reason) = ban_refusal {
                                        div {
                                            "data-testid": "ban-withheld-reason",
                                            class: "flex-1 px-3 py-2 text-sm text-text-muted bg-surface border border-border rounded-lg",
                                            "{reason}"
                                        }
                                    }

                                    // Deputize / revoke-deputy (#410). Any non-owner
                                    // member (except self) may be deputized; the action
                                    // hides itself when the viewer lacks authority.
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
    use crate::components::members::{ban_gate, BanGate};
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
        assert_eq!(
            ban_gate(&members, &member_info, owner_id, v_id, owner_id),
            BanGate::Allowed
        );
        // D (owner's deputy) can ban V even though D is NOT an ancestor of V, so
        // the old `is_downstream` gate would have hidden the Ban button.
        assert_eq!(
            ban_gate(&members, &member_info, d_id, v_id, owner_id),
            BanGate::Allowed
        );
        // An unrelated member cannot ban V.
        assert_eq!(
            ban_gate(&members, &member_info, u_id, v_id, owner_id),
            BanGate::NoAuthority
        );
        // Nobody can ban the owner.
        assert_eq!(
            ban_gate(&members, &member_info, d_id, owner_id, owner_id),
            BanGate::NoAuthority
        );
    }

    /// Source-grep pin for freenet/river#451: the modal's icon legend must
    /// render the 🛡 deputy chip, driven by the SAME shared helper the
    /// member-list row and the conversation's author line use. The reported bug
    /// was that the row showed the shield but this modal did not; without this
    /// pin a future refactor could silently drop the chip again, or reintroduce
    /// a private, drifting copy of the viewer-relevance logic.
    ///
    /// The pin now covers the TOOLTIP as well as visibility: all three surfaces
    /// must go through `DeputyBadge::tooltip`, so the shield cannot say
    /// "appointed by X — can ban you" in the conversation and something else
    /// here.
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
            prod.contains("deputy_badge_for_viewer"),
            "the deputy chip's visibility must come from the shared \
             `deputy_badge_for_viewer` helper so it cannot drift from the \
             member-list row or the conversation author line (#451)"
        );
        assert!(
            prod.contains("DeputyBadge::tooltip"),
            "the deputy chip's tooltip must come from `DeputyBadge::tooltip` \
             so the wording cannot drift between surfaces (#451)"
        );
    }

    /// This modal is the only surface that shows appointer NAMES, and it must
    /// render each as its own element.
    ///
    /// The tooltip deliberately names nobody, because a `title=` attribute is a
    /// flat string in which a nickname of `Bob, the room owner, Carol` reads as
    /// three appointers including a role label River never granted. Joining
    /// these names — here or anywhere — rebuilds that exact hole with different
    /// punctuation, so the shape is pinned rather than left to a comment.
    #[test]
    fn modal_lists_appointer_names_as_separate_elements() {
        let source = include_str!("member_info_modal.rs");
        let prod = &source[..source
            .find("#[cfg(test)]")
            .expect("member_info_modal.rs should have a #[cfg(test)] block")];

        assert!(
            prod.contains("for name in badge.appointer_names()"),
            "the appointer names must be iterated into one element each; a \
             single interpolated string reintroduces the role-label forgery"
        );
        for joiner in [
            "appointer_names().join",
            "appointer_names().concat",
            "deputized_by.join",
        ] {
            assert!(
                !prod.contains(joiner),
                "`{joiner}` flattens attacker-controlled nicknames into one \
                 string, which is the forgery this shape exists to prevent"
            );
        }
    }

    /// freenet/river#489: this modal must render the ⚠ warning, and must render
    /// its sentence as VISIBLE TEXT.
    ///
    /// The visible-text half is the entire reason the modal was wired up, and a
    /// future refactor that "tidied" the explanation into the `title=` alone
    /// would silently undo it while still looking correct on a desktop. A
    /// `title=` tooltip NEVER FIRES ON TOUCH, so on a phone every other surface
    /// can show only a bare glyph, and the reader's natural next move — tapping
    /// the name, which opens this modal — has to be what explains it.
    #[test]
    fn modal_renders_the_impersonation_warning_as_visible_text() {
        let source = include_str!("member_info_modal.rs");
        let prod = &source[..source
            .find("#[cfg(test)]")
            .expect("member_info_modal.rs should have a #[cfg(test)] block")];

        assert!(
            prod.contains("impersonation_warning_for_display"),
            "the ⚠ must come from the shared `impersonation_warning_for_display` \
             entry point, which carries the tier decision — a surface that \
             called the checker directly would render a badge the member list \
             and the conversation do not (#451's cross-surface drift)"
        );
        for bypass in ["check_identical(", "ImpersonationChecker::check"] {
            assert!(
                !prod.contains(bypass),
                "`{bypass}` skips the tier decision in \
                 `impersonation_warning_for_display`"
            );
        }

        // Scoped between anchors rather than by a byte count: the first version
        // took a fixed 900-character window, and adding a comment inside the
        // block silently pushed the element it checks out of range. A pin whose
        // reach depends on how much prose sits above it is not a pin.
        let explanation = {
            let start = prod
                .find("member-info-impersonation-explanation")
                .expect("the modal must render an explanation block for the ⚠");
            let end = prod[start..]
                .find("NicknameField")
                .expect("the explanation block is followed by the nickname field")
                + start;
            &prod[start..end]
        };
        assert!(
            explanation.contains("{impersonation_tooltip}"),
            "the warning's SENTENCE must be rendered as visible text, not only \
             as a `title=` attribute: `title=` does not fire on touch, so a \
             phone user would get a bare glyph and no way to find out what it \
             means"
        );
        assert!(
            explanation.contains("member-info-impersonated-name"),
            "the imitated name must be its own element, so a comma inside an \
             attacker-chosen nickname cannot forge structure the way it did in \
             #488's tooltip"
        );

        // The name must never be folded INTO the sentence — the same rule
        // `ImpersonationWarning::tooltip` states and `appointer_names` follows.
        for joiner in [
            "impersonated.display_name.join",
            "{impersonation_tooltip} {warning.impersonated.display_name}",
        ] {
            assert!(
                !prod.contains(joiner),
                "`{joiner}` joins an attacker-chosen nickname into our sentence"
            );
        }
    }

    /// **The ARGUMENTS, not just the call.** The member list and the
    /// conversation author line each pin theirs; this is the third surface and
    /// was pinned by nothing.
    ///
    /// Two swaps compile, pass every other test in this file, and are invisible
    /// in the example-data fixture — where the impostor and the viewer both
    /// have `privilege_in_view` of `None`, so nothing observable changes:
    ///
    /// * `member_id` -> `self_member_id` at the 2nd argument puts the ⚠ on the
    ///   genuine owner and every genuine moderator.
    /// * `member_id` -> `self_member_id` INSIDE `privilege_in_view` at the 4th
    ///   is the subtler one and matters most in the room this feature exists
    ///   for. A viewer who is themselves badged — every moderator in Freenet
    ///   Official — would open an impostor's modal and be shown the
    ///   PRIVILEGED-clash sentence: "Name conflict … Both are real; check the
    ///   member ID". That is the wording for a legitimate collision, displayed
    ///   for a genuine impersonation, exonerating the impostor to precisely the
    ///   people who would otherwise ban them.
    ///
    /// Whitespace-insensitive so `cargo fmt` may rewrap the call, but still
    /// failing if an ARGUMENT changes.
    #[test]
    fn modal_passes_the_target_id_and_the_targets_own_privilege() {
        let source = include_str!("member_info_modal.rs");
        let prod = &source[..source
            .find("#[cfg(test)]")
            .expect("member_info_modal.rs should have a #[cfg(test)] block")];

        // Scope to the one computation, not the whole file: this file also
        // contains the comment describing the call, and a whole-file scrape
        // would match that instead.
        let start = prod
            .find("let impersonation = owner_key_signal")
            .expect("the modal must compute an impersonation warning");
        let end = prod[start..]
            .find("let impersonation_tooltip")
            .expect("the warning computation is followed by its tooltip")
            + start;
        let call = &prod[start..end];

        let squashed = call.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            squashed.contains(
                "impersonation_warning_for_display( &checker, member_id, &target_nickname, \
                 super::privilege_in_view(member_id, owner_id, &deputy_badges), )"
            ),
            "the member-info modal no longer passes `member_id` — the member \
             whose modal this is — as BOTH the flagged id and the subject of \
             `privilege_in_view`. Passing `self_member_id` at the 2nd argument \
             brands the genuine owner and every genuine moderator; passing it \
             at the 4th shows a badged viewer the \"Name conflict … Both are \
             real\" sentence for a real impostor. Neither is visible in the \
             example-data fixture, which is why this pin exists.\ngot: {squashed}"
        );
    }
}
