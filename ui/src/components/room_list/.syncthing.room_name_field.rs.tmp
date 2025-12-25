use crate::components::app::{CURRENT_ROOM, NEEDS_SYNC, ROOMS};
use dioxus::logger::tracing::*;
use dioxus::prelude::*;
use dioxus_core::Event;
use freenet_scaffold::ComposableState;
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::privacy::{RoomDisplayMetadata, SealedBytes};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};

#[component]
pub fn RoomNameField(config: Configuration, is_owner: bool) -> Element {
    // Extract the room name as a string (for now, only handles public names)
    let mut room_name = use_signal(|| config.display.name.to_string_lossy());

    let update_room_name = move |evt: Event<FormData>| {
        if !is_owner {
            return;
        }

        info!("Updating room name");
        let new_name = evt.value().to_string();
        if !new_name.is_empty() {
            room_name.set(new_name.clone());
            let mut new_config = config.clone();
            new_config.display = RoomDisplayMetadata {
                name: SealedBytes::public(new_name.into_bytes()),
                description: new_config.display.description.clone(),
            };
            new_config.configuration_version += 1;

            // Get the owner key first
            let owner_key = CURRENT_ROOM.read().owner_key.expect("No owner key");

            // Prepare the delta outside the borrow
            let delta = ROOMS.with(|rooms| {
                if let Some(room_data) = rooms.map.get(&owner_key) {
                    let signing_key = &room_data.self_sk;
                    let new_authorized_config =
                        AuthorizedConfigurationV1::new(new_config, signing_key);

                    Some(ChatRoomStateV1Delta {
                        configuration: Some(new_authorized_config),
                        ..Default::default()
                    })
                } else {
                    error!("Room state not found for current room");
                    None
                }
            });

            // Apply the delta if we have one
            if let Some(delta) = delta {
                ROOMS.with_mut(|rooms| {
                    if let Some(room_data) = rooms.map.get_mut(&owner_key) {
                        info!("Applying delta to room state");
                        let parent_state = room_data.room_state.clone();
                        match ComposableState::apply_delta(
                            &mut room_data.room_state,
                            &parent_state,
                            &ChatRoomParametersV1 { owner: owner_key },
                            &Some(delta),
                        ) {
                            Ok(_) => {
                                info!("Delta applied successfully");
                                // Mark room as needing sync after name change
                                NEEDS_SYNC.write().insert(owner_key);
                            }
                            Err(e) => error!("Failed to apply delta: {:?}", e),
                        }
                    }
                });
            }
        } else {
            error!("Room name is empty");
        }
    };

    rsx! {
        div { class: "mb-4",
            label { class: "block text-sm font-medium text-text-muted mb-2", "Room Name" }
            input {
                class: "w-full px-3 py-2 bg-surface border border-border rounded-lg text-text placeholder-text-muted focus:outline-none focus:ring-2 focus:ring-accent focus:border-transparent disabled:opacity-50 disabled:cursor-not-allowed",
                value: "{room_name}",
                readonly: !is_owner,
                disabled: !is_owner,
                onchange: update_room_name,
            }
        }
    }
}
