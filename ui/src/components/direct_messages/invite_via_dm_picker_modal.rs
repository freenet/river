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

use crate::components::app::{MEMBER_INFO_MODAL, ROOMS};
use crate::components::direct_messages::{open_dm_thread, DM_DRAFT, INVITE_VIA_DM_PICKER};
use crate::components::members::Invitation;
use crate::util::ecies::unseal_bytes_with_secrets;
use dioxus::logger::tracing::{error, info};
use dioxus::prelude::*;
use ed25519_dalek::{SigningKey, VerifyingKey};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId};

#[component]
pub fn InviteViaDmPickerModal() -> Element {
    let active = *INVITE_VIA_DM_PICKER.read();
    let Some((current_room, target_peer)) = active else {
        return rsx! {};
    };

    let close = move |_| {
        crate::util::defer(move || {
            *INVITE_VIA_DM_PICKER.write() = None;
        });
    };

    // Build a sorted list of candidate rooms: every room the local user
    // has loaded that isn't the current one. Filtering out rooms where
    // `target_peer` is already a member is intentionally NOT done — see
    // module docs (per-room identities, no cross-room lookup).
    let candidates = use_memo(move || -> Vec<(VerifyingKey, String)> {
        let Ok(rooms) = ROOMS.try_read() else {
            return Vec::new();
        };
        let mut out: Vec<(VerifyingKey, String)> = rooms
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
                (*owner_vk, label)
            })
            .collect();
        out.sort_by(|a, b| a.1.cmp(&b.1));
        out
    });

    let candidates_value = candidates.read().clone();

    rsx! {
        div {
            class: "fixed inset-0 z-50 flex items-center justify-center",
            div {
                class: "absolute inset-0 bg-black/50",
                onclick: close,
            }
            div {
                class: "relative z-10 w-full max-w-md mx-4 bg-panel rounded-xl shadow-xl border border-border flex flex-col max-h-[80vh]",
                // Header
                div { class: "flex items-center justify-between px-5 py-4 border-b border-border",
                    h2 { class: "text-lg font-semibold text-text", "Share an invite via DM" }
                    button {
                        class: "p-1 text-text-muted hover:text-text transition-colors text-xl",
                        onclick: close,
                        "✕"
                    }
                }
                div { class: "flex-1 overflow-y-auto px-2 py-3",
                    if candidates_value.is_empty() {
                        p { class: "text-sm text-text-muted px-3",
                            "You aren't a member of any other rooms yet — create or join one first, then come back here."
                        }
                    } else {
                        p { class: "text-xs text-text-muted px-3 mb-2",
                            "Pick a room to invite this member to. River will generate an invitation link and drop it into a DM in this room for you to review before sending."
                        }
                        for (room_vk, label) in candidates_value.iter() {
                            CandidateRow {
                                key: "{room_vk:?}",
                                current_room: current_room,
                                target_peer: target_peer,
                                candidate_room: *room_vk,
                                label: label.clone()
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
    candidate_room: VerifyingKey,
    label: String,
) -> Element {
    let pick = {
        let label = label.clone();
        move |_| {
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

            wasm_bindgen_futures::spawn_local(async move {
                let mut member_bytes = Vec::new();
                if let Err(e) = ciborium::ser::into_writer(&member, &mut member_bytes) {
                    error!("invite-via-DM: failed to serialize member: {}", e);
                    return;
                }
                // Sign using the delegate-backed signing path with local
                // fallback (same path as `InviteMemberModal`).
                let signature =
                    crate::signing::sign_member_with_fallback(room_key, member_bytes, &inviter_sk)
                        .await;
                let authorized = AuthorizedMember::with_signature(member, signature);
                let invitation = Invitation {
                    room: candidate_data.owner_vk,
                    invitee_signing_key,
                    invitee: authorized,
                };

                let invite_code = invitation.to_encoded_string();
                let base_url =
                    crate::components::members::invite_member_modal::get_invitation_base_url();
                let invite_url = format!("{}?invitation={}", base_url, invite_code);

                let body = format!(
                    "You're invited to join \"{}\" on River. Click to join:\n\n{}",
                    label_inner, invite_url
                );

                crate::util::defer(move || {
                    *DM_DRAFT.write() = Some((current_room, target_peer, body));
                    *INVITE_VIA_DM_PICKER.write() = None;
                    // Close the member-info modal too — both gestures pointed
                    // the user at this picker, so neither should be left
                    // floating behind the thread modal.
                    MEMBER_INFO_MODAL.with_mut(|m| m.member = None);
                });
                open_dm_thread(current_room, target_peer);
                info!(
                    "invite-via-DM: drafted invite for room {:?}",
                    candidate_room
                );
            });
        }
    };

    rsx! {
        button {
            class: "w-full text-left px-3 py-2 rounded-lg hover:bg-surface transition-colors text-sm text-text",
            onclick: pick,
            "{label}"
        }
    }
}
