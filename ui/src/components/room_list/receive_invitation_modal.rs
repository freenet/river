use crate::components::app::freenet_api::freenet_synchronizer::SynchronizerMessage;
use crate::components::app::{PENDING_INVITES, ROOMS, SYNCHRONIZER};
use crate::components::members::Invitation;
use crate::invites::{PendingRoomJoin, PendingRoomStatus};
use crate::room_data::Rooms;
use dioxus::logger::tracing::{error, info, warn};
use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use river_core::room_state::member::MemberId;

const INVITATION_STORAGE_KEY: &str = "river_pending_invitation";
const PROCESSED_INVITATIONS_KEY: &str = "river_processed_invitations";
/// Cap on the number of remembered invitation fingerprints. Old entries are
/// evicted FIFO so localStorage stays bounded across many invite cycles.
const MAX_PROCESSED_INVITATIONS: usize = 64;

/// Save invitation to localStorage so it survives page reloads
pub fn save_invitation_to_storage(invitation: &Invitation) {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            let encoded = invitation.to_encoded_string();
            if let Err(e) = storage.set_item(INVITATION_STORAGE_KEY, &encoded) {
                warn!("Failed to save invitation to localStorage: {:?}", e);
            }
        }
    }
}

/// Load invitation from localStorage (for recovery after page reload)
pub fn load_invitation_from_storage() -> Option<Invitation> {
    let window = web_sys::window()?;
    let storage = window.local_storage().ok()??;
    let encoded = storage.get_item(INVITATION_STORAGE_KEY).ok()??;
    Invitation::from_encoded_string(&encoded).ok()
}

/// Clear saved invitation from localStorage
pub fn clear_invitation_from_storage() {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            let _ = storage.remove_item(INVITATION_STORAGE_KEY);
        }
    }
}

/// Short, stable identifier for an invitation, suitable for storage in a set.
/// 16 bytes of BLAKE3 over the encoded form gives a 2^128 collision space.
fn invitation_fingerprint(encoded: &str) -> String {
    let hash = blake3::hash(encoded.as_bytes());
    let bytes = &hash.as_bytes()[..16];
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn read_processed_list() -> Vec<String> {
    let Some(window) = web_sys::window() else {
        return Vec::new();
    };
    let Ok(Some(storage)) = window.local_storage() else {
        return Vec::new();
    };
    let Ok(Some(raw)) = storage.get_item(PROCESSED_INVITATIONS_KEY) else {
        return Vec::new();
    };
    raw.split('\n')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn write_processed_list(list: &[String]) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Ok(Some(storage)) = window.local_storage() else {
        return;
    };
    let joined = list.join("\n");
    if let Err(e) = storage.set_item(PROCESSED_INVITATIONS_KEY, &joined) {
        warn!("Failed to persist processed invitations: {:?}", e);
    }
}

/// Append `fingerprint` to `list`, deduplicating and capping length.
/// Returns `Some(new_list)` if anything changed, `None` if already present.
fn append_fingerprint(
    mut list: Vec<String>,
    fingerprint: String,
    cap: usize,
) -> Option<Vec<String>> {
    if list.contains(&fingerprint) {
        return None;
    }
    list.push(fingerprint);
    if list.len() > cap {
        let drop = list.len() - cap;
        list.drain(0..drop);
    }
    Some(list)
}

/// Mark an invitation (by its encoded URL form) as processed so future page
/// loads do not re-prompt for a nickname. Called on Accept (entry to the
/// flow) rather than on subscription success: the URL parameter itself is the
/// trigger, and reloading after the user has chosen what to do with this
/// invitation should not surface it again even if `history.replaceState`
/// failed (e.g. inside the gateway's sandboxed iframe, which has no
/// `allow-same-origin`).
pub fn mark_invitation_processed(encoded: &str) {
    let fingerprint = invitation_fingerprint(encoded);
    let list = read_processed_list();
    if let Some(new_list) = append_fingerprint(list, fingerprint, MAX_PROCESSED_INVITATIONS) {
        write_processed_list(&new_list);
    }
}

/// Returns true if `encoded` matches an invitation already processed in this
/// browser. Used by the URL parser to skip stale `?invitation=...` params left
/// behind by a sandbox that blocks `history.replaceState`.
pub fn is_invitation_processed(encoded: &str) -> bool {
    let fingerprint = invitation_fingerprint(encoded);
    read_processed_list().iter().any(|f| f == &fingerprint)
}

/// Dismiss the modal *persistently*: mark the invitation as processed,
/// clear it from `INVITATION_STORAGE_KEY`, and close the modal. Use this for
/// every user-initiated dismiss (Decline, Cancel, Close, Dismiss-on-error).
/// Without the fingerprint mark, a reload would re-surface the modal in the
/// sandboxed-iframe environment where `history.replaceState` cannot strip
/// the `?invitation=...` URL parameter.
fn dismiss_invitation_persistently(inv: &Invitation, mut invitation: Signal<Option<Invitation>>) {
    mark_invitation_processed(&inv.to_encoded_string());
    clear_invitation_from_storage();
    invitation.set(None);
}

/// Main component for the invitation modal
#[component]
pub fn ReceiveInvitationModal(invitation: Signal<Option<Invitation>>) -> Element {
    // No event listener needed — PENDING_INVITES is a GlobalSignal.
    // When get_response.rs sets status to Subscribed, this component
    // re-renders via render_invitation_content reading PENDING_INVITES.

    // Don't render anything if there's no invitation
    let inv_data = invitation.read().as_ref().cloned();
    if inv_data.is_none() {
        return rsx! {};
    }

    rsx! {
        // Modal backdrop - no click dismiss to prevent accidental invitation loss
        div {
            class: "fixed inset-0 z-50 flex items-center justify-center",
            // Overlay (non-dismissable)
            div {
                class: "absolute inset-0 bg-black/50",
            }
            // Modal content
            div {
                class: "relative z-10 w-full max-w-md mx-4 bg-panel rounded-xl shadow-xl border border-border",
                div {
                    class: "p-6",
                    h1 { class: "text-xl font-semibold text-text mb-4", "Invitation Received" }
                    {render_invitation_content(inv_data.unwrap(), invitation)}
                }
            }
        }
    }
}

/// Renders the content of the invitation modal based on the invitation data
fn render_invitation_content(inv: Invitation, invitation: Signal<Option<Invitation>>) -> Element {
    // Clone the status to release the read guard before any branch can mutate
    let status = {
        let pending_invites = PENDING_INVITES.read();
        pending_invites
            .map
            .get(&inv.room)
            .map(|join| join.status.clone())
    };

    match status {
        Some(PendingRoomStatus::PendingSubscription) => render_pending_subscription_state(),
        Some(PendingRoomStatus::Subscribing) => render_subscribing_state(),
        Some(PendingRoomStatus::Error(e)) => render_error_state(&e, &inv, invitation),
        Some(PendingRoomStatus::Subscribed) => {
            // Room subscribed and retrieved successfully, close modal
            render_subscribed_state(&inv.room, invitation)
        }
        None => render_invitation_options(inv, invitation),
    }
}

/// Renders the state when waiting to subscribe to room data
fn render_pending_subscription_state() -> Element {
    rsx! {
        div {
            class: "text-center py-4",
            p { class: "mb-4 text-text", "Preparing to subscribe to room..." }
            div { class: "w-full h-2 bg-surface rounded-full overflow-hidden",
                div { class: "h-full bg-accent animate-pulse w-1/2" }
            }
        }
    }
}

/// Renders the loading state when subscribing to room data
fn render_subscribing_state() -> Element {
    rsx! {
        div {
            class: "text-center py-4",
            p { class: "mb-4 text-text", "Subscribing to room..." }
            div { class: "w-full h-2 bg-surface rounded-full overflow-hidden",
                div { class: "h-full bg-blue-500 animate-pulse w-2/3" }
            }
        }
    }
}

/// Renders the error state when room retrieval fails
fn render_error_state(
    error: &str,
    inv: &Invitation,
    invitation: Signal<Option<Invitation>>,
) -> Element {
    let room_key = inv.room; // Copy type, avoid clone
    let inv_for_dismiss = inv.clone();

    rsx! {
        div {
            class: "bg-red-500/10 border border-red-500/20 rounded-lg p-4",
            p { class: "mb-4 text-red-400", "Failed to retrieve room: {error}" }
            div {
                class: "flex gap-3",
                button {
                    class: "px-4 py-2 bg-accent hover:bg-accent-hover text-white font-medium rounded-lg transition-colors",
                    onmounted: move |cx| {
                        let element = cx.data();
                        wasm_bindgen_futures::spawn_local(async move {
                            let _ = element.set_focus(true).await;
                        });
                    },
                    onclick: move |_| {
                        // Reset to PendingSubscription so the synchronizer retries
                        PENDING_INVITES.with_mut(|pending| {
                            if let Some(join) = pending.map.get_mut(&room_key) {
                                join.status = PendingRoomStatus::PendingSubscription;
                            }
                        });
                    },
                    "Retry"
                }
                button {
                    class: "px-4 py-2 bg-surface hover:bg-surface-hover text-text rounded-lg transition-colors",
                    onclick: move |_| {
                        PENDING_INVITES.write().map.remove(&room_key);
                        dismiss_invitation_persistently(&inv_for_dismiss, invitation);
                    },
                    "Dismiss"
                }
            }
        }
    }
}

/// Renders the state when room is successfully subscribed and retrieved.
/// Cleans up the invitation and returns empty to dismiss the modal.
fn render_subscribed_state(
    room_key: &VerifyingKey,
    mut invitation: Signal<Option<Invitation>>,
) -> Element {
    let room_key = *room_key;
    // Defer signal mutations to avoid RefCell panics during render.
    // The modal renders one empty frame before cleanup runs — acceptable
    // since we return rsx! {} immediately.
    clear_invitation_from_storage();
    crate::util::defer(move || {
        PENDING_INVITES.with_mut(|pending| {
            pending.map.remove(&room_key);
        });
        invitation.set(None);
        info!(
            "Invitation accepted, closing modal for {:?}",
            MemberId::from(room_key)
        );
    });
    rsx! {}
}

/// Renders the invitation options based on the user's membership status
fn render_invitation_options(inv: Invitation, invitation: Signal<Option<Invitation>>) -> Element {
    let Ok(rooms) = ROOMS.try_read() else {
        return rsx! {};
    };
    let (current_key_is_member, invited_member_exists) = check_membership_status(&inv, &rooms);
    drop(rooms);

    if current_key_is_member {
        render_already_member(inv, invitation)
    } else if invited_member_exists {
        render_restore_access_option(inv, invitation)
    } else {
        render_new_invitation(inv, invitation)
    }
}

/// Checks the membership status of the user in the room
fn check_membership_status(inv: &Invitation, current_rooms: &Rooms) -> (bool, bool) {
    if let Some(room_data) = current_rooms.map.get(&inv.room) {
        let user_vk = inv.invitee_signing_key.verifying_key();
        let current_key_is_member = user_vk == room_data.owner_vk
            || room_data
                .room_state
                .members
                .members
                .iter()
                .any(|m| m.member.member_vk == user_vk);
        let invited_member_exists = room_data
            .room_state
            .members
            .members
            .iter()
            .any(|m| m.member.member_vk == inv.invitee.member.member_vk);
        (current_key_is_member, invited_member_exists)
    } else {
        (false, false)
    }
}

/// Renders the UI when the user is already a member of the room.
/// Closing this modal must mark the invitation processed so a reload (with
/// the URL parameter still present) doesn't re-open it.
fn render_already_member(inv: Invitation, invitation: Signal<Option<Invitation>>) -> Element {
    rsx! {
        p { class: "text-text mb-4", "You are already a member of this room with your current key." }
        button {
            class: "px-4 py-2 bg-accent hover:bg-accent-hover text-white font-medium rounded-lg transition-colors",
            onmounted: move |cx| {
                let element = cx.data();
                wasm_bindgen_futures::spawn_local(async move {
                    let _ = element.set_focus(true).await;
                });
            },
            onclick: move |_| {
                dismiss_invitation_persistently(&inv, invitation);
            },
            "Close"
        }
    }
}

/// Renders the UI for restoring access to an existing member
fn render_restore_access_option(
    inv: Invitation,
    invitation: Signal<Option<Invitation>>,
) -> Element {
    rsx! {
        p { class: "text-text mb-2", "This invitation is for a member that already exists in the room." }
        p { class: "text-text-muted mb-4", "If you lost access to your previous key, you can use this invitation to restore access with your current key." }
        div {
            class: "flex gap-3",
            button {
                class: "px-4 py-2 bg-yellow-500 hover:bg-yellow-600 text-white font-medium rounded-lg transition-colors",
                onmounted: move |cx| {
                    let element = cx.data();
                    wasm_bindgen_futures::spawn_local(async move {
                        let _ = element.set_focus(true).await;
                    });
                },
                onclick: {
                    let room = inv.room;
                    let member_vk = inv.invitee.member.member_vk;
                    let inv_for_restore = inv.clone();
                    let inv_for_dismiss = inv.clone();

                    move |_| {
                        // Defer signal mutations to a clean execution context to
                        // prevent RefCell re-entrant borrow panics.
                        let inv_clone = inv_for_restore.invitee.clone();
                        crate::util::defer(move || {
                            ROOMS.with_mut(|rooms| {
                                if let Some(room_data) = rooms.map.get_mut(&room) {
                                    room_data.restore_member_access(
                                        member_vk,
                                        inv_clone,
                                    );
                                }
                            });
                            crate::components::app::mark_needs_sync(room);
                        });
                        dismiss_invitation_persistently(&inv_for_dismiss, invitation);
                    }
                },
                "Restore Access"
            }
            button {
                class: "px-4 py-2 bg-surface hover:bg-surface-hover text-text rounded-lg transition-colors",
                onclick: {
                    let inv_for_cancel = inv.clone();
                    move |_| {
                        dismiss_invitation_persistently(&inv_for_cancel, invitation);
                    }
                },
                "Cancel"
            }
        }
    }
}

/// Renders the UI for a new invitation
fn render_new_invitation(inv: Invitation, invitation: Signal<Option<Invitation>>) -> Element {
    // Clone the invitation for the closures
    let inv_for_accept = inv.clone();
    let inv_for_enter = inv.clone();

    // Generate a default nickname from the member's key
    let encoded = bs58::encode(inv.invitee.member.member_vk.as_bytes()).into_string();
    let shortened = encoded.chars().take(6).collect::<String>();
    let default_nickname = format!("User-{}", shortened);

    // Create a signal for the nickname
    let mut nickname = use_signal(|| default_nickname);

    rsx! {
        p { class: "text-text mb-2", "You have been invited to join a new room." }
        p { class: "text-text-muted mb-4", "Choose a nickname to use in this room:" }

        div { class: "mb-4",
            input {
                class: "w-full px-3 py-2 bg-surface border border-border rounded-lg text-text placeholder-text-muted focus:outline-none focus:ring-2 focus:ring-accent focus:border-transparent",
                r#type: "text",
                value: "{nickname}",
                onmounted: move |cx| {
                    let element = cx.data();
                    wasm_bindgen_futures::spawn_local(async move {
                        let _ = element.set_focus(true).await;
                    });
                },
                oninput: move |evt| nickname.set(evt.value().clone()),
                onkeydown: move |evt: KeyboardEvent| {
                    if evt.key() == Key::Enter && !nickname.read().trim().is_empty() {
                        evt.prevent_default();
                        accept_invitation(inv_for_enter.clone(), nickname.read().clone());
                    }
                },
                placeholder: "Your preferred nickname"
            }
        }

        p { class: "text-text mb-4", "Would you like to accept the invitation?" }
        div {
            class: "flex gap-3",
            button {
                class: "px-4 py-2 bg-accent hover:bg-accent-hover text-white font-medium rounded-lg transition-colors disabled:opacity-50 disabled:cursor-not-allowed",
                disabled: nickname.read().trim().is_empty(),
                onclick: move |_| {
                    accept_invitation(inv_for_accept.clone(), nickname.read().clone());
                },
                "Accept"
            }
            button {
                class: "px-4 py-2 bg-surface hover:bg-surface-hover text-text rounded-lg transition-colors",
                onclick: {
                    let inv_for_decline = inv.clone();
                    move |_| {
                        dismiss_invitation_persistently(&inv_for_decline, invitation);
                    }
                },
                "Decline"
            }
        }
    }
}

/// Handles the invitation acceptance process
fn accept_invitation(inv: Invitation, nickname: String) {
    // Mark this invitation processed up front. The user has now made a choice
    // for this URL parameter; even if subscription fails or the page is
    // reloaded mid-flow, we should not re-prompt for a nickname on every
    // refresh just because the URL still carries `?invitation=...`.
    mark_invitation_processed(&inv.to_encoded_string());

    let room_owner = inv.room;
    let authorized_member = inv.invitee.clone();
    let invitee_signing_key = inv.invitee_signing_key.clone();

    // Use the user-provided nickname
    let nickname = if nickname.trim().is_empty() {
        // Fallback to generated nickname if somehow empty
        let encoded = bs58::encode(authorized_member.member.member_vk.as_bytes()).into_string();
        let shortened = encoded.chars().take(6).collect::<String>();
        format!("User-{}", shortened)
    } else {
        nickname
    };

    info!(
        "Adding room to pending invites: {:?}",
        MemberId::from(room_owner)
    );

    // Add to pending invites
    PENDING_INVITES.with_mut(|pending_invites| {
        pending_invites.map.insert(
            room_owner,
            PendingRoomJoin {
                authorized_member: authorized_member.clone(),
                invitee_signing_key: invitee_signing_key.clone(),
                preferred_nickname: nickname.clone(),
                status: PendingRoomStatus::PendingSubscription,
                subscribing_since: None,
                retry_count: 0,
            },
        );
    });

    info!("Requesting room state for invitation");

    // Send the AcceptInvitation message directly without spawn_local
    let result = SYNCHRONIZER
        .write()
        .get_message_sender()
        .unbounded_send(SynchronizerMessage::AcceptInvitation {
            owner_vk: room_owner,
            authorized_member: Box::new(authorized_member),
            invitee_signing_key: Box::new(invitee_signing_key),
            nickname,
        })
        .map_err(|e| format!("Failed to send message: {}", e));

    match result {
        Ok(_) => {
            info!("Successfully requested room state for invitation");
        }
        Err(e) => {
            // Log detailed error information
            error!("Failed to request room state for invitation: {}", e);
            error!(
                "Error details: invitation for room with owner key: {:?}",
                MemberId::from(room_owner)
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_deterministic() {
        let a = invitation_fingerprint("invitation-code-xyz");
        let b = invitation_fingerprint("invitation-code-xyz");
        assert_eq!(a, b, "same input must hash to same fingerprint");
    }

    #[test]
    fn fingerprint_distinguishes_inputs() {
        let a = invitation_fingerprint("invitation-a");
        let b = invitation_fingerprint("invitation-b");
        assert_ne!(a, b);
    }

    #[test]
    fn fingerprint_is_compact_hex() {
        let fp = invitation_fingerprint("anything");
        assert_eq!(fp.len(), 32, "16 bytes hex-encoded = 32 chars");
        assert!(
            fp.chars().all(|c| c.is_ascii_hexdigit()),
            "fingerprint must be hex"
        );
    }

    #[test]
    fn append_dedups() {
        let list = vec!["a".to_string(), "b".to_string()];
        assert!(
            append_fingerprint(list, "a".to_string(), 64).is_none(),
            "duplicate should return None to skip the write"
        );
    }

    #[test]
    fn append_evicts_fifo_when_at_cap() {
        let list = vec!["1".to_string(), "2".to_string(), "3".to_string()];
        let result = append_fingerprint(list, "4".to_string(), 3).expect("change occurred");
        assert_eq!(
            result,
            vec!["2".to_string(), "3".to_string(), "4".to_string()],
            "oldest entry should be evicted to make room"
        );
    }

    #[test]
    fn append_evicts_multiple_when_above_cap() {
        // Simulates raising the cap or pre-existing oversized state
        let list: Vec<String> = (0..10).map(|i| i.to_string()).collect();
        let result = append_fingerprint(list, "new".to_string(), 3).expect("change occurred");
        assert_eq!(result.len(), 3);
        assert_eq!(result.last().unwrap(), "new");
        assert_eq!(result[0], "8");
    }

    #[test]
    fn append_evicts_at_actual_production_cap() {
        // Guards against off-by-one regressions in the eviction math at the
        // configured MAX_PROCESSED_INVITATIONS boundary.
        let mut list: Vec<String> = (0..MAX_PROCESSED_INVITATIONS)
            .map(|i| i.to_string())
            .collect();
        assert_eq!(list.len(), MAX_PROCESSED_INVITATIONS);
        list = append_fingerprint(list, "new".to_string(), MAX_PROCESSED_INVITATIONS)
            .expect("change occurred");
        assert_eq!(list.len(), MAX_PROCESSED_INVITATIONS);
        assert_eq!(list.last().unwrap(), "new");
        assert_eq!(list[0], "1", "oldest (\"0\") should have been evicted");
    }

    /// `Invitation::to_encoded_string` must round-trip byte-for-byte. The
    /// fingerprint dedup compares hashes of the canonical form, so two calls
    /// to `to_encoded_string` for the same `Invitation` (one at URL parse
    /// time, one at dismiss time) must produce the same bytes; otherwise
    /// `is_invitation_processed` will not recognize a previously-marked
    /// invitation across a page reload, defeating the fix.
    #[test]
    fn invitation_round_trip_is_byte_stable() {
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;
        use river_core::room_state::member::{AuthorizedMember, Member};

        let owner_sk = SigningKey::generate(&mut OsRng);
        let invitee_sk = SigningKey::generate(&mut OsRng);

        let member = Member {
            owner_member_id: owner_sk.verifying_key().into(),
            invited_by: owner_sk.verifying_key().into(),
            member_vk: invitee_sk.verifying_key(),
        };
        let authorized = AuthorizedMember::new(member, &owner_sk);

        let inv = Invitation {
            room: owner_sk.verifying_key(),
            invitee_signing_key: invitee_sk,
            invitee: authorized,
        };

        let first = inv.to_encoded_string();
        let second = inv.to_encoded_string();
        assert_eq!(
            first, second,
            "to_encoded_string must be deterministic across calls"
        );

        // Round-trip: encode -> decode -> encode must produce the same bytes.
        let decoded = Invitation::from_encoded_string(&first)
            .expect("our own encoded form must decode cleanly");
        let re_encoded = decoded.to_encoded_string();
        assert_eq!(
            first, re_encoded,
            "encode->decode->encode must be byte-stable; otherwise the fingerprint dedup breaks across reloads"
        );

        // And the fingerprints must match too.
        assert_eq!(
            invitation_fingerprint(&first),
            invitation_fingerprint(&re_encoded),
            "fingerprints of round-tripped encodings must be equal"
        );
    }
}
