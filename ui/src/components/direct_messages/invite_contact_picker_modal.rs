//! "Share Invite" contact picker (#566).
//!
//! The mirror image of [`super::invite_via_dm_picker_modal`]. That picker
//! starts from a PERSON (a member of the room you're looking at) and asks
//! which of your other rooms to invite them to. This one starts from the
//! ROOM you're looking at and asks WHO to invite — which is the direction
//! users actually arrive from, because the invite control lives at the
//! bottom of that room's member list.
//!
//! Why this exists: the only discoverable way to invite someone used to be
//! "Invite Member", which mints a link. That link is a bearer credential
//! for exactly one person, and in the field people share one link with
//! several people, at which point everyone who used it shares an identity
//! and none of them work. The DM route has none of that failure mode — the
//! recipient gets an Accept button inside a DM thread and no credential
//! ever leaves River — but it was three clicks deep behind a member's card
//! in a *different* room, so nobody found it. This surfaces it as the
//! primary invite action.
//!
//! Candidates are every member of every OTHER room the local user is in
//! (the current room is excluded — its members are already in). Per-room
//! identities mean the same human has a different `member_vk` per room, so
//! we cannot dedupe a person across rooms, and cannot tell whether they
//! are already a member of the target room. Each row therefore names both
//! the person and the room the DM would be sent in, and the local user —
//! who has the context — picks.
//!
//! In-flight / watchdog machinery mirrors the sibling picker for the same
//! reasons documented there: the component is mounted unconditionally in
//! `app.rs` and never unmounts, so a send can outlive a close, and a
//! generation counter lets superseded picks short-circuit.

use crate::components::app::{CURRENT_ROOM, ROOMS};
use crate::components::direct_messages::invite_dm::compose_and_send_invite_dm;
use crate::components::direct_messages::{
    ContactPickInflight, INVITE_CONTACT_PICKER, INVITE_CONTACT_PICKER_INFLIGHT,
};
use crate::components::members::{
    deputy_badges_for_viewer, impersonation_checker_for_viewer, impersonation_warning_for_display,
    privilege_in_view,
};
use crate::util::confusable::WARNING_GLYPH;
use crate::util::ecies::unseal_bytes_with_secrets;
use dioxus::logger::tracing::{error, info, warn};
use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use river_core::room_state::member::MemberId;
use std::sync::atomic::{AtomicU64, Ordering};

/// One person the local user could DM an invitation to, together with the
/// room whose DM channel would carry it.
#[derive(Clone, PartialEq, Debug)]
pub(crate) struct ContactCandidate {
    /// Room the DM is sent inside. Both the local user and `peer` are
    /// members of it. NEVER the invitation's target room.
    pub carrier_room: VerifyingKey,
    /// Display name of `carrier_room`.
    pub room_label: String,
    pub peer: MemberId,
    /// Display name of `peer` in `carrier_room`.
    pub peer_label: String,
    /// The pair already has DM history in `carrier_room`. A person you've
    /// actually talked to is far more likely to be who you meant than a
    /// stranger who happens to share a large room, so these sort first.
    pub has_dm_history: bool,
    /// Tooltip for the ⚠ badge when this person's display name is visually
    /// identical to a privileged member's in `carrier_room`; `None` when
    /// there is nothing to warn about.
    ///
    /// The member list carries this badge, and before this picker existed
    /// the only route to a DM invite went THROUGH the member list — so the
    /// user had seen the badge before choosing anyone. Picking from here
    /// skips that list entirely, so the warning has to travel with the row
    /// or promoting this surface would quietly remove a protection.
    pub warning_tooltip: Option<String>,
}

/// Watchdog timeout, matching the sibling picker. `sign_member_with_fallback`
/// caps at 10s before its local-signing fallback; this is the catch-all for
/// "something else got wedged".
const PICKER_WATCHDOG_SECS: u64 = 15;

/// Cap on the personal-message field — a UX cap against a runaway paste,
/// not a wire-format cap. Matches the sibling picker.
const PERSONAL_MESSAGE_CHAR_CAP: usize = 4_000;

/// Most rows rendered at once. A user in a large room has hundreds of
/// co-members, and rendering all of them makes the list unusable and the
/// per-keystroke render cost real. Anything beyond this is reachable by
/// typing in the filter box, and the count of what's hidden is shown so
/// the truncation is never silent.
const MAX_RENDERED_CANDIDATES: usize = 40;

/// Monotonic pick generation, outside any component scope so it can never
/// panic on access.
static PICK_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Order candidates: people you have DM history with first, then by
/// display name, then by room name, then by member id so the order is
/// total and stable across renders (two members can share a nickname —
/// nicknames are not unique — and a non-total comparator would let rows
/// swap between renders).
pub(crate) fn sort_contacts(candidates: &mut [ContactCandidate]) {
    candidates.sort_by(|a, b| {
        b.has_dm_history
            .cmp(&a.has_dm_history)
            .then_with(|| {
                a.peer_label
                    .to_lowercase()
                    .cmp(&b.peer_label.to_lowercase())
            })
            .then_with(|| {
                a.room_label
                    .to_lowercase()
                    .cmp(&b.room_label.to_lowercase())
            })
            .then_with(|| a.peer.to_string().cmp(&b.peer.to_string()))
    });
}

/// Case-insensitive substring match against the person's name OR the
/// room's name — a user who remembers "someone from the dev room" can get
/// there by typing the room.
pub(crate) fn contact_matches_query(candidate: &ContactCandidate, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    candidate.peer_label.to_lowercase().contains(&q)
        || candidate.room_label.to_lowercase().contains(&q)
}

/// Apply the filter and the render cap. Returns the rows to render plus
/// how many further matches were withheld, so the caller can say so.
pub(crate) fn visible_contacts(
    all: &[ContactCandidate],
    query: &str,
    cap: usize,
) -> (Vec<ContactCandidate>, usize) {
    let matching: Vec<ContactCandidate> = all
        .iter()
        .filter(|c| contact_matches_query(c, query))
        .cloned()
        .collect();
    let hidden = matching.len().saturating_sub(cap);
    let mut shown = matching;
    shown.truncate(cap);
    (shown, hidden)
}

/// Open the contact picker for `target_room` — the room the invitation
/// will grant access to.
pub fn open_invite_contact_picker(target_room: VerifyingKey) {
    crate::util::defer(move || {
        *INVITE_CONTACT_PICKER.write() = Some(target_room);
    });
}

/// Is there anybody the local user could DM an invite to for
/// `target_room`? Drives the enabled state of the "Share Invite" button so
/// it doesn't lead to an empty picker.
///
/// Returns `None` when `ROOMS` can't be read — the caller decides what to
/// do with "don't know" (the member panel leaves the button enabled, so a
/// contended read never hides a working action; the picker's own empty
/// state covers the case where it really was empty).
pub fn has_invitable_contacts(target_room: VerifyingKey) -> Option<bool> {
    let rooms = ROOMS.try_read().ok()?;
    Some(rooms.map.iter().any(|(owner_vk, room_data)| {
        if *owner_vk == target_room {
            return false;
        }
        let self_id: MemberId = room_data.self_sk.verifying_key().into();
        let owner_id = MemberId::from(owner_vk);
        (owner_id != self_id)
            || room_data
                .room_state
                .members
                .members
                .iter()
                .any(|m| m.member.id() != self_id)
    }))
}

#[component]
pub fn InviteContactPickerModal() -> Element {
    // --- Hooks first (Rules of Hooks) --------------------------------
    // ALL hooks must run unconditionally, BEFORE the early return below.
    // This component renders 0 hooks when closed and N when open, which
    // is only sound as strict all-or-nothing — a hook after the guard
    // would shift the sequence on the first close→reopen and panic.
    //
    // The selection is tagged with the target room it was made for, so a
    // selection left over from a previous open (the `use_signal`s survive
    // close: the component never unmounts) can't arm the Send button.
    let mut selected: Signal<Option<(VerifyingKey, VerifyingKey, MemberId)>> = use_signal(|| None);
    let mut query = use_signal(String::new);
    let mut personal_message = use_signal(String::new);
    let mut send_error: Signal<Option<String>> = use_signal(|| None);
    let mut last_success_label: Signal<Option<String>> = use_signal(|| None);

    // Reset scratch state on every open/close/target transition. Reads
    // only `INVITE_CONTACT_PICKER`, so it can't feed back on the signals
    // it writes. Uses `.read()` deliberately, not `try_read()`: this is
    // the effect's sole subscription and a miss would silently drop it,
    // permanently disabling the reset (see AGENTS.md).
    use_effect(move || {
        let _ = INVITE_CONTACT_PICKER.read();
        selected.set(None);
        query.set(String::new());
        personal_message.set(String::new());
        send_error.set(None);
        last_success_label.set(None);
    });

    // --- Early return: render nothing while the picker is closed ------
    let active = *INVITE_CONTACT_PICKER.read();
    let Some(target_room) = active else {
        return rsx! {};
    };

    let in_flight = *INVITE_CONTACT_PICKER_INFLIGHT.read();
    let any_pending = in_flight.is_some();

    let close = move |_| {
        // Don't close mid-send.
        if INVITE_CONTACT_PICKER_INFLIGHT.read().is_some() {
            return;
        }
        crate::util::defer(move || {
            *INVITE_CONTACT_PICKER.write() = None;
        });
    };

    // Target-room label and data. Computed inline every render, NOT via
    // `use_memo`: a memo subscribes only to the signals it reads
    // (`ROOMS`), and `target_room` is a plain captured value, so reopening
    // for a different room would keep handing back the first room's label
    // for the life of the session (the #291 bug, same shape).
    let target_room_data = ROOMS
        .try_read()
        .ok()
        .and_then(|r| r.map.get(&target_room).cloned());
    let target_label = target_room_data
        .as_ref()
        .map(room_display_label)
        .unwrap_or_else(|| "this room".to_string());

    // Candidate list, also built inline for the same reason.
    let all_candidates = build_contact_candidates(target_room);
    let query_value = query.read().clone();
    let (rows, hidden_count) =
        visible_contacts(&all_candidates, &query_value, MAX_RENDERED_CANDIDATES);

    // Two synchronous render-time checks, so the Send button is never
    // armed by a stale selection — not even on the first frame after
    // reopen, before the reset effect runs: the selection must be tagged
    // with the current target room, and must still be a live candidate.
    let selected_value = (*selected.read())
        .filter(|(tag, _, _)| *tag == target_room)
        .map(|(_, carrier, peer)| (carrier, peer))
        .filter(|(carrier, peer)| {
            all_candidates
                .iter()
                .any(|c| c.carrier_room == *carrier && c.peer == *peer)
        });
    let personal_message_value = personal_message.read().clone();
    let send_error_value = send_error.read().clone();
    let last_success_label_value = last_success_label.read().clone();
    let pmessage_chars = personal_message_value.chars().count();
    let can_send = selected_value.is_some() && !any_pending;

    let candidates_for_send = all_candidates.clone();
    let target_label_for_send = target_label.clone();
    let do_send = move |_| {
        // Re-check pending; a click can race the disabled attribute.
        if INVITE_CONTACT_PICKER_INFLIGHT.peek().is_some() {
            return;
        }
        // Guard the send path directly rather than trusting the button's
        // disabled state — same two checks as the render filter above.
        let (carrier_room, peer) = match *selected.peek() {
            Some((tag, carrier, peer))
                if tag == target_room
                    && candidates_for_send
                        .iter()
                        .any(|c| c.carrier_room == carrier && c.peer == peer) =>
            {
                (carrier, peer)
            }
            _ => {
                send_error.set(Some("Pick someone to invite first.".into()));
                return;
            }
        };
        send_error.set(None);

        let pmessage = personal_message.peek().clone();
        let pmessage_opt = if pmessage.trim().is_empty() {
            None
        } else {
            Some(pmessage.trim().to_string())
        };

        // The invitation is signed against the TARGET room, so its data
        // (identity + secrets) is what the send needs — re-read here so a
        // room unloaded since render fails loudly instead of signing
        // against stale state.
        let Some(target_data) = ROOMS
            .try_read()
            .ok()
            .and_then(|r| r.map.get(&target_room).cloned())
        else {
            error!("invite-contact-picker: target room data missing");
            send_error.set(Some(
                "This room is no longer loaded. Reopen it and try again.".into(),
            ));
            return;
        };
        let peer_label = candidates_for_send
            .iter()
            .find(|c| c.carrier_room == carrier_room && c.peer == peer)
            .map(|c| c.peer_label.clone())
            .unwrap_or_else(|| "them".to_string());

        let my_generation = PICK_GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
        crate::util::defer(move || {
            *INVITE_CONTACT_PICKER_INFLIGHT.write() = Some(ContactPickInflight {
                generation: my_generation,
                carrier_room,
                peer,
            });
        });

        let target_label_for_task = target_label_for_send.clone();
        crate::util::safe_spawn_local(async move {
            // carrier_room carries the DM; target_room is what the
            // invitation grants — the opposite assignment from the
            // sibling picker, which is the whole point of this surface.
            let outcome =
                compose_and_send_invite_dm(carrier_room, peer, target_data, pmessage_opt).await;

            crate::util::defer(move || {
                // This pick may have been superseded while awaiting: the
                // watchdog may have fired, or the user may have started a
                // newer pick. Applying picker-local UI updates then would
                // clobber a newer session or resurrect a closed one, so
                // gate them on the generation still being ours and do
                // only the global INFLIGHT cleanup otherwise.
                let still_mine = matches!(
                    *INVITE_CONTACT_PICKER_INFLIGHT.peek(),
                    Some(p) if p.generation == my_generation
                );
                clear_inflight_if_matches(my_generation);
                match outcome {
                    Ok(()) => {
                        info!("invite-contact-picker: sent invite for room {target_room:?}");
                        if still_mine {
                            last_success_label.set(Some(format!(
                                "{peer_label} — invitation to \"{target_label_for_task}\" sent"
                            )));
                            *INVITE_CONTACT_PICKER.write() = None;
                        } else {
                            // The send DID succeed — `send_structured_dm`
                            // already wrote to ROOMS and queued sync, so
                            // the recipient gets it regardless. Only the
                            // picker-local feedback is skipped.
                            info!(
                                "invite-contact-picker: send completed after watchdog \
                                 fired — skipping picker-local UI updates"
                            );
                        }
                    }
                    Err(e) => {
                        warn!("invite-contact-picker: send failed: {e}");
                        if still_mine {
                            send_error.set(Some(e));
                        } else {
                            warn!(
                                "invite-contact-picker: error arrived after watchdog \
                                 fired; not surfacing to closed picker"
                            );
                        }
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
                "data-testid": "invite-contact-picker-modal",
                class: "relative z-10 w-full max-w-md mx-4 bg-panel rounded-xl shadow-xl border border-border flex flex-col max-h-[80vh]",
                div { class: "flex items-center justify-between px-5 py-4 border-b border-border",
                    h2 { class: "text-base font-semibold text-text",
                        "Invite someone to "
                        span { class: "text-accent", "{target_label}" }
                    }
                    button {
                        "data-testid": "invite-contact-picker-close-button",
                        class: format!(
                            "p-1 text-text-muted hover:text-text transition-colors text-xl {}",
                            if any_pending { "opacity-40 cursor-not-allowed" } else { "" }
                        ),
                        disabled: any_pending,
                        "aria-label": "Close invite picker",
                        onclick: close,
                        "✕"
                    }
                }
                div { class: "flex-1 overflow-y-auto px-5 py-4 space-y-3",
                    if all_candidates.is_empty() {
                        div {
                            "data-testid": "invite-contact-picker-empty",
                            class: "text-sm text-text-muted space-y-2",
                            p {
                                "There's nobody to send an invitation to yet — River can only DM an \
                                 invite to someone you already share a room with."
                            }
                            p {
                                "Use "
                                span { class: "text-text font-medium", "Invite by link" }
                                " instead. That link works for "
                                span { class: "text-text font-medium", "one person only" }
                                " — generate a fresh one for each person you invite."
                            }
                        }
                    } else {
                        p { class: "text-xs text-text-muted",
                            "River sends them an invitation card in a DM, with an Accept button. \
                             Nothing to copy, paste, or leak."
                        }
                        div { class: "space-y-1",
                            label { class: "text-xs text-text-muted block", "Who?" }
                            input {
                                "data-testid": "invite-contact-picker-search",
                                class: "w-full px-3 py-2 bg-surface border border-border rounded-lg text-sm text-text",
                                r#type: "text",
                                placeholder: "Search by name or room…",
                                value: "{query_value}",
                                disabled: any_pending,
                                oninput: move |e| query.set(e.value()),
                            }
                            if rows.is_empty() {
                                p {
                                    "data-testid": "invite-contact-picker-no-matches",
                                    class: "text-xs text-text-muted py-2",
                                    "Nobody matches that."
                                }
                            }
                            for row in rows.iter() {
                                ContactRow {
                                    key: "{row.carrier_room:x?}-{row.peer}",
                                    candidate: row.clone(),
                                    is_selected: selected_value == Some((row.carrier_room, row.peer)),
                                    any_pending,
                                    on_select: {
                                        let carrier = row.carrier_room;
                                        let peer = row.peer;
                                        move |_| {
                                            selected.set(Some((target_room, carrier, peer)));
                                        }
                                    },
                                }
                            }
                            if hidden_count > 0 {
                                p { class: "text-[10px] text-text-muted pt-1",
                                    "{hidden_count} more — narrow the search to see them."
                                }
                            }
                        }
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
                                    // Soft-cap: trim rather than reject so a
                                    // paste of something bigger still leaves
                                    // text the user can edit.
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
                        div { class: "text-xs text-emerald-400", "{label}" }
                    }
                }
                if !all_candidates.is_empty() {
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
                            "data-testid": "invite-contact-picker-send-button",
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
fn ContactRow(
    candidate: ContactCandidate,
    is_selected: bool,
    any_pending: bool,
    on_select: EventHandler<()>,
) -> Element {
    let peer_label = candidate.peer_label.clone();
    let room_label = candidate.room_label.clone();
    let has_dm_history = candidate.has_dm_history;
    let warning_tooltip = candidate.warning_tooltip.clone();

    // The accessible name must carry the warning too. The glyph is a child
    // of a button with an explicit `aria-label`, and an explicit label
    // REPLACES the element's content for naming purposes — so a badge
    // rendered inside is invisible to a screen reader unless it is said
    // here. Tests read `data-person` / `data-room` instead of parsing this
    // string, so the copy is free to grow without breaking them.
    let aria = match warning_tooltip.as_deref() {
        Some(_) => format!(
            "Send the invitation to {peer_label}, in a DM inside {room_label}. \
             Warning: this name is visually identical to a privileged \
             member's — check the member ID before inviting"
        ),
        None => format!("Send the invitation to {peer_label}, in a DM inside {room_label}"),
    };
    let select_class = if is_selected {
        "border-accent bg-accent/10"
    } else if any_pending {
        "border-border opacity-60 cursor-not-allowed"
    } else {
        "border-border hover:bg-surface cursor-pointer"
    };
    rsx! {
        button {
            "data-testid": "invite-contact-row",
            // Addressing hooks for automation, so a spec never has to
            // parse the accessible name (which carries prose that is
            // expected to change).
            "data-person": "{peer_label}",
            "data-room": "{room_label}",
            class: format!(
                "w-full text-left px-3 py-2 rounded-lg border text-sm text-text flex items-center gap-2 transition-colors {}",
                select_class
            ),
            disabled: any_pending,
            "aria-label": "{aria}",
            "aria-pressed": "{is_selected}",
            // Member ID on hover — the remedy the warning tooltip tells
            // the reader to use, and the only way to tell two members with
            // the same display name apart.
            title: "Member ID: {candidate.peer}",
            onclick: move |_| on_select.call(()),
            div { class: "flex-1 min-w-0",
                div { class: "truncate", "{peer_label}" }
                div { class: "text-[10px] text-text-muted truncate",
                    if has_dm_history {
                        "you've DM'd them · via {room_label}"
                    } else {
                        "via {room_label}"
                    }
                }
            }
            if let Some(tooltip) = warning_tooltip {
                span {
                    "data-testid": "invite-contact-impersonation-warning",
                    class: "member-icon flex-shrink-0",
                    title: "{tooltip}",
                    " {WARNING_GLYPH}"
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

/// Display name for a room, unsealing the private-room case.
fn room_display_label(room_data: &crate::room_data::RoomData) -> String {
    let sealed = &room_data
        .room_state
        .configuration
        .configuration
        .display
        .name;
    match unseal_bytes_with_secrets(sealed, &room_data.secrets) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
        Err(_) => sealed.to_string_lossy(),
    }
}

/// Every person the local user could DM an invitation for `target_room`
/// to, sorted for display. Excludes `target_room` itself (its members are
/// already in) and the local user's own per-room identity in each room.
///
/// Returns an empty list rather than failing when `ROOMS` is contended;
/// the picker renders its empty state and the next render recovers.
fn build_contact_candidates(target_room: VerifyingKey) -> Vec<ContactCandidate> {
    let Ok(rooms) = ROOMS.try_read() else {
        return Vec::new();
    };
    let mut out: Vec<ContactCandidate> = Vec::new();
    for (owner_vk, room_data) in rooms.map.iter() {
        if *owner_vk == target_room {
            continue;
        }
        let room_label = room_display_label(room_data);
        let self_id: MemberId = room_data.self_sk.verifying_key().into();
        let owner_id = MemberId::from(owner_vk);
        // Both built ONCE per room, BEFORE the per-peer loop. Rebuilding
        // either per peer re-folds every protected name and, in a private
        // room, re-unseals every protected nickname, once per row — the
        // same cost rule `MemberList` follows.
        let deputy_badges = deputy_badges_for_viewer(
            &room_data.room_state.members,
            &room_data.room_state.member_info,
            &room_data.secrets,
            owner_id,
            self_id,
        );
        let impersonation = impersonation_checker_for_viewer(
            &room_data.room_state.member_info,
            &room_data.secrets,
            owner_id,
            &deputy_badges,
        );
        // The owner is not in `members.members` (membership is implicit
        // for them) but IS a valid DM recipient — `send_structured_dm`
        // special-cases them — so add them explicitly or the one person
        // most likely to be able to help a newcomer is missing.
        let peers = std::iter::once(owner_id).chain(
            room_data
                .room_state
                .members
                .members
                .iter()
                .map(|m| m.member.id()),
        );
        for peer in peers {
            if peer == self_id {
                continue;
            }
            let peer_label = room_data
                .room_state
                .member_info
                .canonical(peer)
                .map(|mi| {
                    crate::util::display_name::display_nickname(
                        &mi.member_info.preferred_nickname,
                        &room_data.secrets,
                    )
                })
                .unwrap_or_else(|| peer.to_string().chars().take(8).collect());
            let has_dm_history = room_data
                .room_state
                .direct_messages
                .messages
                .iter()
                .any(|m| {
                    let (sender, recipient) = (m.message.sender, m.message.recipient);
                    (sender == self_id && recipient == peer)
                        || (sender == peer && recipient == self_id)
                });
            // `peer`, not `self_id` — passing any other id flags the
            // genuine owner and every genuine moderator instead of the
            // imitator. Same argument trap the member list is pinned for.
            let warning_tooltip = impersonation_warning_for_display(
                &impersonation,
                peer,
                &peer_label,
                privilege_in_view(peer, owner_id, &deputy_badges),
            )
            .map(|w| w.tooltip());
            out.push(ContactCandidate {
                carrier_room: *owner_vk,
                room_label: room_label.clone(),
                peer,
                peer_label,
                has_dm_history,
                warning_tooltip,
            });
        }
    }
    sort_contacts(&mut out);
    out
}

/// Clear `INVITE_CONTACT_PICKER_INFLIGHT` only if it still names this
/// pick's generation, so a stale terminal-defer can't wipe a newer pick's
/// marker.
fn clear_inflight_if_matches(my_generation: u64) {
    let still_mine = matches!(
        *INVITE_CONTACT_PICKER_INFLIGHT.peek(),
        Some(p) if p.generation == my_generation
    );
    if still_mine {
        *INVITE_CONTACT_PICKER_INFLIGHT.write() = None;
    }
}

/// Clear the in-flight marker AND force-close the picker if this
/// generation is still pending after `PICKER_WATCHDOG_SECS`.
///
/// Force-closing (rather than merely clearing the marker) avoids the
/// "picker open but its pick already abandoned" state: every late
/// completion then finds INFLIGHT cleared, its still-mine gate skips the
/// UI updates, and the user sees a closed modal — a clear signal to check
/// the DM thread — instead of a modal with no feedback and a retry that
/// could double-send. Same trade-off as the sibling picker: a timeout
/// loses the typed personal message.
fn schedule_watchdog(my_generation: u64) {
    use std::time::Duration;
    crate::util::safe_spawn_local(async move {
        crate::util::sleep(Duration::from_secs(PICKER_WATCHDOG_SECS)).await;
        crate::util::defer(move || {
            let still_mine = matches!(
                *INVITE_CONTACT_PICKER_INFLIGHT.peek(),
                Some(p) if p.generation == my_generation
            );
            if !still_mine {
                return;
            }
            warn!(
                "invite-contact-picker: watchdog fired after {PICKER_WATCHDOG_SECS}s; \
                 force-closing picker"
            );
            clear_inflight_if_matches(my_generation);
            *INVITE_CONTACT_PICKER.write() = None;
        });
    });
}

/// Open the contact picker for whatever room is currently being viewed.
/// Returns `false` (and does nothing) when no room is selected, so a
/// caller can fall back rather than opening an unanchored picker.
pub fn open_invite_contact_picker_for_current_room() -> bool {
    let Some(room) = CURRENT_ROOM.read().owner_key else {
        return false;
    };
    open_invite_contact_picker(room);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn vk(seed: u8) -> VerifyingKey {
        SigningKey::from_bytes(&[seed; 32]).verifying_key()
    }

    fn member(seed: u8) -> MemberId {
        MemberId::from(&vk(seed))
    }

    fn candidate(peer_seed: u8, peer_label: &str, room_label: &str, dm: bool) -> ContactCandidate {
        ContactCandidate {
            carrier_room: vk(peer_seed.wrapping_add(100)),
            room_label: room_label.to_string(),
            peer: member(peer_seed),
            peer_label: peer_label.to_string(),
            has_dm_history: dm,
            warning_tooltip: None,
        }
    }

    #[test]
    fn dm_history_sorts_ahead_of_strangers() {
        let mut v = vec![
            candidate(1, "Alice", "Room A", false),
            candidate(2, "Zoe", "Room A", true),
        ];
        sort_contacts(&mut v);
        // Zoe sorts first despite the later name: a person you've actually
        // DM'd is far more likely to be who you meant.
        assert_eq!(v[0].peer_label, "Zoe");
        assert_eq!(v[1].peer_label, "Alice");
    }

    #[test]
    fn names_sort_case_insensitively_then_by_room() {
        let mut v = vec![
            candidate(3, "bob", "Zed Room", false),
            candidate(4, "Bob", "Alpha Room", false),
            candidate(5, "alice", "Zed Room", false),
        ];
        sort_contacts(&mut v);
        assert_eq!(v[0].peer_label, "alice");
        // Same name (case-insensitively) → the room name breaks the tie.
        assert_eq!(v[1].room_label, "Alpha Room");
        assert_eq!(v[2].room_label, "Zed Room");
    }

    #[test]
    fn sort_is_total_so_duplicate_names_and_rooms_cannot_swap() {
        // Nicknames are NOT unique, and two members of the SAME room can
        // share one. Without the member-id tiebreak the comparator is not
        // total and rows could reorder between renders.
        let a = ContactCandidate {
            carrier_room: vk(9),
            room_label: "Same Room".into(),
            peer: member(1),
            peer_label: "Ian".into(),
            has_dm_history: false,
            warning_tooltip: None,
        };
        let b = ContactCandidate {
            carrier_room: vk(9),
            room_label: "Same Room".into(),
            peer: member(2),
            peer_label: "Ian".into(),
            has_dm_history: false,
            warning_tooltip: None,
        };
        let mut forward = vec![a.clone(), b.clone()];
        let mut reverse = vec![b, a];
        sort_contacts(&mut forward);
        sort_contacts(&mut reverse);
        assert_eq!(forward, reverse);
    }

    #[test]
    fn query_matches_person_or_room_case_insensitively() {
        let c = candidate(6, "Nacho", "Freenet Devs", false);
        assert!(contact_matches_query(&c, ""));
        assert!(contact_matches_query(&c, "  "));
        assert!(contact_matches_query(&c, "nach"));
        assert!(contact_matches_query(&c, "NACH"));
        // Matching on the ROOM is deliberate: "someone from the dev room"
        // is how people remember contacts they haven't named.
        assert!(contact_matches_query(&c, "devs"));
        assert!(!contact_matches_query(&c, "hector"));
    }

    #[test]
    fn visible_contacts_caps_and_reports_what_it_withheld() {
        let all: Vec<ContactCandidate> = (0u8..10)
            .map(|i| candidate(i, &format!("Person {i}"), "Room A", false))
            .collect();
        let (shown, hidden) = visible_contacts(&all, "", 4);
        assert_eq!(shown.len(), 4);
        // Truncation is never silent — the caller renders this count.
        assert_eq!(hidden, 6);

        let (shown, hidden) = visible_contacts(&all, "Person 3", 4);
        assert_eq!(shown.len(), 1);
        assert_eq!(hidden, 0);

        let (shown, hidden) = visible_contacts(&all, "nobody", 4);
        assert!(shown.is_empty());
        assert_eq!(hidden, 0);
    }
}
