use crate::components::app::{CURRENT_ROOM, ROOMS};
use crate::util::ecies::{seal_for_room, unseal_bytes_with_secrets};
use dioxus::logger::tracing::*;
use dioxus::prelude::*;
use dioxus_core::Event;
use freenet_scaffold::ComposableState;
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::privacy::RoomDisplayMetadata;
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};
use wasm_bindgen_futures::spawn_local;

#[component]
pub fn RoomNameField(config: Configuration, is_owner: bool) -> Element {
    // Extract and decrypt the room name using version-aware decryption
    let initial_name = {
        let owner_key = CURRENT_ROOM.read().owner_key;
        let secrets = ROOMS
            .try_read()
            .ok()
            .and_then(|rooms| {
                owner_key
                    .and_then(|key| rooms.map.get(&key))
                    .map(|room_data| room_data.secrets.clone())
            })
            .unwrap_or_default();
        match unseal_bytes_with_secrets(&config.display.name, &secrets) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
            Err(_) => config.display.name.to_string_lossy(),
        }
    };
    let initial_name_for_revert = initial_name.clone();
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
                        room_data.is_private(),
                        room_data.get_secret().map(|(s, v)| (*s, v)),
                    ))
                } else {
                    error!("Room state not found for current room");
                    None
                }
            });

            let Some((room_key, self_sk, room_state_clone, is_private, room_secret_opt)) =
                signing_data
            else {
                return;
            };

            // Privacy guard for freenet/river#299: a private room with no
            // locally-available secret MUST NOT publish a plaintext room name
            // into the configuration. `seal_for_room` returns `None` in that
            // case so we defer — the owner can retry once the secret has
            // arrived. Revert the input so the UI doesn't silently lie about
            // what was saved.
            let room_secret_ref = room_secret_opt.as_ref().map(|(s, v)| (s, *v));
            let Some(sealed_name) =
                seal_for_room(is_private, room_secret_ref, new_name.clone().into_bytes())
            else {
                warn!(
                    "Private room secret not yet available locally — \
                     room name edit deferred to avoid leaking a plaintext \
                     configuration delta (freenet/river#299)."
                );
                room_name.set(initial_name_for_revert.clone());
                return;
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

                // Defer ROOMS mutation to a clean execution context to
                // prevent RefCell re-entrant borrow panics.
                crate::util::defer(move || {
                    let applied = ROOMS.with_mut(|rooms| {
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
                                    // #310: apply_delta re-runs the public-only
                                    // actions-state rebuild; re-derive private
                                    // edits/reactions with decryption. No-op on
                                    // public rooms.
                                    room_data.rebuild_private_actions_state();
                                    true
                                }
                                Err(e) => {
                                    error!("Failed to apply delta: {:?}", e);
                                    false
                                }
                            }
                        } else {
                            false
                        }
                    });
                    if applied {
                        crate::components::app::mark_needs_sync(owner_key);
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
