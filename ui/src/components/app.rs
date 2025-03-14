pub mod freenet_api;
pub mod room_state_handler;
pub mod sync_info;

use super::{conversation::Conversation, members::MemberList, room_list::RoomList};
use crate::components::app::freenet_api::freenet_synchronizer::SynchronizerMessage;
use crate::components::app::freenet_api::freenet_synchronizer::SynchronizerStatus;
use crate::components::app::freenet_api::FreenetSynchronizer;
use crate::storage;
use crate::room_data::RoomData;
use crate::components::members::member_info_modal::MemberInfoModal;
use crate::components::members::Invitation;
use crate::components::room_list::create_room_modal::CreateRoomModal;
use crate::components::room_list::edit_room_modal::EditRoomModal;
use crate::components::room_list::receive_invitation_modal::ReceiveInvitationModal;
use crate::invites::PendingInvites;
use crate::room_data::{CurrentRoom, Rooms};
use dioxus::logger::tracing::{error, info};
use dioxus::prelude::*;
use document::Stylesheet;
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::client_api::WebApi;
use river_common::room_state::member::MemberId;
use river_common::ChatRoomStateV1;
use std::collections::HashMap;
use wasm_bindgen_futures::spawn_local;
use web_sys::window;

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
    info!("App component loaded");

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
            info!("Starting FreenetSynchronizer from App component");
            SYNCHRONIZER.write().start().await;
        });

        // Add use_effect to load rooms from local storage on startup
        use_effect(move || {
            info!("Loading rooms from local storage");
            
            spawn_local(async move {
                match storage::load_rooms() {
                    Ok(stored_rooms) => {
                        if stored_rooms.is_empty() {
                            info!("No stored rooms found");
                            return;
                        }
                        
                        info!("Found {} stored rooms", stored_rooms.len());
                        
                        // Process each stored room
                        for (owner_vk, stored_data) in stored_rooms {
                            match stored_data.get_signing_key() {
                                Ok(signing_key) => {
                                    // Generate contract key
                                    let contract_key = crate::util::owner_vk_to_contract_key(&owner_vk);
                                    
                                    // Create a placeholder room state
                                    let room_data = RoomData {
                                        owner_vk,
                                        room_state: ChatRoomStateV1::default(),
                                        self_sk: signing_key,
                                        contract_key: contract_key.clone(),
                                    };
                                    
                                    // Add to rooms
                                    ROOMS.with_mut(|rooms| {
                                        rooms.map.insert(owner_vk, room_data);
                                    });
                                    
                                    // Register with sync info
                                    SYNC_INFO.with_mut(|sync_info| {
                                        sync_info.register_new_room(owner_vk);
                                    });
                                    
                                    // Request subscription to the room
                                    let message_sender = SYNCHRONIZER.read().get_message_sender();
                                    if let Err(e) = message_sender.unbounded_send(
                                        SynchronizerMessage::SubscribeToRoom { 
                                            owner_vk, 
                                            contract_key 
                                        }
                                    ) {
                                        error!("Failed to subscribe to stored room: {}", e);
                                    }
                                },
                                Err(e) => {
                                    error!("Failed to decode signing key for room: {}", e);
                                }
                            }
                        }
                    },
                    Err(e) => {
                        error!("Failed to load stored rooms: {}", e);
                    }
                }
            });
        });

        // Add use_effect to watch for changes to rooms and trigger synchronization
        use_effect(move || {
            // This will run whenever rooms changes
            info!("Rooms state changed, triggering synchronization");

            // Get a clone of the message sender outside of any read/write operations
            let message_sender = {
                info!("About to read SYNCHRONIZER to get message sender");
                let sender = SYNCHRONIZER.read().get_message_sender();
                info!("Successfully got message sender");
                sender
            };

            // Check if we have rooms to synchronize
            let has_rooms = {
                info!("About to read ROOMS to check if empty");
                let has_rooms = !ROOMS.read().map.is_empty();
                info!("Successfully checked ROOMS: has_rooms={}", has_rooms);
                has_rooms
            };

            if has_rooms {
                info!("Sending ProcessRooms message to synchronizer");
                if let Err(e) = message_sender.unbounded_send(SynchronizerMessage::ProcessRooms) {
                    error!("Failed to send ProcessRooms message: {}", e);
                }
            } else {
                info!("No rooms to synchronize");
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
        
        // Only show invite button when a room is selected
        {
            let mut invite_modal_active = use_signal(|| false);
            
            CURRENT_ROOM.read().owner_key.map(|_| {
                rsx! {
                    button {
                        class: "invite-member-button",
                        onclick: move |_| {
                            if let Some(current_room) = CURRENT_ROOM.read().owner_key {
                                MEMBER_INFO_MODAL.with_mut(|modal| {
                                    modal.member = None;
                                });
                                invite_modal_active.set(true);
                            }
                        },
                        span { class: "icon", i { class: "fas fa-user-plus" } }
                        span { "Invite Member" }
                    }
                }
            })
        }

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
