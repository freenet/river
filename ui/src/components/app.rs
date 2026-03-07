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
    load_invitation_from_storage, save_invitation_to_storage, ReceiveInvitationModal,
};
use crate::invites::PendingInvites;
use crate::room_data::{CurrentRoom, Rooms};
use dioxus::document::{Link, Stylesheet};
use dioxus::logger::tracing::{debug, error, info};
use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::{FaBars, FaUsers, FaXmark};
use dioxus_free_icons::Icon;
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::client_api::WebApi;
use river_core::room_state::member::MemberId;
use wasm_bindgen_futures::spawn_local;
use web_sys::window;

/// Which panel is open as an overlay on mobile. None = conversation only.
#[derive(Clone, Copy, PartialEq)]
pub enum MobilePanel {
    Rooms,
    Members,
}

pub static MOBILE_PANEL: GlobalSignal<Option<MobilePanel>> = Global::new(|| None);

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

// Build metadata from build.rs
const BUILD_TIMESTAMP: &str = env!("BUILD_TIMESTAMP_ISO");
const GIT_COMMIT: &str = env!("GIT_COMMIT_HASH");

#[component]
pub fn App() -> Element {
    info!(
        "River UI loaded - Built: {} | Commit: {}",
        BUILD_TIMESTAMP, GIT_COMMIT
    );

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

    // Check URL for invitation parameter, then fall back to localStorage
    let mut found_invitation = false;
    if let Some(window) = window() {
        if let Ok(search) = window.location().search() {
            if let Ok(params) = web_sys::UrlSearchParams::new_with_str(&search) {
                if let Some(invitation_code) = params.get("invitation") {
                    if let Ok(invitation) = Invitation::from_encoded_string(&invitation_code) {
                        info!("Received invitation from URL: {:?}", invitation);
                        save_invitation_to_storage(&invitation);
                        receive_invitation.set(Some(invitation));
                        found_invitation = true;

                        // Remove invitation parameter from URL to prevent re-processing on refresh
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
                            let _ = history.replace_state_with_url(
                                &wasm_bindgen::JsValue::NULL,
                                "",
                                Some(&new_url),
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
        // Favicon
        Link { rel: "icon", r#type: "image/svg+xml", href: asset!("/assets/river_logo.svg") }
        // Stylesheets
        Stylesheet { href: asset!("/assets/styles.css") }
        Stylesheet { href: asset!("/assets/main.css") }

        // Outer wrapper: flex-col for mobile header + main content
        // app-root: position:fixed for iOS Safari virtual keyboard fix
        div { class: "flex flex-col overflow-hidden app-root",

        // Mobile header bar (hidden on md+)
        div { class: "md:hidden flex items-center justify-between px-3 py-2 bg-panel border-b border-border flex-shrink-0",
            button {
                class: "p-2 rounded-lg text-text-muted hover:text-accent hover:bg-surface transition-colors",
                aria_label: if *MOBILE_PANEL.read() == Some(MobilePanel::Rooms) { "Close rooms" } else { "Open rooms" },
                onclick: move |_| {
                    let current = *MOBILE_PANEL.read();
                    *MOBILE_PANEL.write() = match current {
                        Some(MobilePanel::Rooms) => None,
                        _ => Some(MobilePanel::Rooms),
                    };
                },
                if *MOBILE_PANEL.read() == Some(MobilePanel::Rooms) {
                    Icon { icon: FaXmark, width: 20, height: 20 }
                } else {
                    Icon { icon: FaBars, width: 20, height: 20 }
                }
            }
            img {
                class: "w-8 h-8",
                src: asset!("/assets/river_logo.svg"),
                alt: "River"
            }
            button {
                class: "p-2 rounded-lg text-text-muted hover:text-accent hover:bg-surface transition-colors",
                aria_label: if *MOBILE_PANEL.read() == Some(MobilePanel::Members) { "Close members" } else { "Open members" },
                onclick: move |_| {
                    let current = *MOBILE_PANEL.read();
                    *MOBILE_PANEL.write() = match current {
                        Some(MobilePanel::Members) => None,
                        _ => Some(MobilePanel::Members),
                    };
                },
                if *MOBILE_PANEL.read() == Some(MobilePanel::Members) {
                    Icon { icon: FaXmark, width: 20, height: 20 }
                } else {
                    Icon { icon: FaUsers, width: 20, height: 20 }
                }
            }
        }

        // Main chat layout
        div { class: "flex flex-1 min-h-0 bg-bg overflow-hidden",
            // Desktop: always visible sidebar. Mobile: overlay with backdrop when active.
            if *MOBILE_PANEL.read() == Some(MobilePanel::Rooms) {
                // Backdrop scrim — tap to dismiss
                div {
                    class: "md:hidden fixed inset-0 top-[49px] z-40 bg-black/30",
                    onclick: move |_| { *MOBILE_PANEL.write() = None; },
                }
            }
            div {
                class: {
                    let mobile_rooms = *MOBILE_PANEL.read() == Some(MobilePanel::Rooms);
                    if mobile_rooms {
                        "absolute inset-0 top-[49px] z-50 md:relative md:inset-auto md:top-auto md:z-auto md:block"
                    } else {
                        "hidden md:block"
                    }
                },
                RoomList {}
            }

            // Conversation: always visible
            div { class: "flex-1 flex flex-col min-w-0",
                Conversation {}
            }

            // Desktop: always visible sidebar. Mobile: overlay with backdrop when active.
            if *MOBILE_PANEL.read() == Some(MobilePanel::Members) {
                // Backdrop scrim — tap to dismiss
                div {
                    class: "md:hidden fixed inset-0 top-[49px] z-40 bg-black/30",
                    onclick: move |_| { *MOBILE_PANEL.write() = None; },
                }
            }
            div {
                class: {
                    let mobile_members = *MOBILE_PANEL.read() == Some(MobilePanel::Members);
                    if mobile_members {
                        "absolute inset-0 top-[49px] z-50 md:relative md:inset-auto md:top-auto md:z-auto md:block"
                    } else {
                        "hidden md:block"
                    }
                },
                MemberList {}
            }
        }

        } // close outer flex-col wrapper
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
