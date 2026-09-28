use crate::components::app::{CURRENT_ROOM, ROOMS};
use crate::components::members::{collect_invitation_secrets, Invitation};
use crate::room_data::RoomData;
use crate::util::ecies::unseal_bytes_with_secrets;
use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::{FaArrowsRotate, FaCopy, FaXmark};
use dioxus_free_icons::Icon;
use ed25519_dalek::SigningKey;
use river_core::room_state::member::{AuthorizedMember, Member};

/// Fallback URL for non-browser environments or when `window.location` is
/// unavailable. This is ONLY reached off the browser (native/test builds) or
/// if `web_sys::window()` returns `None` — i.e. never on a real gateway, where
/// the URL is always derived from `window.location`. It deliberately does NOT
/// embed the production contract ID: doing so baked that ID into the UI WASM,
/// and the test-publish flow then either had to corrupt the WASM to retarget it
/// (freenet/river#257) or shipped a test webapp whose invitation fallback
/// pointed at the production contract. The `CONTRACT_ID` placeholder makes the
/// non-functional fallback obvious and keeps the WASM contract-ID-agnostic.
const FALLBACK_BASE_URL: &str = "http://127.0.0.1:7509/v1/contract/web/CONTRACT_ID/";

/// Get the base URL for invitation links.
/// Derives from the current window.location so invitations work on any host/port.
/// `pub(crate)` so the `InviteViaDmPickerModal` (#252) can produce the
/// same URL shape — must match this one byte-for-byte so recipients land
/// on the right gateway.
pub(crate) fn get_invitation_base_url() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(window) = web_sys::window() {
            // Get the current URL's origin (protocol + host + port) and pathname
            let location = window.location();
            let href = location.href().unwrap_or_default();
            // Remove any query string or fragment, keep the base path
            if let Some(pos) = href.find('?') {
                href[..pos].to_string()
            } else if let Some(pos) = href.find('#') {
                href[..pos].to_string()
            } else {
                href
            }
        } else {
            FALLBACK_BASE_URL.to_string()
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        FALLBACK_BASE_URL.to_string()
    }
}

async fn create_invitation(room_data: Option<RoomData>) -> Result<Invitation, String> {
    let Some(room_data) = room_data else {
        return Err("No room selected".to_string());
    };
    // Issuing an invitation signs the invitee's `Member` record
    // with the inviter's key, so this whole path needs the
    // private half. Surface it as a normal resource error (the
    // modal already renders `Err(String)`) rather than panicking.
    let Some(self_sk) = room_data.signing_key().cloned() else {
        return Err(
            "The local signing key for this room is unavailable, so an invitation cannot be created."
                .to_string(),
        );
    };
    // Generate new signing key for invitee
    let invitee_signing_key = SigningKey::generate(&mut rand::thread_rng());
    let invitee_verifying_key = invitee_signing_key.verifying_key();

    // Create member struct
    let member = Member {
        owner_member_id: room_data.owner_vk.into(),
        invited_by: self_sk.verifying_key().into(),
        member_vk: invitee_verifying_key,
    };

    // Serialize member to CBOR for signing
    let mut member_bytes = Vec::new();
    ciborium::ser::into_writer(&member, &mut member_bytes)
        .map_err(|e| format!("Failed to serialize member: {}", e))?;

    // Sign using delegate with fallback to local signing
    let signature =
        crate::signing::sign_member_with_fallback(room_data.room_key(), member_bytes, &self_sk)
            .await;

    // Create authorized member with pre-computed signature
    let authorized_member = AuthorizedMember::with_signature(member, signature);

    // For a private room, embed the room secrets the inviter
    // holds so the invitee can decrypt the room immediately on
    // join, without waiting for the owner delegate's
    // `encrypted_secrets` back-fill. Empty for a public room,
    // or if the inviter holds no secret yet (then the invitee
    // falls back to that wait).
    let room_secrets = if room_data.is_private() {
        collect_invitation_secrets(&room_data.secrets)
    } else {
        Vec::new()
    };

    // Create invitation
    let invitation = Invitation {
        room: room_data.owner_vk,
        invitee_signing_key,
        invitee: authorized_member,
        room_secrets,
    };

    Ok(invitation)
}

#[component]
pub fn InviteMemberModal(is_active: Signal<bool>) -> Element {
    let current_room_data_signal: Memo<Option<RoomData>> = use_memo(move || {
        // freenet/river#555: anchor before the fallible ROOMS read.
        crate::util::signal_guard::anchor();
        CURRENT_ROOM
            .read()
            .owner_key
            .as_ref()
            .and_then(|key| match ROOMS.try_read() {
                Ok(rooms) => rooms.map.get(key).cloned(),
                Err(_) => {
                    crate::util::signal_guard::schedule_nudge();
                    None
                }
            })
    });

    if !*is_active.read() {
        return rsx! {};
    }

    rsx! {
        // Dimmer and centring wrapper are one element, so a click outside the card hits this handler.
        // Same stacking context (.app-root) and z-index as the composer, so DOM order puts it on top.
        div {
            "data-testid": "invite-member-backdrop",
            class: "fixed inset-0 bg-black/50 z-50 flex items-center justify-center p-4",
            onclick: move |_| is_active.set(false),
            div {
                "data-testid": "invite-member-modal",
                class: "bg-panel rounded-xl shadow-xl max-w-lg w-full max-h-[90vh] overflow-y-auto",
                onclick: move |e| e.stop_propagation(),

                // Header
                div { class: "px-6 py-4 border-b border-border flex items-center justify-between",
                    h2 { class: "text-lg font-semibold text-text", "Invite Member" }
                    button {
                        "data-testid": "invite-member-close-button",
                        class: "p-1 text-text-muted hover:text-text transition-colors",
                        onclick: move |_| is_active.set(false),
                        Icon { icon: FaXmark, width: 14, height: 14 }
                    }
                }

                // Body. Mounted only while the modal is open, so every open
                // starts a fresh invitation from nothing.
                div { class: "px-6 py-4",
                    InviteMemberBody { is_active, room: current_room_data_signal }
                }
            }
        }
    }
}

#[component]
fn InviteMemberBody(is_active: Signal<bool>, room: Memo<Option<RoomData>>) -> Element {
    // Subscribe to the room only while it is unknown: a transient `None` retries,
    // and later room updates never re-mint the link.
    let mut invitation_future = use_resource(move || async move {
        let known = room.peek().clone();
        let room_data = if known.is_some() { known } else { room() };
        create_invitation(room_data).await
    });

    match &*invitation_future.read_unchecked() {
        Some(Ok(invitation)) => {
            let room_name = room()
                .map(|r| {
                    let sealed_name = &r.room_state.configuration.configuration.display.name;
                    match unseal_bytes_with_secrets(sealed_name, &r.secrets) {
                        Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                        Err(_) => sealed_name.to_string_lossy(),
                    }
                })
                .unwrap_or_else(|| "this chat room".to_string());

            let invite_code = invitation.to_encoded_string();
            let base_url = get_invitation_base_url();
            let invite_url = format!("{}?invitation={}", base_url, invite_code);

            let default_msg = format!(
                "You've been invited to join the chat room \"{}\"!\n\n\
                To join:\n\
                1. Install Freenet from https://freenet.org\n\
                2. Open this link:\n\
                {}\n\n\
                IMPORTANT: This invitation is for you only — do not share it with anyone else. \
                It contains a unique identity key. If someone else uses this link, you will both \
                share the same identity and neither will work correctly.",
                room_name, invite_url
            );

            rsx! {
                // Recommended path first: ask, then send the
                // invitation as a DM via the built-in
                // "Share invite" flow (#252, #457). It hands
                // the recipient a one-click Accept card and
                // never puts a bearer credential through an
                // outside channel, so it belongs ABOVE the
                // link/code it is meant to displace.
                div {
                    "data-testid": "invite-dm-recommendation",
                    class: "mb-4 p-3 bg-accent-soft border-l-4 border-accent rounded-r-lg",
                    p { class: "text-sm text-text",
                        span { class: "font-medium", "Best way to invite someone: send the invitation in a DM. " }
                        "Ask whether they'd like to join first. Then, in any room you already share with them, click their name in the member list, choose "
                        span { class: "font-medium", "Share invite" }
                        ", and pick this room. River drops an invitation card into your DM thread that they accept in one click — nothing to copy, paste, or leak."
                    }
                }

                // Fallback path (no shared room yet): the
                // link/code below are bearer credentials — a
                // single reusable identity — hence the
                // one-person-only warning.
                div {
                    "data-testid": "invite-share-warning",
                    class: "mb-4 p-3 bg-warning-bg border-l-4 border-yellow-500 rounded-r-lg",
                    p { class: "text-sm text-text",
                        span { class: "font-medium", "No DM yet? Share the link or code below privately, with one person only. " }
                        "Each invitation creates a unique identity and is good for exactly one person. If two people use the same link or code, they share one identity and neither works correctly. Click "
                        span { class: "font-medium", "New Invitation" }
                        " for every additional person."
                    }
                }

                InvitationContent {
                    invitation_text: default_msg,
                    invitation_url: invite_url,
                    invitation_code: invite_code,
                    is_active,
                    // Clear first, so the old link can't be copied while the new one is signed.
                    on_new_invitation: move |_| {
                        invitation_future.clear();
                        invitation_future.restart();
                    }
                }
            }
        }
        Some(Err(err)) => {
            rsx! {
                div { class: "text-center py-8",
                    p { class: "text-red-500 mb-4", "{err}" }
                    button {
                        class: "px-4 py-2 bg-surface hover:bg-surface-hover text-text rounded-lg transition-colors",
                        onclick: move |_| {
                            invitation_future.clear();
                            invitation_future.restart();
                        },
                        "Try Again"
                    }
                }
            }
        }
        None => {
            rsx! {
                div { class: "text-center py-8",
                    div { class: "w-8 h-8 border-2 border-accent border-t-transparent rounded-full animate-spin mx-auto mb-4" }
                    p { class: "text-text-muted", "Generating invitation..." }
                }
            }
        }
    }
}

#[component]
fn InvitationContent(
    invitation_text: String,
    invitation_url: String,
    invitation_code: String,
    is_active: Signal<bool>,
    on_new_invitation: EventHandler<()>,
) -> Element {
    let mut copy_msg_text = use_signal(|| "Copy Message".to_string());
    let mut copy_link_text = use_signal(|| "Copy Link".to_string());
    let mut copy_code_text = use_signal(|| "Copy Code".to_string());

    // Clone the texts for use in the closures
    let invitation_text_for_clipboard = invitation_text.clone();
    let invitation_url_for_clipboard = invitation_url.clone();
    let invitation_code_for_clipboard = invitation_code.clone();

    let copy_message_to_clipboard = move |_| {
        crate::util::copy_to_clipboard(&invitation_text_for_clipboard);
        copy_msg_text.set("Copied!".to_string());
        copy_link_text.set("Copy Link".to_string());
        copy_code_text.set("Copy Code".to_string());
    };

    let copy_link_to_clipboard = {
        move |_| {
            crate::util::copy_to_clipboard(&invitation_url_for_clipboard);
            copy_link_text.set("Copied!".to_string());
            copy_msg_text.set("Copy Message".to_string());
            copy_code_text.set("Copy Code".to_string());
        }
    };

    let copy_code_to_clipboard = {
        move |_| {
            crate::util::copy_to_clipboard(&invitation_code_for_clipboard);
            copy_code_text.set("Copied!".to_string());
            copy_link_text.set("Copy Link".to_string());
            copy_msg_text.set("Copy Message".to_string());
        }
    };

    rsx! {
        // Link section
        div { class: "mb-4",
            label { class: "block text-sm font-medium text-text mb-1", "Invitation link:" }
            div { class: "flex gap-2",
                input {
                    "data-testid": "invite-link-input",
                    class: "flex-1 px-3 py-2 bg-surface border border-border rounded-lg text-sm text-text font-mono truncate",
                    r#type: "text",
                    value: invitation_url,
                    readonly: true
                }
                button {
                    "data-testid": "invite-copy-link-button",
                    class: "px-3 py-2 bg-accent hover:bg-accent-hover text-white text-sm rounded-lg transition-colors flex items-center gap-2",
                    onclick: copy_link_to_clipboard,
                    Icon { icon: FaCopy, width: 14, height: 14 }
                    span { "{copy_link_text}" }
                }
            }
        }

        // Portable invite-code section. The link above bakes in the current
        // host (e.g. a localhost node or the production gateway); a user on a
        // different host (try.freenet.org, another peer) would otherwise have
        // to hand-edit the host out of the link. This bare code is
        // host-independent — the recipient pastes it into River's
        // "Enter Invite Code" box on ANY host/peer (freenet/river#381). It is
        // the exact same bearer credential the link carries in its
        // `?invitation=` parameter, so it is no more sensitive to share than
        // the link, and the same "share privately with one person" warning
        // above applies.
        div { class: "mb-4",
            label { class: "block text-sm font-medium text-text mb-1", "Portable invite code:" }
            p { class: "text-xs text-text-muted mb-1",
                "Works on any host or peer. The recipient pastes this into River's \u{201c}Enter Invite Code\u{201d} box."
            }
            div { class: "flex gap-2",
                input {
                    "data-testid": "invite-code-input",
                    class: "flex-1 px-3 py-2 bg-surface border border-border rounded-lg text-sm text-text font-mono truncate",
                    r#type: "text",
                    value: invitation_code,
                    readonly: true
                }
                button {
                    "data-testid": "invite-copy-code-button",
                    class: "px-3 py-2 bg-accent hover:bg-accent-hover text-white text-sm rounded-lg transition-colors flex items-center gap-2",
                    onclick: copy_code_to_clipboard,
                    Icon { icon: FaCopy, width: 14, height: 14 }
                    span { "{copy_code_text}" }
                }
            }
        }

        // Full message section
        div { class: "mb-4",
            label { class: "block text-sm font-medium text-text mb-1", "Full invitation message:" }
            div {
                class: "p-3 bg-surface rounded-lg text-xs text-text font-mono whitespace-pre-wrap max-h-40 overflow-y-auto",
                "{invitation_text}"
            }
        }

        // Action buttons
        div { class: "flex flex-wrap gap-2",
            button {
                "data-testid": "invite-copy-message-button",
                class: "px-4 py-2 bg-accent hover:bg-accent-hover text-white text-sm font-medium rounded-lg transition-colors flex items-center gap-2",
                onclick: copy_message_to_clipboard,
                Icon { icon: FaCopy, width: 14, height: 14 }
                span { "{copy_msg_text}" }
            }
            button {
                "data-testid": "invite-new-invitation-button",
                class: "px-4 py-2 bg-surface hover:bg-surface-hover text-text text-sm rounded-lg transition-colors flex items-center gap-2",
                onclick: move |_| {
                    copy_msg_text.set("Copy Message".to_string());
                    copy_link_text.set("Copy Link".to_string());
                    copy_code_text.set("Copy Code".to_string());
                    on_new_invitation.call(());
                },
                Icon { icon: FaArrowsRotate, width: 14, height: 14 }
                span { "New Invitation" }
            }
            button {
                "data-testid": "invite-member-close-footer-button",
                class: "px-4 py-2 text-text-muted hover:text-text text-sm rounded-lg transition-colors",
                onclick: move |_| is_active.set(false),
                "Close"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::create_invitation;
    use futures::executor::block_on;

    // Refused before signing (which reads the `WEB_API` global), so no Dioxus runtime is needed.
    #[test]
    fn a_room_without_a_local_key_is_refused_before_signing() {
        let owner = ed25519_dalek::SigningKey::from_bytes(&[3; 32]).verifying_key();
        let mut room = crate::room_data::test_minimal_room_data(owner);
        room.self_sk = None;
        let refused = block_on(create_invitation(Some(room))).err();
        assert!(
            refused
                .as_deref()
                .is_some_and(|e| e.contains("local signing key for this room is unavailable")),
            "got {refused:?}"
        );
    }
}
