//! "Share an invite via DM…" picker (#252, redesigned for structured
//! invite-DM variant).
//!
//! Opened from the member-info modal of the *current* room when the local
//! user wants to invite that member to one of their OTHER rooms via a DM in
//! the current room. The picker is now the **composer**: a room dropdown
//! plus an optional "personal message" textarea plus a Send button. On
//! Send, it dispatches a structured
//! [`river_core::room_state::dm_body::DirectMessageBody::Invite`] DM
//! directly via [`crate::components::direct_messages::send_structured_dm`]
//! — no URL pasted into the composer, no `DM_DRAFT` indirection, no DM
//! thread modal opened in the middle of the flow.
//!
//! The recipient renders the structured Invite variant as an in-thread
//! "Invitation card" with an Accept button. The Accept button calls the
//! same [`crate::components::room_list::receive_invitation_modal::present_invitation`]
//! entry point the URL-bar accept flow uses, so there's exactly one
//! invitation-accept code path. See `dm_thread_modal.rs` for the
//! recipient side.
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
    send_structured_dm, InvitePickInflight, SendDmOutcome, INVITE_VIA_DM_PICKER,
    INVITE_VIA_DM_PICKER_INFLIGHT,
};
use crate::components::members::Invitation;
use crate::util::ecies::unseal_bytes_with_secrets;
use dioxus::logger::tracing::{error, info, warn};
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fa_solid_icons::FaLock, Icon};
use ed25519_dalek::{SigningKey, VerifyingKey};
use river_core::room_state::dm_body::{DirectMessageBody, InvitePayload};
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

/// Cap on the personal-message field. Generous because the underlying
/// DM body cap is 32 KiB minus crypto + CBOR overhead; this is a UX cap
/// to prevent a runaway paste, not a wire-format cap. Mirrors what the
/// chat composer enforces.
const PERSONAL_MESSAGE_CHAR_CAP: usize = 4_000;

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

    // Selected target room (the room we're generating an invite TO) and
    // optional personal message. `use_signal` keeps both local to this
    // mount — when the picker closes both are dropped, so re-opening
    // starts fresh.
    let mut selected_room: Signal<Option<VerifyingKey>> = use_signal(|| None);
    let mut personal_message = use_signal(String::new);
    let mut send_error: Signal<Option<String>> = use_signal(|| None);
    let mut last_success_label: Signal<Option<String>> = use_signal(|| None);

    let close = move |_| {
        // Don't close while a send is in flight.
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
    let selected_room_value = *selected_room.read();
    let personal_message_value = personal_message.read().clone();
    let send_error_value = send_error.read().clone();
    let last_success_label_value = last_success_label.read().clone();
    let pmessage_chars = personal_message_value.chars().count();
    let can_send = selected_room_value.is_some() && !any_pending;

    // Clone the candidates for the do_send closure; the rsx! body
    // also needs to iterate over candidates_value, so each consumer
    // gets its own clone (Vec<CandidateRoom> isn't Copy).
    let candidates_for_send = candidates_value.clone();
    // Issue Send: generates an invite for the selected room, then sends a
    // structured DM containing it. On success the picker closes and the
    // member-info modal closes too. On failure we surface an inline
    // error and let the user retry.
    let do_send = move |_| {
        // Re-check pending; click can race the disabled-attribute on the button.
        if INVITE_VIA_DM_PICKER_INFLIGHT.peek().is_some() {
            return;
        }
        let Some(candidate_room_vk) = *selected_room.peek() else {
            send_error.set(Some("Pick a room to invite them to first.".into()));
            return;
        };
        send_error.set(None);

        let pmessage = personal_message.peek().clone();
        let pmessage_opt = if pmessage.trim().is_empty() {
            None
        } else {
            Some(pmessage.trim().to_string())
        };

        // Snapshot the room data and label for the success toast.
        let Some(candidate_data) = ROOMS
            .try_read()
            .ok()
            .and_then(|r| r.map.get(&candidate_room_vk).cloned())
        else {
            error!("invite-via-DM: candidate room data missing");
            send_error.set(Some(
                "The room you picked is no longer loaded. Try again.".into(),
            ));
            return;
        };
        let candidate_label = candidates_for_send
            .iter()
            .find(|c| c.room_vk == candidate_room_vk)
            .map(|c| c.label.clone())
            .unwrap_or_else(|| "Unknown room".to_string());

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

        let my_generation = PICK_GENERATION.fetch_add(1, Ordering::Relaxed) + 1;

        crate::util::defer(move || {
            *INVITE_VIA_DM_PICKER_INFLIGHT.write() = Some(InvitePickInflight {
                generation: my_generation,
                room_vk: candidate_room_vk,
            });
        });

        let candidate_label_for_task = candidate_label.clone();
        crate::util::safe_spawn_local(async move {
            let outcome = drive_send(
                current_room,
                target_peer,
                candidate_data,
                member,
                room_key,
                inviter_sk,
                invitee_signing_key,
                pmessage_opt,
            )
            .await;

            crate::util::defer(move || {
                clear_inflight_if_matches(my_generation);
                match outcome {
                    Ok(()) => {
                        info!(
                            "invite-via-DM: sent invite for room {:?}",
                            candidate_room_vk
                        );
                        last_success_label.set(Some(candidate_label_for_task.clone()));
                        // Close the picker and the parent member-info modal —
                        // the user is done with this flow.
                        *INVITE_VIA_DM_PICKER.write() = None;
                        MEMBER_INFO_MODAL.with_mut(|m| m.member = None);
                    }
                    Err(e) => {
                        warn!("invite-via-DM: send failed: {}", e);
                        send_error.set(Some(e));
                    }
                }
            });
        });

        schedule_watchdog(my_generation);
    };

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
                div { class: "flex-1 overflow-y-auto px-5 py-4 space-y-3",
                    if candidates_value.is_empty() {
                        p { class: "text-sm text-text-muted",
                            "You aren't a member of any other rooms. Create or join one, then come back here."
                        }
                    } else {
                        p { class: "text-xs text-text-muted",
                            "Send an invitation card to "
                            span { class: "text-text", "{peer_label_value}" }
                            ". They'll see an Accept button right inside the DM thread — no link to copy."
                        }
                        // Room selection — clicking a candidate row selects
                        // it (radio-style). Sorted alphabetical; private
                        // rooms get a lock icon.
                        div { class: "space-y-1",
                            label { class: "text-xs text-text-muted block",
                                "Which room?"
                            }
                            for room in candidates_value.iter() {
                                CandidateRow {
                                    key: "{room.room_vk:x?}",
                                    candidate: room.clone(),
                                    is_selected: selected_room_value == Some(room.room_vk),
                                    any_pending,
                                    on_select: {
                                        let r = room.room_vk;
                                        move |_| {
                                            selected_room.set(Some(r));
                                        }
                                    },
                                }
                            }
                        }
                        // Optional personal message.
                        div { class: "space-y-1",
                            label { class: "text-xs text-text-muted block",
                                "Add a personal message (optional)"
                            }
                            textarea {
                                class: "w-full px-3 py-2 bg-surface border border-border rounded-lg text-sm text-text resize-none min-h-[3rem] max-h-32",
                                placeholder: "e.g. \"Thought you'd enjoy this room\"",
                                value: "{personal_message_value}",
                                disabled: any_pending,
                                oninput: move |e| {
                                    let v = e.value();
                                    // Soft-cap: trim rather than reject so paste-of-bigger-text
                                    // still leaves something the user can edit.
                                    let trimmed: String = if v.chars().count() > PERSONAL_MESSAGE_CHAR_CAP {
                                        v.chars().take(PERSONAL_MESSAGE_CHAR_CAP).collect()
                                    } else {
                                        v
                                    };
                                    personal_message.set(trimmed);
                                },
                            }
                            div { class: "flex justify-end",
                                span { class: "text-[10px] text-text-muted",
                                    "{pmessage_chars}/{PERSONAL_MESSAGE_CHAR_CAP}"
                                }
                            }
                        }
                    }
                    if let Some(err) = send_error_value.as_ref() {
                        div { class: "text-xs text-red-400", "{err}" }
                    }
                    if let Some(label) = last_success_label_value.as_ref() {
                        div { class: "text-xs text-emerald-400",
                            "Invitation to \""
                            span { class: "font-medium", "{label}" }
                            "\" sent."
                        }
                    }
                }
                // Footer: Send button only enabled once a room is picked
                // and no send is in flight.
                if !candidates_value.is_empty() {
                    div { class: "border-t border-border px-5 py-3 flex items-center justify-between",
                        if any_pending {
                            div { class: "flex items-center gap-2 text-xs text-text-muted",
                                div { class: "animate-spin w-3 h-3 border-2 border-text-muted border-t-transparent rounded-full" }
                                "Sending invite…"
                            }
                        } else {
                            span { class: "text-[10px] text-text-muted" }
                        }
                        button {
                            class: "px-4 py-2 bg-accent hover:bg-accent-hover disabled:opacity-50 disabled:cursor-not-allowed text-white text-sm font-medium rounded-lg transition-colors",
                            disabled: !can_send,
                            onclick: do_send,
                            "Send invite"
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn CandidateRow(
    candidate: CandidateRoom,
    is_selected: bool,
    any_pending: bool,
    on_select: EventHandler<()>,
) -> Element {
    let candidate_label = candidate.label.clone();
    let candidate_member_count = candidate.member_count;
    let candidate_is_private = candidate.is_private;

    let aria = format!("Select room {} as the invite destination", candidate_label);
    let select_class = if is_selected {
        "border-accent bg-accent/10"
    } else if any_pending {
        "border-border opacity-60 cursor-not-allowed"
    } else {
        "border-border hover:bg-surface cursor-pointer"
    };
    rsx! {
        button {
            class: format!(
                "w-full text-left px-3 py-2 rounded-lg border text-sm text-text flex items-center gap-2 transition-colors {}",
                select_class
            ),
            disabled: any_pending,
            "aria-label": "{aria}",
            "aria-pressed": "{is_selected}",
            onclick: move |_| on_select.call(()),
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
            if is_selected {
                span {
                    class: "text-accent text-xs flex-shrink-0",
                    "aria-label": "Selected",
                    "✓"
                }
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

/// Sign the invitation, encode it, and dispatch a structured
/// `DirectMessageBody::Invite` DM. Returns a user-facing error string
/// on failure or `Ok(())` on success.
#[allow(clippy::too_many_arguments)]
async fn drive_send(
    current_room: VerifyingKey,
    target_peer: MemberId,
    candidate_data: crate::room_data::RoomData,
    member: Member,
    room_key: river_core::chat_delegate::RoomKey,
    inviter_sk: SigningKey,
    invitee_signing_key: SigningKey,
    personal_message: Option<String>,
) -> Result<(), String> {
    // Sign the member-claim via the delegate-backed signing path. Same
    // semantics as the legacy URL-paste flow.
    let mut member_bytes = Vec::new();
    if ciborium::ser::into_writer(&member, &mut member_bytes).is_err() {
        return Err("Couldn't serialize membership claim. Try again.".into());
    }
    let signature =
        crate::signing::sign_member_with_fallback(room_key, member_bytes, &inviter_sk).await;
    let authorized = AuthorizedMember::with_signature(member, signature);
    let invitation = Invitation {
        room: candidate_data.owner_vk,
        invitee_signing_key,
        invitee: authorized,
    };

    // Encode the Invitation as CBOR — same bytes the URL form base58-
    // encodes. The recipient decodes these bytes back to `Invitation`.
    let mut invitation_payload = Vec::new();
    ciborium::ser::into_writer(&invitation, &mut invitation_payload)
        .map_err(|e| format!("Couldn't encode invitation: {}", e))?;

    let body = DirectMessageBody::Invite(Box::new(InvitePayload {
        room_owner_vk: candidate_data.owner_vk,
        invitation_payload,
        personal_message,
    }));

    match send_structured_dm(current_room, target_peer, body).await {
        SendDmOutcome::Sent => Ok(()),
        SendDmOutcome::RoomGone => Err("The room you're DM'ing in is no longer loaded.".into()),
        SendDmOutcome::RecipientNotMember => {
            Err("The recipient is no longer a member of this room.".into())
        }
        SendDmOutcome::SelfDm => Err("Cannot send a DM to yourself.".into()),
        SendDmOutcome::SenderMissingRejoin => Err(
            "You're not currently in this room's member list and no rejoin \
             credentials are stored locally. Reload the room or re-accept your \
             invitation before sending an invite DM."
                .into(),
        ),
        SendDmOutcome::CapHit => Err(
            "This thread is full. Ask the recipient to delete some older DMs \
             from you, then try again."
                .into(),
        ),
        SendDmOutcome::BodyTooLargeOrEncodeFailed(e) => Err(format!(
            "Couldn't send invite — body too large or encode failed: {}",
            e
        )),
        SendDmOutcome::DeltaFailed(e) => Err(format!(
            "Couldn't send invite — local apply_delta failed: {}",
            e
        )),
        SendDmOutcome::SilentDrop => Err(
            "Invite couldn't be added to the room (your member entry may be \
             missing). Try posting a message in the room first, then retry."
                .into(),
        ),
    }
}

/// Schedule a one-shot watchdog that clears `INVITE_VIA_DM_PICKER_INFLIGHT`
/// if `my_generation` is still in flight after `PICKER_WATCHDOG_SECS`.
/// Belt-and-suspenders against a stuck spawn_local task (Skeptical-review
/// M1 on PR #260).
fn schedule_watchdog(my_generation: u64) {
    use std::time::Duration;
    crate::util::safe_spawn_local(async move {
        crate::util::sleep(Duration::from_secs(PICKER_WATCHDOG_SECS)).await;
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
            // Don't unconditionally drop the user's draft on watchdog —
            // they may want to retry. Leave the picker open with a
            // generic error.
        });
    });
}
