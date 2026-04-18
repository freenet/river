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
use crate::components::members::member_info_modal::MemberInfoModal;
use crate::components::members::Invitation;
use crate::components::room_list::create_room_modal::CreateRoomModal;
use crate::components::room_list::edit_room_modal::EditRoomModal;
use crate::components::room_list::receive_invitation_modal::{
    is_invitation_processed, load_invitation_from_storage, mark_invitation_processed,
    save_invitation_to_storage, ReceiveInvitationModal,
};
use crate::invites::PendingInvites;
use crate::room_data::{CurrentRoom, Rooms};
use dioxus::document::{Link, Stylesheet};
use dioxus::logger::tracing::{debug, error, info};
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

    let mut receive_invitation = use_signal(|| None::<Invitation>);

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
    // The URL is also rewritten via `history.replaceState` to drop the
    // `?invitation=...` parameter, but that call may be blocked when River is
    // embedded in the gateway's sandboxed iframe (which has no
    // `allow-same-origin`). To stay correct when the URL cannot be cleaned,
    // we record a fingerprint of every invitation we have already shown the
    // user and skip it on subsequent loads. See issue #215.
    let mut found_invitation = false;
    if let Some(window) = window() {
        if let Ok(search) = window.location().search() {
            if let Ok(params) = web_sys::UrlSearchParams::new_with_str(&search) {
                if let Some(invitation_code) = params.get("invitation") {
                    let already_processed = is_invitation_processed(&invitation_code);
                    if already_processed {
                        info!("Skipping invitation in URL: already processed in this browser");
                    } else if let Ok(invitation) = Invitation::from_encoded_string(&invitation_code)
                    {
                        info!("Received invitation from URL: {:?}", invitation);
                        // Record the fingerprint immediately. The modal flow
                        // also calls `mark_invitation_processed` on Accept,
                        // but recording it here covers the case where the
                        // user reloads before clicking anything.
                        mark_invitation_processed(&invitation_code);
                        save_invitation_to_storage(&invitation);
                        receive_invitation.set(Some(invitation));
                        found_invitation = true;
                    }

                    // Best-effort: remove the invitation parameter from the
                    // URL. May fail in the sandboxed iframe; the processed
                    // fingerprint above is the authoritative guard.
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

    // Recover invitation from localStorage if not found in URL (e.g. after page reload)
    if !found_invitation {
        if let Some(invitation) = load_invitation_from_storage() {
            info!("Recovered pending invitation from localStorage");
            receive_invitation.set(Some(invitation));
        }
    }

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
