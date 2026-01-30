use crate::components::app::{CURRENT_ROOM, NEEDS_SYNC, ROOMS};
use crate::util::ecies::{seal_bytes, unseal_bytes};
use dioxus::logger::tracing::*;
use dioxus::prelude::*;
use dioxus_core::Event;
use freenet_scaffold::ComposableState;
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::privacy::{RoomDisplayMetadata, SealedBytes};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};
use wasm_bindgen_futures::spawn_local;

#[component]
pub fn RoomNameField(config: Configuration, is_owner: bool) -> Element {
    // Extract and decrypt the room name if we have the secret
    let initial_name = {
        let owner_key = CURRENT_ROOM.read().owner_key;
        let rooms = ROOMS.read();
        let secret = owner_key
            .and_then(|key| rooms.map.get(&key))
            .and_then(|room_data| room_data.current_secret.as_ref());
        match unseal_bytes(&config.display.name, secret) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
            Err(_) => config.display.name.to_string_lossy(),
        }
    };
    let mut room_name = use_signal(|| initial_name);

    let update_room_name = move |evt: Event<FormData>| {
        if !is_owner {
            return;
        }

        info!("Updating room name");
        let new_name = evt.value().to_string();
        if !new_name.is_empty() {
            room_name.set(new_name.clone());

            // Get the owner key first
            let owner_key = CURRENT_ROOM.read().owner_key.expect("No owner key");

            // Get signing data and encryption info from room
            let signing_data = ROOMS.with(|rooms| {
                if let Some(room_data) = rooms.map.get(&owner_key) {
                    Some((
                        room_data.room_key(),
                        room_data.self_sk.clone(),
                        room_data.room_state.clone(),
                        room_data.current_secret,
                        room_data.current_secret_version,
                    ))
                } else {
                    error!("Room state not found for current room");
                    None
                }
            });

            let Some((room_key, self_sk, room_state_clone, room_secret, secret_version)) = signing_data else {
                return;
            };

            // Encrypt name if room is private and we have a secret
            let sealed_name = match (room_secret, secret_version) {
                (Some(secret), Some(version)) => seal_bytes(new_name.as_bytes(), &secret, version),
                _ => SealedBytes::public(new_name.clone().into_bytes()),
            };

            let mut new_config = config.clone();
            new_config.display = RoomDisplayMetadata {
                name: sealed_name,
                description: new_config.display.description.clone(),
            };
            new_config.configuration_version += 1;

            spawn_local(async move {
                    // Serialize config to CBOR for signing
                    let mut config_bytes = Vec::new();
                    if let Err(e) = ciborium::ser::into_writer(&new_config, &mut config_bytes) {
                        error!("Failed to serialize config for signing: {:?}", e);
                        return;
                    }

                    // Sign using delegate with fallback to local signing
                    let signature =
                        crate::signing::sign_config_with_fallback(room_key, config_bytes, &self_sk)
                            .await;

                    let new_authorized_config =
                        AuthorizedConfigurationV1::with_signature(new_config, signature);

                    let delta = ChatRoomStateV1Delta {
                        configuration: Some(new_authorized_config),
                        ..Default::default()
                    };

                    ROOMS.with_mut(|rooms| {
                        if let Some(room_data) = rooms.map.get_mut(&owner_key) {
                            info!("Applying delta to room state");
                            match ComposableState::apply_delta(
                                &mut room_data.room_state,
                                &room_state_clone,
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
                });
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
