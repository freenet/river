pub mod freenet_api;
pub mod room_state_handler;

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
use dioxus::logger::tracing::{error, info};
use dioxus::prelude::*;
use document::Stylesheet;
use ed25519_dalek::VerifyingKey;
use river_common::room_state::member::MemberId;
use wasm_bindgen_futures::spawn_local;
use web_sys::window;

pub fn App() -> Element {
    info!("App component loaded");

    // Create context providers for our application state
    let rooms = use_context_provider(|| Signal::new(initial_rooms()));
    use_context_provider(|| Signal::new(CurrentRoom { owner_key: None }));
    use_context_provider(|| Signal::new(MemberInfoModalSignal { member: None }));
    use_context_provider(|| Signal::new(EditRoomModalSignal { room: None }));
    use_context_provider(|| Signal::new(CreateRoomModalSignal { show: false }));
    let _pending_invites = use_context_provider(|| Signal::new(PendingInvites::default()));

    // Create the sync status signal
    let sync_status = use_context_provider(|| Signal::new(SynchronizerStatus::Connecting));

    // Track initialization to prevent multiple starts
    let mut initialized = use_signal(|| false);

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
        // Only initialize once
        if !initialized.read().clone() {
            info!("Initializing Freenet synchronizer (first time)");

            // Create the synchronizer signal
            let mut synchronizer = use_context_provider(|| {
                Signal::new(FreenetSynchronizer::new(rooms.clone(), sync_status))
            });

            // Use spawn_local to handle the async start() method
            spawn_local(async move {
                info!("Starting FreenetSynchronizer from App component");
                synchronizer.write().start().await;
            });

            // Mark as initialized
            initialized.set(true);
            info!("Freenet synchronizer initialization flag set");
        } else {
            info!("Freenet synchronizer already initialized, skipping");
        }

        // Add use_effect to watch for changes to rooms and trigger synchronization
        {
            let rooms_clone = rooms.clone();

            use_effect(move || {
                // This will run whenever rooms changes
                info!("Rooms state changed, triggering synchronization");
                let _rooms_read = rooms_clone.read(); // Read to track dependency

                // Get the synchronizer from context
                if let Some(synchronizer_signal) = use_context::<Signal<FreenetSynchronizer>>() {
                    // Then try to read the signal
                    if let Ok(sync) = synchronizer_signal.try_read() {
                    let sender = sync.get_message_sender();

                    // Send a message to process rooms
                    info!("Sending ProcessRooms message to synchronizer");
                    if let Err(e) = sender.unbounded_send(SynchronizerMessage::ProcessRooms) {
                        error!("Failed to send ProcessRooms message: {}", e);
                    }
                    } else {
                        error!("Could not read synchronizer signal");
                    }
                } else {
                    error!("No synchronizer signal found in context");
                }

                // No need to return anything
            });
        }

        info!("FreenetSynchronizer setup complete");
    }

    rsx! {
        Stylesheet { href: asset!("./assets/bulma.min.css") }
        Stylesheet { href: asset!("./assets/main.css") }
        Stylesheet { href: asset!("./assets/fontawesome/css/all.min.css") }

        // Status indicator for Freenet connection
        div {
            class: "notification is-small",
            style: {
                let status = sync_status.read();
                match &*status {
                    SynchronizerStatus::Connected => "position: fixed; top: 10px; right: 10px; padding: 5px 10px; background-color: #48c774; color: white; z-index: 100;",
                    SynchronizerStatus::Connecting => "position: fixed; top: 10px; right: 10px; padding: 5px 10px; background-color: #ffdd57; color: black; z-index: 100;",
                    SynchronizerStatus::Disconnected => "position: fixed; top: 10px; right: 10px; padding: 5px 10px; background-color: #f14668; color: white; z-index: 100;",
                    SynchronizerStatus::Error(_) => "position: fixed; top: 10px; right: 10px; padding: 5px 10px; background-color: #f14668; color: white; z-index: 100;",
                }
            },
            {
                let status = sync_status.read();
                match &*status {
                    SynchronizerStatus::Connected => "Connected".to_string(),
                    SynchronizerStatus::Connecting => "Connecting...".to_string(),
                    SynchronizerStatus::Disconnected => "Disconnected".to_string(),
                    SynchronizerStatus::Error(ref msg) => format!("Error: {}", msg),
                }
            }
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
