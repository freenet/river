pub mod freenet_api;
pub mod room_state_handler;

use super::{conversation::Conversation, members::MemberList, room_list::RoomList};
use crate::components::app::freenet_api::FreenetApiSynchronizer;
use crate::components::members::member_info_modal::MemberInfoModal;
use crate::components::members::Invitation;
use crate::components::room_list::create_room_modal::CreateRoomModal;
use crate::components::room_list::edit_room_modal::EditRoomModal;
use crate::components::room_list::receive_invitation_modal::ReceiveInvitationModal;
use crate::invites::PendingInvites;
use crate::room_data::{CurrentRoom, Rooms};
use dioxus::logger::tracing::info;
use dioxus::prelude::*;
use document::Stylesheet;
use ed25519_dalek::VerifyingKey;
use river_common::room_state::member::MemberId;
use web_sys::window;

pub fn App() -> Element {
    info!("App component loaded");
    
    use_context_provider(|| Signal::new(initial_rooms()));
    use_context_provider(|| Signal::new(CurrentRoom { owner_key: None }));
    use_context_provider(|| Signal::new(MemberInfoModalSignal { member: None }));
    use_context_provider(|| Signal::new(EditRoomModalSignal { room: None }));
    use_context_provider(|| Signal::new(CreateRoomModalSignal { show: false }));
    use_context_provider(|| Signal::new(PendingInvites::default()));
    
    // Provide the sync status signal
    use_context_provider(|| Signal::new(crate::components::app::freenet_api::SyncStatus::Connecting));
    
    // Get the current sync status for display
    let sync_status = use_context::<Signal<crate::components::app::freenet_api::SyncStatus>>();
    
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
        info!("Initializing Freenet API synchronizer");

        // Get the signals we need to pass
        let rooms_signal = use_context::<Signal<Rooms>>().expect("Rooms signal should be available");
        let status_signal = use_context::<Signal<crate::components::app::freenet_api::SyncStatus>>().expect("Status signal should be available");
        let pending_invites_signal = use_context::<Signal<PendingInvites>>().expect("PendingInvites signal should be available");

        // Create the synchronizer with signals
        let mut api = FreenetApiSynchronizer::new()
            .with_signals(rooms_signal, status_signal, pending_invites_signal);
        
        // Start it directly before putting it in context
        api.start();
        info!("After calling api.start()");
        
        // Put it in the context
        use_context_provider(|| Signal::new(api));
        
        // Log that we've completed the initialization
        info!("FreenetApiSynchronizer initialization complete");
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
                    crate::components::app::freenet_api::SyncStatus::Connected => "position: fixed; top: 10px; right: 10px; padding: 5px 10px; background-color: #48c774; color: white; z-index: 100;",
                    crate::components::app::freenet_api::SyncStatus::Connecting => "position: fixed; top: 10px; right: 10px; padding: 5px 10px; background-color: #ffdd57; color: black; z-index: 100;",
                    crate::components::app::freenet_api::SyncStatus::Syncing => "position: fixed; top: 10px; right: 10px; padding: 5px 10px; background-color: #3298dc; color: white; z-index: 100;",
                    crate::components::app::freenet_api::SyncStatus::Error(_) => "position: fixed; top: 10px; right: 10px; padding: 5px 10px; background-color: #f14668; color: white; z-index: 100;",
                }
            },
            {
                let status = sync_status.read();
                match &*status {
                    crate::components::app::freenet_api::SyncStatus::Connected => "Connected".to_string(),
                    crate::components::app::freenet_api::SyncStatus::Connecting => "Connecting...".to_string(),
                    crate::components::app::freenet_api::SyncStatus::Syncing => "Syncing...".to_string(),
                    crate::components::app::freenet_api::SyncStatus::Error(ref msg) => format!("Error: {}", msg),
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
