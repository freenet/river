pub mod chat_delegate;
pub mod freenet_api;
pub mod sync_info;

use super::{conversation::Conversation, members::MemberList, room_list::RoomList};
use crate::components::app::chat_delegate::set_up_chat_delegate;
use crate::components::app::freenet_api::freenet_synchronizer::SynchronizerMessage;
use crate::components::app::freenet_api::freenet_synchronizer::SynchronizerStatus;
use crate::components::app::freenet_api::FreenetSynchronizer;
use crate::components::members::member_info_modal::MemberInfoModal;
use crate::components::members::Invitation;
use crate::components::room_list::create_room_modal::CreateRoomModal;
use crate::components::room_list::edit_room_modal::EditRoomModal;
use crate::components::room_list::receive_invitation_modal::ReceiveInvitationModal;
use crate::invites::PendingInvites;
use crate::room_data::{CurrentRoom, Rooms};
use dioxus::logger::tracing::{debug, error, info};
use dioxus::prelude::*;
use document::Stylesheet;
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::client_api::WebApi;
use river_core::room_state::member::MemberId;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use wasm_bindgen_futures::JsFuture;
use web_sys::{window, Response};

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

#[component]
pub fn App() -> Element {
    info!("Loaded App component");

    let mut receive_invitation = use_signal(|| None::<Invitation>);

    // Read authorization header on mount and store in global
    //  use_effect(|| {
    spawn_local(async {
        // First, try to get the auth token
        fetch_auth_token().await;

        // Now that we've tried to get the auth token, start the synchronizer
        debug!("Starting FreenetSynchronizer from App component");

        // Start the synchronizer directly
        {
            let mut synchronizer = SYNCHRONIZER.write();
            synchronizer.start().await;
        }

        let _ = set_up_chat_delegate().await;
    });
    //  });

    // Check URL for invitation parameter
    if let Some(window) = window() {
        if let Ok(search) = window.location().search() {
            if let Ok(params) = web_sys::UrlSearchParams::new_with_str(&search) {
                if let Some(invitation_code) = params.get("invitation") {
                    if let Ok(invitation) = Invitation::from_encoded_string(&invitation_code) {
                        info!("Received invitation: {:?}", invitation);
                        receive_invitation.set(Some(invitation));
                    }
                }
            }
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
                let has_rooms = !ROOMS.read().map.is_empty();
                let has_invitations = !PENDING_INVITES.read().map.is_empty();

                if has_rooms || has_invitations {
                    info!("Sending ProcessRooms message to synchronizer, has_rooms={}, has_invitations={}", has_rooms, has_invitations);

                    if let Err(e) = message_sender.unbounded_send(SynchronizerMessage::ProcessRooms)
                    {
                        error!("Failed to send ProcessRooms message: {}", e);
                    } else {
                        info!("ProcessRooms message sent successfully");

                        // Clear the sync queue after successfully sending message
                        NEEDS_SYNC.write().clear();
                    }

                    // Also save rooms to delegate when they change
                    // Use spawn_local to avoid blocking the UI thread
                    spawn_local(async {
                        if let Err(e) = chat_delegate::save_rooms_to_delegate().await {
                            error!("Failed to save rooms to delegate: {}", e);
                        }
                    });
                } else {
                    debug!("No rooms to synchronize");
                    // Clear the queue even if there's nothing to sync
                    NEEDS_SYNC.write().clear();
                }
            }
        });

        info!("FreenetSynchronizer setup complete");
    }

    rsx! {
        Stylesheet { href: asset!("./assets/bulma.min.css") }
        Stylesheet { href: asset!("./assets/main.css") }
        Stylesheet { href: asset!("./assets/fontawesome/css/all.min.css") }

        // Status indicator for Freenet connection
        div {
            class: match &*SYNC_STATUS.read() {
                SynchronizerStatus::Connected => "connection-status connected",
                SynchronizerStatus::Connecting => "connection-status connecting",
                SynchronizerStatus::Disconnected => "connection-status disconnected",
                SynchronizerStatus::Error(_) => "connection-status error",
            },
            div { class: "status-icon" }
            div { class: "status-text",
                {
                    match &*SYNC_STATUS.read() {
                        SynchronizerStatus::Connected => "Connected".to_string(),
                        SynchronizerStatus::Connecting => "Connecting...".to_string(),
                        SynchronizerStatus::Disconnected => "Disconnected".to_string(),
                        SynchronizerStatus::Error(ref msg) => format!("Error: {}", msg),
                    }
                }
            }
        }

        // No longer needed - using the invite button in the members list instead

        div { class: "chat-container",
            RoomList {}
            Conversation {}
            MemberList {}
        }
        EditRoomModal {}
        MemberInfoModal {}
        CreateRoomModal {}
        ReceiveInvitationModal {
            invitation: receive_invitation
        }
    }
}

#[cfg(not(feature = "example-data"))]
fn initial_rooms() -> Rooms {
    Rooms {
        map: std::collections::HashMap::new(),
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

/// Fetches the authorization token from the current page's headers
/// and stores it in the AUTH_TOKEN global signal
async fn fetch_auth_token() {
    if let Some(win) = window() {
        let href = win.location().href().unwrap_or_default();

        match JsFuture::from(win.fetch_with_str(&href)).await {
            Ok(resp_value) => {
                if let Ok(resp) = resp_value.dyn_into::<Response>() {
                    if let Ok(Some(token)) = resp.headers().get("authorization") {
                        info!("Found auth token: {}", token);

                        // Extract the token part without the "Bearer" prefix
                        if token.starts_with("Bearer ") {
                            let token_part = token.trim_start_matches("Bearer ").trim();
                            *AUTH_TOKEN.write() = Some(token_part.to_string());
                            debug!("Stored token value: {}", token_part);
                        } else {
                            // If it doesn't have the expected format, store as-is
                            *AUTH_TOKEN.write() = Some(token);
                        }
                    } else {
                        debug!("Authorization header missing or not exposed");
                    }
                }
            }
            Err(err) => {
                error!("Failed to fetch page for auth header: {:?}", err);
            }
        }
    }
}
