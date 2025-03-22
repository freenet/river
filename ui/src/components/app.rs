pub mod freenet_api;
pub mod sync_info;
pub mod chat_delegate;

use super::{conversation::Conversation, members::MemberList, room_list::RoomList};
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
use river_common::room_state::member::MemberId;
use river_common::ChatRoomStateV1;
use std::collections::HashMap;
use wasm_bindgen_futures::spawn_local;
use web_sys::window;
use crate::components::app::chat_delegate::set_up_chat_delegate;

pub static ROOMS: GlobalSignal<Rooms> = Global::new(initial_rooms);
pub static CURRENT_ROOM: GlobalSignal<CurrentRoom> =
    Global::new(|| CurrentRoom { owner_key: None });
pub static MEMBER_INFO_MODAL: GlobalSignal<MemberInfoModalSignal> =
    Global::new(|| MemberInfoModalSignal { member: None });
pub static EDIT_ROOM_MODAL: GlobalSignal<EditRoomModalSignal> =
    Global::new(|| EditRoomModalSignal { room: None });
pub static CREATE_ROOM_MODAL: GlobalSignal<CreateRoomModalSignal> =
    Global::new(|| CreateRoomModalSignal { show: false });
pub static PENDING_INVITES: GlobalSignal<PendingInvites> = Global::new(|| PendingInvites::new());
pub static SYNC_STATUS: GlobalSignal<SynchronizerStatus> =
    Global::new(|| SynchronizerStatus::Connecting);
pub static SYNCHRONIZER: GlobalSignal<FreenetSynchronizer> =
    Global::new(|| FreenetSynchronizer::new());
pub static WEB_API: GlobalSignal<Option<WebApi>> = Global::new(|| None);

#[component]
pub fn App() -> Element {
    info!("Loaded River App component");

    let mut receive_invitation = use_signal(|| None::<Invitation>);

    // Check URL for invitation parameter
    if let Some(window) = window() {
        if let Ok(search) = window.location().search() {
            if let Some(params) = web_sys::UrlSearchParams::new_with_str(&search).ok() {
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
        // Use spawn_local to handle the async start() method
        spawn_local(async move {
            debug!("Starting FreenetSynchronizer from App component");
            // Clone the synchronizer to avoid holding the write lock during async operation
            let mut synchronizer_clone = SYNCHRONIZER.read().clone();
            synchronizer_clone.start().await;
            
            // Update the global synchronizer with the started instance
            *SYNCHRONIZER.write() = synchronizer_clone;

            set_up_chat_delegate().await;
        });

        // Add use_effect to watch for changes to rooms and trigger synchronization
        use_effect(move || {
            // This will run whenever rooms changes
            debug!("Rooms state changed, triggering synchronization");

            // Get all the data we need upfront to avoid nested borrows
            let message_sender = SYNCHRONIZER.read().get_message_sender();
            let has_rooms = !ROOMS.read().map.is_empty();
            let has_invitations = !PENDING_INVITES.read().map.is_empty();

            if has_rooms || has_invitations {
                info!("Change detected, sending ProcessRooms message to synchronizer, has_rooms={}, has_invitations={}", has_rooms, has_invitations);
                if let Err(e) = message_sender.unbounded_send(SynchronizerMessage::ProcessRooms) {
                    error!("Failed to send ProcessRooms message: {}", e);
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
            }

            // No need to return anything
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
