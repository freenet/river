//! "Share an invite via DM…" picker (#252).
//!
//! Opened from the member-info modal of the *current* room when the local
//! user wants to invite that member to one of their OTHER rooms via a DM in
//! the current room. The picker lists every other room the local user is in;
//! clicking one:
//!
//! 1. generates an invitation for that room (mirrors
//!    `InviteMemberModal`'s flow — fresh invitee signing key, signs an
//!    `AuthorizedMember`, wraps in `Invitation`, encodes to base58 URL),
//! 2. drops a pre-composed DM body into [`super::DM_DRAFT`],
//! 3. opens the DM thread for the original peer in the current room
//!    via [`super::open_dm_thread`].
//!
//! The thread modal's body component drains `DM_DRAFT` when it first sees
//! a matching `(room, peer)` so the user lands on the composer with the
//! invite URL pre-populated and can review/edit before sending.
//!
//! Cross-room identity note: room members are keyed by per-room
//! `member_vk`, NOT by some user-global identity. So we cannot reliably
//! filter "rooms the target is already a member of" — we only know the
//! target's `MemberId` in the current room. We therefore list every other
//! room the local user is in; the local user is the one with context to
//! pick the right destination.
//!
//! In-flight state lives at module scope (`INVITE_VIA_DM_PICKER_INFLIGHT`,
//! defined in the parent module) rather than as a `use_signal` inside
//! the picker, because the watchdog task can outlive the picker's
//! unmount on the success path; reading a use_signal whose owning
//! component has been dropped panics in Dioxus.
//! A monotonic generation counter lets stale watchdogs from prior picks
//! short-circuit immediately when a newer pick has taken over.

use crate::components::app::{MEMBER_INFO_MODAL, ROOMS};
use crate::components::direct_messages::{
    open_dm_thread, InvitePickInflight, DM_DRAFT, INVITE_VIA_DM_PICKER,
    INVITE_VIA_DM_PICKER_INFLIGHT,
};
use crate::components::members::Invitation;
use crate::util::ecies::unseal_bytes_with_secrets;
use dioxus::logger::tracing::{error, info, warn};
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fa_solid_icons::FaLock, Icon};
use ed25519_dalek::{SigningKey, VerifyingKey};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId};
use river_core::room_state::privacy::PrivacyMode;
use std::sync::atomic::{AtomicU64, Ordering};

/// Per-render snapshot of a candidate room.
#[derive(Clone, PartialEq)]
struct CandidateRoom {
    room_vk: VerifyingKey,
    label: String,
    member_count: usize,
    is_private: bool,
}

/// Watchdog timeout for the spawn_local task. If we don't reach the
/// terminal `defer` block in this window, force-close the picker so the
/// user isn't stranded with a permanently-disabled UI. `sign_member_with_fallback`
/// itself caps at 10s before its local-signing fallback; this watchdog
/// is the catch-all for "something else got wedged."
const PICKER_WATCHDOG_SECS: u64 = 15;

/// Monotonic pick generation. Each row-click bumps this; watchdogs
/// capture the value at scheduling time and short-circuit if it has
/// moved on. Lives outside any component scope so it can never panic
/// on access (Codex P2 + Skeptical M1 / L3 on PR #260).
static PICK_GENERATION: AtomicU64 = AtomicU64::new(0);

#[component]
pub fn InviteViaDmPickerModal() -> Element {
    let active = *INVITE_VIA_DM_PICKER.read();
    let Some((current_room, target_peer)) = active else {
        return rsx! {};
    };

    let in_flight = *INVITE_VIA_DM_PICKER_INFLIGHT.read();
    let any_pending = in_flight.is_some();

    let close = move |_| {
        // Don't close while generation is in flight; the spawn_local
        // task is still running and would race the picker remount.
        // (Watchdog at PICKER_WATCHDOG_SECS will force-close on a
        // truly-stuck task.)
        if INVITE_VIA_DM_PICKER_INFLIGHT.read().is_some() {
            return;
        }
        crate::util::defer(move || {
            *INVITE_VIA_DM_PICKER.write() = None;
        });
    };

    // Resolve the target peer's nickname so the title can say
    // "Invite Bob to another room" instead of the generic "Share an
    // invite via DM" — gives the user immediate context for which
    // member they're acting on.
    let peer_label = use_memo(move || -> String {
        let rooms = ROOMS.try_read().ok();
        let nickname = rooms
            .as_ref()
            .and_then(|r| r.map.get(&current_room))
            .and_then(|room_data| {
                let secrets = &room_data.secrets;
                room_data
                    .room_state
                    .member_info
                    .member_info
                    .iter()
                    .find(|mi| mi.member_info.member_id == target_peer)
                    .map(|mi| {
                        match unseal_bytes_with_secrets(&mi.member_info.preferred_nickname, secrets)
                        {
                            Ok(b) => String::from_utf8_lossy(&b).to_string(),
                            Err(_) => mi.member_info.preferred_nickname.to_string_lossy(),
                        }
                    })
            });
        nickname.unwrap_or_else(|| target_peer.to_string().chars().take(8).collect())
    });

    // Build a sorted list of candidate rooms — every room the local user
    // has loaded that isn't the current one. Per the module doc, we
    // don't filter on "target is already a member" because per-room
    // identities make that check unreliable.
    let candidates = use_memo(move || -> Vec<CandidateRoom> {
        let Ok(rooms) = ROOMS.try_read() else {
            return Vec::new();
        };
        let mut out: Vec<CandidateRoom> = rooms
            .map
            .iter()
            .filter(|(owner_vk, _)| **owner_vk != current_room)
            .map(|(owner_vk, room_data)| {
                let sealed_name = &room_data
                    .room_state
                    .configuration
                    .configuration
                    .display
                    .name;
                let label = match unseal_bytes_with_secrets(sealed_name, &room_data.secrets) {
                    Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                    Err(_) => sealed_name.to_string_lossy(),
                };
                CandidateRoom {
                    room_vk: *owner_vk,
                    label,
                    // Owner is implicit, not in members.members — add 1
                    // for a useful display count.
                    member_count: room_data.room_state.members.members.len() + 1,
                    is_private: matches!(
                        room_data
                            .room_state
                            .configuration
                            .configuration
                            .privacy_mode,
                        PrivacyMode::Private
                    ),
                }
            })
            .collect();
        out.sort_by(|a, b| a.label.cmp(&b.label));
        out
    });

    let candidates_value = candidates.read().clone();
    let peer_label_value = peer_label.read().clone();
    let in_flight_room = in_flight.map(|p| p.room_vk);

    rsx! {
        div {
            class: "fixed inset-0 z-50 flex items-center justify-center",
            div {
                class: "absolute inset-0 bg-black/50",
                onclick: close,
            }
            div {
                class: "relative z-10 w-full max-w-md mx-4 bg-panel rounded-xl shadow-xl border border-border flex flex-col max-h-[80vh]",
                // Header — peer name inline so the user remembers who
                // they're inviting.
                div { class: "flex items-center justify-between px-5 py-4 border-b border-border",
                    h2 { class: "text-base font-semibold text-text",
                        "Invite "
                        span { class: "text-accent", "{peer_label_value}" }
                        " to another room"
                    }
                    button {
                        class: format!(
                            "p-1 text-text-muted hover:text-text transition-colors text-xl {}",
                            if any_pending { "opacity-40 cursor-not-allowed" } else { "" }
                        ),
                        disabled: any_pending,
                        "aria-label": "Close picker",
                        onclick: close,
                        "✕"
                    }
                }
                div { class: "flex-1 overflow-y-auto px-2 py-3",
                    if candidates_value.is_empty() {
                        p { class: "text-sm text-text-muted px-3",
                            "You aren't a member of any other rooms. Create or join one, then come back here."
                        }
                    } else {
                        p { class: "text-xs text-text-muted px-3 mb-2",
                            "Pick a room — River drafts a DM with the invite URL for you to edit before sending."
                        }
                        for room in candidates_value.iter() {
                            CandidateRow {
                                key: "{room.room_vk:x?}",
                                current_room: current_room,
                                target_peer: target_peer,
                                candidate: room.clone(),
                                in_flight_room: in_flight_room,
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn CandidateRow(
    current_room: VerifyingKey,
    target_peer: MemberId,
    candidate: CandidateRoom,
    in_flight_room: Option<VerifyingKey>,
) -> Element {
    let this_is_pending = in_flight_room == Some(candidate.room_vk);
    let any_pending = in_flight_room.is_some();

    let candidate_room = candidate.room_vk;
    let candidate_label = candidate.label.clone();
    let candidate_member_count = candidate.member_count;
    let candidate_is_private = candidate.is_private;

    let pick = {
        let label = candidate_label.clone();
        move |_| {
            // Guard against double-clicks via the global in-flight signal.
            if INVITE_VIA_DM_PICKER_INFLIGHT.peek().is_some() {
                return;
            }
            let label = label.clone();
            // Fetch the candidate-room's signing key and the local user's
            // membership claim from ROOMS at click time so we don't carry
            // a stale snapshot. Failure to read either is a logic error
            // we report via console — no point poisoning the picker for
            // every other candidate.
            let Some(candidate_data) = ROOMS
                .try_read()
                .ok()
                .and_then(|r| r.map.get(&candidate_room).cloned())
            else {
                error!("invite-via-DM: candidate room data missing");
                return;
            };
            let invitee_signing_key = SigningKey::generate(&mut rand::thread_rng());
            let invitee_vk = invitee_signing_key.verifying_key();
            let invited_by: MemberId = candidate_data.self_sk.verifying_key().into();
            let owner_id: MemberId = candidate_data.owner_vk.into();

            let member = Member {
                owner_member_id: owner_id,
                invited_by,
                member_vk: invitee_vk,
            };
            let room_key = candidate_data.room_key();
            let inviter_sk = candidate_data.self_sk.clone();
            let label_inner = label.clone();

            // Bump the generation counter for this pick. Watchdogs
            // captured before this point will no-op when they wake.
            let my_generation = PICK_GENERATION.fetch_add(1, Ordering::Relaxed) + 1;

            // Set the in-flight marker via defer to avoid mutating a
            // Dioxus signal directly from an onclick handler
            // (AGENTS.md "Dioxus WASM Signal Safety Rules"). The
            // re-render happens this same defer tick so the spinner is
            // visible before the awaited delegate call returns.
            crate::util::defer(move || {
                *INVITE_VIA_DM_PICKER_INFLIGHT.write() = Some(InvitePickInflight {
                    generation: my_generation,
                    room_vk: candidate_room,
                });
            });

            // `safe_spawn_local` to avoid the Firefox-mobile re-entrant
            // Task::run panic documented in AGENTS.md.
            crate::util::safe_spawn_local(async move {
                let outcome = drive_pick(
                    candidate_data,
                    member,
                    room_key,
                    inviter_sk,
                    invitee_signing_key,
                    label_inner,
                )
                .await;

                // Terminal cleanup. The in-flight clear is gated on
                // generation so it doesn't wipe a NEWER pick's marker
                // if our pick somehow took so long that the user
                // already started another one (defensive — the
                // double-click guard above should prevent this).
                let body_opt = outcome.ok();
                let success = body_opt.is_some();
                crate::util::defer(move || {
                    clear_inflight_if_matches(my_generation);
                    *INVITE_VIA_DM_PICKER.write() = None;
                    MEMBER_INFO_MODAL.with_mut(|m| m.member = None);
                    if let Some(body) = body_opt {
                        *DM_DRAFT.write() = Some((current_room, target_peer, body));
                    }
                });
                if success {
                    open_dm_thread(current_room, target_peer);
                    info!(
                        "invite-via-DM: drafted invite for room {:?}",
                        candidate_room
                    );
                }
            });

            schedule_watchdog(my_generation);
        }
    };

    let label_for_a11y = candidate_label.clone();
    let aria = format!("Pick room {} as the invite destination", label_for_a11y);
    rsx! {
        button {
            class: format!(
                "w-full text-left px-3 py-2 rounded-lg text-sm text-text flex items-center gap-2 transition-colors {}",
                if any_pending {
                    "opacity-60 cursor-not-allowed"
                } else {
                    "hover:bg-surface"
                }
            ),
            disabled: any_pending,
            "aria-label": "{aria}",
            onclick: pick,
            if candidate_is_private {
                span {
                    class: "flex-shrink-0 text-text-muted",
                    title: "Private room (members-only, end-to-end encrypted)",
                    "aria-label": "Private room",
                    Icon { width: 12, height: 12, icon: FaLock }
                }
            }
            div { class: "flex-1 min-w-0 truncate", "{candidate_label}" }
            {
                let label = if candidate_member_count == 1 {
                    format!("{} member", candidate_member_count)
                } else {
                    format!("{} members", candidate_member_count)
                };
                rsx! {
                    span { class: "text-[10px] text-text-muted flex-shrink-0",
                        "{label}"
                    }
                }
            }
            if this_is_pending {
                div { class: "animate-spin w-3 h-3 border-2 border-text-muted border-t-transparent rounded-full flex-shrink-0" }
                span { class: "text-[10px] text-text-muted", "Generating…" }
            }
        }
    }
}

/// Clear `INVITE_VIA_DM_PICKER_INFLIGHT` only if it still names this
/// pick's generation. Prevents a stale terminal-defer from wiping a
/// newer pick's marker (defensive — same generation-gating used by
/// the watchdog).
fn clear_inflight_if_matches(my_generation: u64) {
    let still_mine = matches!(
        *INVITE_VIA_DM_PICKER_INFLIGHT.peek(),
        Some(p) if p.generation == my_generation
    );
    if still_mine {
        *INVITE_VIA_DM_PICKER_INFLIGHT.write() = None;
    }
}

/// Run the actual signing + URL composition. Pulled out as `async fn` so
/// the row's click handler stays readable.
async fn drive_pick(
    candidate_data: crate::room_data::RoomData,
    member: Member,
    room_key: river_core::chat_delegate::RoomKey,
    inviter_sk: SigningKey,
    invitee_signing_key: SigningKey,
    label: String,
) -> Result<String, &'static str> {
    let mut member_bytes = Vec::new();
    if ciborium::ser::into_writer(&member, &mut member_bytes).is_err() {
        warn!("invite-via-DM: failed to serialize member");
        return Err("serialize-member-failed");
    }
    // Sign using the delegate-backed signing path with local fallback.
    // The delegate is the source of truth in case the local self_sk is
    // stale after a sibling-device identity-import migration. Up to a
    // 10s wait before fallback; the row spinner covers that window.
    let signature =
        crate::signing::sign_member_with_fallback(room_key, member_bytes, &inviter_sk).await;
    let authorized = AuthorizedMember::with_signature(member, signature);
    let invitation = Invitation {
        room: candidate_data.owner_vk,
        invitee_signing_key,
        invitee: authorized,
    };

    let invite_code = invitation.to_encoded_string();
    let base_url = crate::components::members::invite_member_modal::get_invitation_base_url();
    let invite_url = format!("{}?invitation={}", base_url, invite_code);

    Ok(format!(
        "You're invited to join \"{}\" on River. Click to join:\n\n{}",
        label, invite_url
    ))
}

/// Schedule a one-shot watchdog that clears `INVITE_VIA_DM_PICKER_INFLIGHT`
/// and tears down the picker if `my_generation` is still in flight after
/// `PICKER_WATCHDOG_SECS`. Belt-and-suspenders against a stuck spawn_local
/// task (Skeptical-review M1 on PR #260).
///
/// Generation-keyed: a watchdog scheduled for an earlier pick wakes,
/// observes that the current generation has moved on, and no-ops. This
/// is correct even after the picker has unmounted, because the global
/// signal lives at module scope (not in any component's `use_signal`).
fn schedule_watchdog(my_generation: u64) {
    use std::time::Duration;
    crate::util::safe_spawn_local(async move {
        crate::util::sleep(Duration::from_secs(PICKER_WATCHDOG_SECS)).await;
        // Move the GlobalSignal read INSIDE the defer block so it
        // happens under the Dioxus runtime scope. `safe_spawn_local`
        // only defers spawning; it does NOT push the runtime, so a
        // raw `peek()` here would panic via `Runtime::current()`
        // when the watchdog wakes — even on successful picks that
        // already cleared the state (Codex P2 on PR #260).
        //
        // Reading INFLIGHT inside the defer also fixes the
        // false-alarm warn the previous version emitted when the
        // terminal defer landed in the 0.1s window before the
        // watchdog's defer fired: we now re-check the generation
        // and early-return if the pick has already terminated
        // (Skeptical M1).
        crate::util::defer(move || {
            let still_mine = matches!(
                *INVITE_VIA_DM_PICKER_INFLIGHT.peek(),
                Some(p) if p.generation == my_generation
            );
            if !still_mine {
                return;
            }
            warn!(
                "invite-via-DM: watchdog fired after {}s; force-closing picker",
                PICKER_WATCHDOG_SECS
            );
            clear_inflight_if_matches(my_generation);
            *INVITE_VIA_DM_PICKER.write() = None;
        });
    });
}
