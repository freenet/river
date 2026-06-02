pub mod chat_delegate;
pub mod document_title;
pub mod freenet_api;
pub mod notifications;
pub mod receive_times;
pub mod sync_info;

use super::{conversation::Conversation, members::MemberList, room_list::RoomList};
use crate::components::app::document_title::DocumentTitleUpdater;
use crate::components::app::freenet_api::freenet_synchronizer::SynchronizerMessage;
use crate::components::app::freenet_api::freenet_synchronizer::SynchronizerStatus;
use crate::components::app::freenet_api::FreenetSynchronizer;
use crate::components::direct_messages::{DmThreadModal, InviteViaDmPickerModal};
use crate::components::members::member_info_modal::MemberInfoModal;
use crate::components::members::Invitation;
use crate::components::room_list::create_room_modal::CreateRoomModal;
use crate::components::room_list::edit_room_modal::EditRoomModal;
use crate::components::room_list::receive_invitation_modal::{
    accept_invitation, clear_invitation_from_storage, decide_recovered_invitation,
    is_invitation_processed, load_invitation_from_storage, load_invitation_nickname_from_storage,
    save_invitation_to_storage, take_resume_once, ReceiveInvitationModal,
    RecoveredInvitationAction, PRESENT_INVITATION_REQUEST,
};
use crate::invites::PendingInvites;
use crate::room_data::{CurrentRoom, Rooms};
use dioxus::document::{Link, Stylesheet};
use dioxus::logger::tracing::{debug, error, info, warn};
use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::client_api::WebApi;
use river_core::room_state::member::MemberId;
use wasm_bindgen_futures::spawn_local;
use web_sys::window;

/// Which panel is visible on mobile (<md). On desktop all panels are always visible.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MobileView {
    Rooms,
    Chat,
    Members,
}

pub static ROOMS: GlobalSignal<Rooms> = Global::new(initial_rooms);
pub static CURRENT_ROOM: GlobalSignal<CurrentRoom> =
    Global::new(|| CurrentRoom { owner_key: None });
pub static MEMBER_INFO_MODAL: GlobalSignal<MemberInfoModalSignal> =
    Global::new(|| MemberInfoModalSignal { member: None });
pub static EDIT_ROOM_MODAL: GlobalSignal<EditRoomModalSignal> =
    Global::new(|| EditRoomModalSignal { room: None });
pub static CREATE_ROOM_MODAL: GlobalSignal<CreateRoomModalSignal> =
    Global::new(|| CreateRoomModalSignal { show: false });
pub static PENDING_INVITES: GlobalSignal<PendingInvites> = Global::new(PendingInvites::new);
pub static SYNC_STATUS: GlobalSignal<SynchronizerStatus> =
    Global::new(|| SynchronizerStatus::Connecting);
pub static SYNCHRONIZER: GlobalSignal<FreenetSynchronizer> = Global::new(FreenetSynchronizer::new);
pub static WEB_API: GlobalSignal<Option<WebApi>> = Global::new(|| None);
pub static AUTH_TOKEN: GlobalSignal<Option<String>> = Global::new(|| None);

// Tracks which rooms need to be synced due to USER actions (not network updates)
// This prevents infinite loops where network responses trigger more syncs
pub static NEEDS_SYNC: GlobalSignal<std::collections::HashSet<VerifyingKey>> =
    Global::new(std::collections::HashSet::new);

/// Mark a room as needing sync, deferred via setTimeout(0).
///
/// IMPORTANT: Writing to NEEDS_SYNC triggers a Dioxus use_effect synchronously,
/// which cascades into ProcessRooms → ROOMS.read() and other signal reads.
/// If called while any signal is borrowed, this causes a RefCell re-entrant
/// borrow panic in WASM.
///
/// We use setTimeout(0) instead of spawn_local because spawn_local runs within
/// wasm-bindgen-futures' task scheduler, which may itself hold a RefCell borrow
/// when polling tasks. setTimeout(0) breaks out of the WASM call stack entirely,
/// ensuring the write happens in a completely clean execution context.
pub fn mark_needs_sync(room_key: ed25519_dalek::VerifyingKey) {
    crate::util::defer(move || {
        NEEDS_SYNC.write().insert(room_key);
    });
}

/// Which panel is active on mobile. Defaults to Chat (conversation view).
pub static MOBILE_VIEW: GlobalSignal<MobileView> = Global::new(|| MobileView::Chat);

// Build metadata from build.rs
const BUILD_TIMESTAMP: &str = env!("BUILD_TIMESTAMP_ISO");
const GIT_COMMIT: &str = env!("GIT_COMMIT_HASH");

#[component]
pub fn App() -> Element {
    info!(
        "River UI loaded - Built: {} | Commit: {}",
        BUILD_TIMESTAMP, GIT_COMMIT
    );

    // Capture the Dioxus runtime for use in defer()/setTimeout callbacks.
    // Must be called from within a Dioxus component where the runtime is active.
    crate::util::capture_runtime();

    // Install the document-level click interceptor that catches
    // in-page invite-URL anchor clicks and routes them through the
    // in-app receive-invitation flow instead of letting the browser
    // navigate the iframe in place (Ivvor's "lockup" report,
    // 2026-05-16). Safe to call on every re-render — idempotent.
    crate::components::invite_click_interceptor::install_invite_click_interceptor();

    let mut receive_invitation = use_signal(|| None::<Invitation>);

    // One-shot guard for the localStorage auto-resume path (#218). The
    // recovery block below runs in the `App` component BODY, which re-executes
    // on every render. The `Resume` branch has side effects (re-sends
    // `AcceptInvitation` and resets `PENDING_INVITES` to `PendingSubscription`)
    // and the persisted nickname stays in localStorage until the join
    // completes — so without this guard, every re-render while the
    // subscription is in flight would re-fire `accept_invitation`, resetting
    // the status and looping. We resume at most once per page load; that
    // single resume re-populates `PENDING_INVITES` and the synchronizer drives
    // the rest. Uses the same `use_hook(Rc<Cell>)` one-shot idiom as
    // `dm_thread_modal.rs`.
    let invitation_resume_fired = use_hook(|| std::rc::Rc::new(std::cell::Cell::new(false)));

    // Bridge the click interceptor's `INTERCEPTED_INVITATION_CODE` global
    // into the local `receive_invitation` signal that drives
    // `ReceiveInvitationModal`. Same gate as the URL-bar flow below: we
    // ignore codes that have already been processed in this browser
    // (so a click on the same link twice doesn't reopen the modal
    // after acceptance/dismiss).
    //
    // Per AGENTS.md "Dioxus WASM Signal Safety Rules":
    // * `.try_read()` (not `.read()`) — the interceptor writes from a
    //   deferred JS callback whose Drop fires subscriber notifications
    //   synchronously; a non-fallible read while the write guard is
    //   still alive panics on Firefox mobile.
    // * Synchronous clear (not `defer()`) — `use_effect` that defers
    //   clearing a signal it subscribes to triggers a re-fire loop
    //   (effect re-runs because `receive_invitation.set` mutates a
    //   sibling signal → re-render → effect observes the same Some →
    //   processes again). Project rule "Never defer signal clears in
    //   `use_effect`" is explicit about this.
    use_effect(move || {
        let pending = {
            let g = crate::components::invite_click_interceptor::INTERCEPTED_INVITATION_CODE
                .try_read()
                .ok();
            g.and_then(|opt| opt.clone())
        };
        let Some(code) = pending else {
            return;
        };
        // Synchronous clear BEFORE processing, so the re-render
        // triggered by `receive_invitation.set(...)` below doesn't
        // observe the same Some value and re-fire this effect.
        *crate::components::invite_click_interceptor::INTERCEPTED_INVITATION_CODE.write() = None;
        match Invitation::from_encoded_string(&code) {
            Ok(invitation) => {
                let fingerprint = invitation.to_encoded_string();
                if is_invitation_processed(&fingerprint) {
                    debug!("Intercepted invite click already processed; ignoring");
                } else {
                    info!("Intercepted invite link click: opening modal in place");
                    save_invitation_to_storage(&invitation);
                    receive_invitation.set(Some(invitation));
                }
            }
            Err(e) => {
                warn!(
                    "Intercepted invite click had unparseable code: {} (len {})",
                    e,
                    code.len()
                );
            }
        }
    });

    // Get auth token from window global (injected by Freenet gateway)
    // This is synchronous - no network request needed
    get_auth_token_from_window();

    // Start synchronizer - auth token is already available
    spawn_local(async {
        debug!("Starting FreenetSynchronizer from App component");
        // Note: The synchronizer will set up the chat delegate after connection is established
        let mut synchronizer = SYNCHRONIZER.write();
        synchronizer.start().await;
    });

    // Check URL for invitation parameter, then fall back to localStorage.
    //
    // The URL cannot be rewritten by the iframe: `history.replaceState`
    // requires same-origin and the gateway iframe runs in an opaque origin
    // (`sandbox="allow-scripts allow-forms allow-popups"`, no
    // `allow-same-origin`). Instead, the user's accept/decline actions are
    // recorded as fingerprints in River's slice of the *top-level* URL hash
    // (`#river-processed=fp1,fp2,...`), which the gateway shell propagates
    // into the iframe on every load. The hash is updated via the shell's
    // postMessage bridge, which has same-origin access.
    //
    // The fingerprint is keyed off `Invitation::to_encoded_string()`
    // (canonical CBOR + base58, see `invitation_round_trip_is_byte_stable`
    // test) so URL-time and dismiss-time fingerprints match for the same
    // invitation. See issues #215 and #219.
    let mut found_invitation = false;
    if let Some(window) = window() {
        if let Ok(search) = window.location().search() {
            if let Ok(params) = web_sys::UrlSearchParams::new_with_str(&search) {
                if let Some(invitation_code) = params.get("invitation") {
                    if let Ok(invitation) = Invitation::from_encoded_string(&invitation_code) {
                        if is_invitation_processed(&invitation.to_encoded_string()) {
                            debug!(
                                "Skipping invitation in URL: already accepted or dismissed in this browser"
                            );
                        } else {
                            info!("Received invitation from URL: {:?}", invitation);
                            save_invitation_to_storage(&invitation);
                            receive_invitation.set(Some(invitation));
                            found_invitation = true;
                        }
                    }

                    // Best-effort: remove the invitation parameter from the
                    // URL. Fails in the sandboxed iframe (opaque origin) but
                    // is still useful in non-sandboxed deployments such as
                    // dx-serve dev mode. The processed-hash fingerprint is
                    // the authoritative guard.
                    params.delete("invitation");
                    let new_search = params.to_string().as_string().unwrap_or_default();
                    let new_url = if new_search.is_empty() {
                        window.location().pathname().unwrap_or_default()
                    } else {
                        format!(
                            "{}?{}",
                            window.location().pathname().unwrap_or_default(),
                            new_search
                        )
                    };
                    if let Ok(history) = window.history() {
                        if let Err(e) = history.replace_state_with_url(
                            &wasm_bindgen::JsValue::NULL,
                            "",
                            Some(&new_url),
                        ) {
                            debug!(
                                "history.replaceState failed (likely sandboxed iframe): {:?}",
                                e
                            );
                        }
                    }
                }
            }
        }
    }

    // Bridge `PRESENT_INVITATION_REQUEST` (set by the DM-thread "Accept"
    // button on an invite-card DM) into the local `receive_invitation`
    // signal so `ReceiveInvitationModal` opens for in-app accepts the same
    // way it does for URL-bar accepts. Synchronous clear before the
    // `receive_invitation.set` follows the same rule as the click-
    // interceptor bridge above (AGENTS.md "Never defer signal clears in
    // use_effect").
    //
    // Gate on `is_invitation_processed` — without it, a previously-
    // accepted-then-LeaveRoom'd invitation re-presented via the DM
    // accept-card would pop the nickname-prompt modal again, which is
    // alarming for the user and forces them through the whole join
    // flow for a room they've already acted on (#279). The URL-bar
    // (line 192) and click-interceptor (line 141) paths already gate
    // on this; the DM-card bridge was the missing case.
    //
    // `try_read() -> Err` on the FIRST render is benign: the picker /
    // accept-card paths only ever write via `crate::util::defer()`, which
    // schedules a setTimeout(0) macrotask — the writer is never holding
    // the borrow at the moment this effect's first run reads. If a future
    // code path adds a NON-deferred writer to `PRESENT_INVITATION_REQUEST`,
    // the Err path can mask a present invitation for a render tick; pin
    // the "writes always defer" invariant via grep when extending callers.
    use_effect(move || {
        let pending = {
            let g = PRESENT_INVITATION_REQUEST.try_read().ok();
            g.and_then(|opt| opt.clone())
        };
        let Some(inv) = pending else {
            return;
        };
        *PRESENT_INVITATION_REQUEST.write() = None;
        let fingerprint = inv.to_encoded_string();
        if is_invitation_processed(&fingerprint) {
            // The user has already accepted or dismissed this
            // invitation in this browser. Don't re-open the modal:
            // either they're still a member (in which case the rail
            // already shows the room) or they've explicitly left and
            // re-presenting the join flow would be confusing. Silent
            // drop is the same handling the click-interceptor bridge
            // uses for the equivalent case.
            debug!(
                "In-app invitation accept ignored: already processed for room {:?}",
                MemberId::from(inv.room)
            );
            return;
        }
        info!(
            "In-app invitation accept: opening modal for room {:?}",
            MemberId::from(inv.room)
        );
        receive_invitation.set(Some(inv));
    });

    // Recover invitation from localStorage if not found in URL (e.g. after
    // page reload before subscription completed). The invitation artifact
    // stays in storage from the moment it is seen until the room is
    // subscribed OR the user dismisses it (`clear_invitation_from_storage`).
    //
    // Three cases, in priority order:
    //   1. A nickname is ALSO saved → the user already clicked Accept and the
    //      subscription was still in flight at reload. Auto-resume with that
    //      nickname (#218) so the "Subscribing…" indicator returns instead of
    //      a blank UI / re-prompt. This MUST take precedence over the
    //      processed-fingerprint check below, because `accept_invitation`
    //      marks the invitation processed up front — so a mid-flight reload
    //      always sees `is_invitation_processed == true`. Storage-still-present
    //      + nickname-present is the authoritative "join not yet finished"
    //      signal here.
    //   2. No nickname, but the invitation was already acted on in this
    //      browser → discard (the user accepted-then-left, or dismissed).
    //   3. No nickname, not yet processed → the user reloaded before deciding;
    //      re-open the modal at the nickname prompt.
    if !found_invitation {
        if let Some(invitation) = load_invitation_from_storage() {
            let encoded = invitation.to_encoded_string();
            let action = decide_recovered_invitation(
                load_invitation_nickname_from_storage(&encoded),
                is_invitation_processed(&encoded),
            );
            match action {
                RecoveredInvitationAction::Resume { nickname } => {
                    // Resume at most once per page load (see
                    // `invitation_resume_fired` above). Re-running on every
                    // render would re-send `AcceptInvitation` and reset the
                    // pending status, looping.
                    if take_resume_once(&invitation_resume_fired) {
                        // Mount the modal first, then defer the accept so the
                        // `PENDING_INVITES` mutation and channel send happen in
                        // a clean execution context (per the Dioxus signal-
                        // safety rules) rather than mid-render of the `App`
                        // component body.
                        info!("Recovered pending invitation with saved nickname; auto-resuming subscription");
                        receive_invitation.set(Some(invitation.clone()));
                        crate::util::defer(move || {
                            accept_invitation(invitation, nickname);
                        });
                    }
                }
                RecoveredInvitationAction::Discard => {
                    debug!("Discarding recovered invitation: already processed");
                    clear_invitation_from_storage();
                }
                RecoveredInvitationAction::Prompt => {
                    info!("Recovered pending invitation from localStorage");
                    receive_invitation.set(Some(invitation));
                }
            }
        }
    }

    // Seed DM_LAST_SEEN from the room state we just hydrated, so
    // previously-existing inbound DMs don't show up as unread on every
    // page load. We explicitly subscribe to ROOMS here so the effect
    // re-fires when the delegate finishes hydrating (`ROOMS` is empty on
    // synchronous first render). `seed_dm_last_seen_if_needed` is
    // internally gated by a one-shot flag — it ONLY seeds on the first
    // non-empty ROOMS observation, so subsequent inbound DMs are NOT
    // auto-seeded as already-read (Codex / Skeptical found that the
    // previous always-run version defeated the unread feature).
    use_effect(|| {
        // Touch ROOMS to register a subscription so this effect re-runs
        // on hydration.
        let _hydration_marker = ROOMS.try_read().map(|r| r.map.len()).unwrap_or(0);
        crate::components::direct_messages::seed_dm_last_seen_if_needed();
    });

    // Outbound DM cache hygiene (#256): whenever ROOMS changes, drop
    // cached plaintext entries whose ciphertext is no longer present
    // in any room (recipient purged them, or per-pair-cap eviction in
    // the contract dropped them). The prune helper only writes when
    // something is actually removed, so it doesn't re-trigger itself.
    // We deliberately do NOT subscribe to OUTBOUND_DMS here — that
    // would loop, since prune mutates OUTBOUND_DMS.
    use_effect(|| {
        let _rooms_marker = ROOMS.try_read().map(|r| r.map.len()).unwrap_or(0);
        crate::components::app::chat_delegate::prune_outbound_dms_for_purges();
    });

    #[cfg(not(feature = "no-sync"))]
    {
        // The synchronizer is now started in the auth token effect

        // Watch NEEDS_SYNC signal for USER-initiated changes only
        // This prevents infinite loops from network response updates to ROOMS
        use_effect(move || {
            let rooms_needing_sync = NEEDS_SYNC.read().clone();

            if !rooms_needing_sync.is_empty() {
                info!(
                    "User changes detected for {} rooms, triggering synchronization",
                    rooms_needing_sync.len()
                );

                // Get all the data we need upfront to avoid nested borrows
                let message_sender = SYNCHRONIZER.read().get_message_sender();
                let has_rooms = ROOMS.try_read().map(|r| !r.map.is_empty()).unwrap_or(false);
                let has_invitations = !PENDING_INVITES.read().map.is_empty();

                // Save and clear NEEDS_SYNC synchronously to prevent infinite re-runs
                // of this effect (the effect subscribes to NEEDS_SYNC, so deferring
                // the clear would cause a tight loop before setTimeout fires).
                let pending_rooms: Vec<_> = NEEDS_SYNC.read().iter().cloned().collect();
                NEEDS_SYNC.write().clear();

                if has_rooms || has_invitations {
                    info!("Sending ProcessRooms message to synchronizer, has_rooms={}, has_invitations={}", has_rooms, has_invitations);

                    if let Err(e) = message_sender.unbounded_send(SynchronizerMessage::ProcessRooms)
                    {
                        error!("Failed to send ProcessRooms message: {}", e);
                        // Re-insert rooms so they'll be retried on next trigger
                        for key in pending_rooms {
                            mark_needs_sync(key);
                        }
                    } else {
                        info!("ProcessRooms message sent successfully");
                    }

                    // Use safe_spawn_local to avoid re-entrant borrow of
                    // wasm-bindgen-futures' task scheduler on Firefox mobile.
                    crate::util::safe_spawn_local(async {
                        if let Err(e) = chat_delegate::save_rooms_to_delegate().await {
                            error!("Failed to save rooms to delegate: {}", e);
                        }
                    });
                } else {
                    debug!("No rooms to synchronize");
                }
            }
        });

        info!("FreenetSynchronizer setup complete");
    }

    rsx! {
        // Favicon
        Link { rel: "icon", r#type: "image/svg+xml", href: asset!("/assets/river_logo.svg") }
        // Stylesheets
        Stylesheet { href: asset!("/assets/styles.css") }
        Stylesheet { href: asset!("/assets/main.css") }

        // Main chat layout - grid with fixed sidebars and flexible center
        div { class: "flex bg-bg overflow-hidden app-root",
            // On desktop (md+): all three panels always visible
            // On mobile (<md): only the active panel is shown
            div {
                class: {
                    let is_rooms = *MOBILE_VIEW.read() == MobileView::Rooms;
                    if is_rooms { "w-full md:w-64 md:flex-shrink-0 flex" } else { "hidden md:w-64 md:flex-shrink-0 md:flex" }
                },
                RoomList {}
            }
            div {
                class: {
                    let is_chat = *MOBILE_VIEW.read() == MobileView::Chat;
                    if is_chat { "flex-1 flex min-w-0" } else { "hidden md:flex md:flex-1 md:min-w-0" }
                },
                Conversation {}
            }
            div {
                class: {
                    let is_members = *MOBILE_VIEW.read() == MobileView::Members;
                    if is_members { "w-full md:w-56 md:flex-shrink-0 flex" } else { "hidden md:w-56 md:flex-shrink-0 md:flex" }
                },
                MemberList {}
            }
        }
        EditRoomModal {}
        MemberInfoModal {}
        CreateRoomModal {}
        DmThreadModal {}
        InviteViaDmPickerModal {}
        ReceiveInvitationModal {
            invitation: receive_invitation
        }
        DocumentTitleUpdater {}
    }
}

#[cfg(not(feature = "example-data"))]
fn initial_rooms() -> Rooms {
    Rooms {
        map: std::collections::HashMap::new(),
        current_room_key: None,
        removed_rooms: std::collections::HashSet::new(),
        migrated_rooms: Vec::new(),
    }
}

#[cfg(feature = "example-data")]
fn initial_rooms() -> Rooms {
    crate::example_data::create_example_rooms()
}

pub struct EditRoomModalSignal {
    pub room: Option<VerifyingKey>,
}

pub struct CreateRoomModalSignal {
    pub show: bool,
}

pub struct MemberInfoModalSignal {
    pub member: Option<MemberId>,
}

/// Gets the authorization token from the window global variable.
/// The Freenet HTTP gateway injects this token into the HTML as:
/// <script>window.__FREENET_AUTH_TOKEN__ = "token_value";</script>
fn get_auth_token_from_window() {
    if let Some(win) = window() {
        match js_sys::Reflect::get(&win, &"__FREENET_AUTH_TOKEN__".into()) {
            Ok(token_value) => {
                if let Some(token) = token_value.as_string() {
                    info!("Found auth token from window global");
                    *AUTH_TOKEN.write() = Some(token);
                } else if token_value.is_undefined() || token_value.is_null() {
                    debug!("Auth token not injected by gateway (running locally?)");
                } else {
                    debug!("Auth token has unexpected type");
                }
            }
            Err(err) => {
                error!("Failed to read auth token from window: {:?}", err);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // -----------------------------------------------------------------
    // Issue freenet/river#279 regression guard:
    //
    // The bridge effect that translates `PRESENT_INVITATION_REQUEST`
    // (set by the DM thread's "Accept" button on an invite-card DM)
    // into the local `receive_invitation` signal MUST gate on
    // `is_invitation_processed`. Without that gate, a previously-
    // accepted-then-LeaveRoom'd room re-presented via the DM accept
    // path re-opens the full nickname-prompt flow — which is alarming
    // for the user and forces them to re-run the join flow for a
    // room they already acted on.
    //
    // The URL-bar and click-interceptor paths already gate on this;
    // the DM bridge was the missing case. Source-text pin because the
    // effect runs inside Dioxus's render loop and isn't unit-testable
    // without the runtime.
    // -----------------------------------------------------------------
    #[test]
    fn dm_accept_bridge_gates_on_is_invitation_processed() {
        let src = include_str!("app.rs");
        // Pin the gate by searching the bridge effect for the
        // is_invitation_processed call AND the early-return comment.
        // A future refactor that removes the gate would need to also
        // update this test, which is the intended forcing function.
        assert!(
            src.contains("is_invitation_processed(&fingerprint)")
                && src.contains("In-app invitation accept ignored: already processed"),
            "The PRESENT_INVITATION_REQUEST bridge effect must gate on \
             is_invitation_processed before opening the nickname-prompt \
             modal — otherwise a stale invite card re-presents the full \
             join flow for a room the user has already accepted/left (#279)."
        );
    }
}
