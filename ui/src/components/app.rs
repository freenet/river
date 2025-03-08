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

#[component]
pub fn App() -> Element {
    info!("App component loaded");

    // Create context providers for our application state using signals
    let rooms = use_context_provider(|| use_signal(|| initial_rooms()));
    use_context_provider(|| use_signal(|| CurrentRoom { owner_key: None }));
    use_context_provider(|| use_signal(|| MemberInfoModalSignal { member: None }));
    use_context_provider(|| use_signal(|| EditRoomModalSignal { room: None }));
    use_context_provider(|| use_signal(|| CreateRoomModalSignal { show: false }));
    let _pending_invites = use_context_provider(|| use_signal(|| PendingInvites::default()));

    // Create the sync status signal
    let sync_status = use_context_provider(|| use_signal(|| SynchronizerStatus::Connecting));

    let receive_invitation = use_signal(|| None::<Invitation>);

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
        info!("Initializing Freenet synchronizer");

        // Create the synchronizer signal
        let synchronizer = use_context_provider(|| {
            use_signal(|| FreenetSynchronizer::new(rooms.clone(), sync_status))
        });

        // Use spawn_local to handle the async start() method
        let sync_clone = synchronizer.clone();
        spawn_local(async move {
            info!("Starting FreenetSynchronizer from App component");
            sync_clone.with_mut(|s| async move {
                s.start().await;
            }).await;
        });

        // Add use_effect to watch for changes to rooms and trigger synchronization
        use_effect(move || {
            // This will run whenever rooms changes
            info!("Rooms state changed, triggering synchronization");
            if !rooms.read().map.is_empty() {
                let sender = synchronizer.read().get_message_sender();

                info!("Sending ProcessRooms message to synchronizer");
                if let Err(e) = sender.unbounded_send(SynchronizerMessage::ProcessRooms) {
                    error!("Failed to send ProcessRooms message: {}", e);
                }
            } else {
                info!("No rooms to synchronize");
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
            class: "notification is-small",
            style: {
                match *sync_status.read() {
                    SynchronizerStatus::Connected => "position: fixed; top: 10px; right: 10px; padding: 5px 10px; background-color: #48c774; color: white; z-index: 100;",
                    SynchronizerStatus::Connecting => "position: fixed; top: 10px; right: 10px; padding: 5px 10px; background-color: #ffdd57; color: black; z-index: 100;",
                    SynchronizerStatus::Disconnected => "position: fixed; top: 10px; right: 10px; padding: 5px 10px; background-color: #f14668; color: white; z-index: 100;",
                    SynchronizerStatus::Error(_) => "position: fixed; top: 10px; right: 10px; padding: 5px 10px; background-color: #f14668; color: white; z-index: 100;",
                }
            },
            {
                match *sync_status.read() {
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
