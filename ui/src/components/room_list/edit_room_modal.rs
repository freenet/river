use super::room_name_field::RoomNameField;
use crate::components::app::chat_delegate::save_rooms_to_delegate;
use crate::components::app::{CURRENT_ROOM, EDIT_ROOM_MODAL, NEEDS_SYNC, ROOMS};
use crate::util::ecies::{seal_bytes, unseal_bytes_with_secrets};
use dioxus::logger::tracing::{error, info};
use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::FaRotate;
use dioxus_free_icons::Icon;
use freenet_scaffold::ComposableState;
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::privacy::{PrivacyMode, RoomDisplayMetadata, SealedBytes};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};
use std::ops::Deref;
use wasm_bindgen_futures::spawn_local;

#[component]
pub fn EditRoomModal() -> Element {
    // State for leave confirmation
    let mut show_leave_confirmation = use_signal(|| false);

    // Memoize the room being edited
    let editing_room = use_memo(move || {
        EDIT_ROOM_MODAL.read().room.and_then(|editing_room_vk| {
            ROOMS.read().map.iter().find_map(|(room_vk, room_data)| {
                if &editing_room_vk == room_vk {
                    Some(room_data.clone())
                } else {
                    None
                }
            })
        })
    });

    // Memoize the room configuration
    let room_config = use_memo(move || {
        editing_room
            .read()
            .as_ref()
            .map(|room_data| room_data.room_state.configuration.configuration.clone())
    });

    // Memoize if the current user is the owner of the room being edited
    let user_is_owner = use_memo(move || {
        editing_room.read().as_ref().is_some_and(|room_data| {
            let user_vk = room_data.self_sk.verifying_key();
            let room_vk = EDIT_ROOM_MODAL.read().room.unwrap();
            user_vk == room_vk
        })
    });

    // Render the modal if room configuration is available
    if let Some(config) = room_config.clone().read().deref() {
        rsx! {
            // Modal backdrop
            div {
                class: "fixed inset-0 z-50 flex items-center justify-center",
                // Overlay
                div {
                    class: "absolute inset-0 bg-black/50",
                    onclick: move |_| {
                        EDIT_ROOM_MODAL.write().room = None;
                    }
                }
                // Modal content
                div {
                    class: "relative z-10 w-full max-w-md mx-4 bg-panel rounded-xl shadow-xl border border-border",
                    div {
                        class: "p-6",
                        h1 { class: "text-xl font-semibold text-text mb-4", "Room Details" }

                        RoomNameField {
                            config: config.clone(),
                            is_owner: *user_is_owner.read()
                        }

                        RoomDescriptionField {
                            config: config.clone(),
                            is_owner: *user_is_owner.read()
                        }

                        // Member capacity
                        if let Some(room_data) = editing_room.read().as_ref() {
                            {
                                let member_count = room_data.room_state.members.members.len();
                                let max_members = config.max_members;
                                let is_full = member_count >= max_members;
                                rsx! {
                                    MaxMembersField {
                                        member_count: member_count,
                                        max_members: max_members,
                                        is_full: is_full,
                                        is_owner: *user_is_owner.read(),
                                        config: config.clone(),
                                    }
                                }
                            }
                        }

                        // Numeric configuration fields (owner-only)
                        if *user_is_owner.read() {
                            NumericConfigField {
                                label: "Max Recent Messages",
                                value: config.max_recent_messages,
                                config: config.clone(),
                                field: ConfigField::MaxRecentMessages,
                            }
                            NumericConfigField {
                                label: "Max Message Size (bytes)",
                                value: config.max_message_size,
                                config: config.clone(),
                                field: ConfigField::MaxMessageSize,
                            }
                            NumericConfigField {
                                label: "Max User Bans",
                                value: config.max_user_bans,
                                config: config.clone(),
                                field: ConfigField::MaxUserBans,
                            }
                            NumericConfigField {
                                label: "Max Nickname Size",
                                value: config.max_nickname_size,
                                config: config.clone(),
                                field: ConfigField::MaxNicknameSize,
                            }
                            NumericConfigField {
                                label: "Max Room Name Size",
                                value: config.max_room_name,
                                config: config.clone(),
                                field: ConfigField::MaxRoomName,
                            }
                            NumericConfigField {
                                label: "Max Room Description Size",
                                value: config.max_room_description,
                                config: config.clone(),
                                field: ConfigField::MaxRoomDescription,
                            }
                        }

                        // Read-only room info
                        if let Some(room_data) = editing_room.read().as_ref() {
                            // Room Public Key
                            div {
                                class: "mt-4",
                                label {
                                    class: "block text-sm font-medium text-text-muted mb-1",
                                    title: "Ed25519 public key (Curve25519 elliptic curve)",
                                    "Room Public Key"
                                }
                                input {
                                    r#type: "text",
                                    readonly: true,
                                    title: "Ed25519 public key (Curve25519 elliptic curve)",
                                    class: "w-full px-3 py-2 bg-surface border border-border rounded-lg text-text-muted text-sm font-mono cursor-text select-all",
                                    value: "{bs58::encode(room_data.owner_vk.as_bytes()).into_string()}"
                                }
                            }
                            // Contract ID
                            div {
                                class: "mt-4",
                                label {
                                    class: "block text-sm font-medium text-text-muted mb-1",
                                    "Contract ID"
                                }
                                input {
                                    r#type: "text",
                                    readonly: true,
                                    class: "w-full px-3 py-2 bg-surface border border-border rounded-lg text-text-muted text-sm font-mono cursor-text select-all",
                                    value: "{room_data.contract_key.id()}"
                                }
                            }

                            // Secret Version (only for private rooms)
                            {
                                let is_private = room_data.room_state.configuration.configuration.privacy_mode == PrivacyMode::Private;
                                let is_owner = room_data.owner_vk == room_data.self_sk.verifying_key();
                                let secret_version = room_data.room_state.secrets.current_version;

                                if is_private {
                                    Some(rsx! {
                                        div {
                                            class: "mt-4",
                                            label {
                                                class: "block text-sm font-medium text-text-muted mb-1",
                                                "Secret Version"
                                            }
                                            div {
                                                class: "flex items-center gap-2",
                                                input {
                                                    r#type: "text",
                                                    readonly: true,
                                                    class: "flex-1 px-3 py-2 bg-surface border border-border rounded-lg text-text-muted text-sm font-mono cursor-text select-all",
                                                    value: "{secret_version}"
                                                }
                                                {
                                                    if is_owner {
                                                        Some(rsx! {
                                                            button {
                                                                class: "px-3 py-2 bg-surface hover:bg-surface-hover border border-border rounded-lg text-text-muted hover:text-text transition-colors flex items-center gap-2",
                                                                title: "Rotate room secret - generates a new encryption key for future messages",
                                                                onclick: move |_| {
                                                                    if let Some(current_room) = EDIT_ROOM_MODAL.read().room {
                                                                        info!("Rotating secret for room");
                                                                        ROOMS.with_mut(|rooms| {
                                                                            if let Some(room_data) = rooms.map.get_mut(&current_room) {
                                                                                match room_data.rotate_secret() {
                                                                                    Ok(secrets_delta) => {
                                                                                        info!("Secret rotated successfully");
                                                                                        let current_state = room_data.room_state.clone();
                                                                                        let delta = ChatRoomStateV1Delta {
                                                                                            secrets: Some(secrets_delta),
                                                                                            ..Default::default()
                                                                                        };
                                                                                        if let Err(e) = room_data.room_state.apply_delta(
                                                                                            &current_state,
                                                                                            &ChatRoomParametersV1 { owner: current_room },
                                                                                            &Some(delta),
                                                                                        ) {
                                                                                            error!("Failed to apply rotation delta: {}", e);
                                                                                        } else {
                                                                                            NEEDS_SYNC.write().insert(current_room);
                                                                                        }
                                                                                    }
                                                                                    Err(e) => error!("Failed to rotate secret: {}", e),
                                                                                }
                                                                            }
                                                                        });
                                                                    }
                                                                },
                                                                Icon { icon: FaRotate, width: 14, height: 14 }
                                                                span { "Rotate" }
                                                            }
                                                        })
                                                    } else {
                                                        None
                                                    }
                                                }
                                            }
                                        }
                                    })
                                } else {
                                    None
                                }
                            }
                        }

                        // Leave Room Section
                        if *show_leave_confirmation.read() {
                            div {
                                class: "bg-yellow-500/10 border border-yellow-500/20 rounded-lg p-4 mt-4",
                                p {
                                    class: "text-yellow-400 mb-3",
                                    if *user_is_owner.read() {
                                        "Warning: You are the owner of this room. Leaving will permanently delete it for you. Other members might retain access if they have the contract key, but coordination will be lost."
                                    } else {
                                        "Are you sure you want to leave this room? This action cannot be undone."
                                    }
                                }
                                div {
                                    class: "flex gap-3",
                                    button {
                                        class: "px-4 py-2 bg-red-500 hover:bg-red-600 text-white font-medium rounded-lg transition-colors",
                                        onclick: move |_| {
                                            // Read the room_vk first and drop the read borrow
                                            let room_vk_to_remove = EDIT_ROOM_MODAL.read().room;

                                            if let Some(room_vk) = room_vk_to_remove {
                                                // Perform writes *after* the read borrow is dropped
                                                ROOMS.write().map.remove(&room_vk);

                                                // Check and potentially clear CURRENT_ROOM
                                                if CURRENT_ROOM.read().owner_key == Some(room_vk) {
                                                    CURRENT_ROOM.write().owner_key = None;
                                                }

                                                // Close the modal *last*
                                                EDIT_ROOM_MODAL.write().room = None;

                                                // Save updated rooms to delegate storage
                                                info!("Room removed, saving to delegate");
                                                spawn(async move {
                                                    if let Err(e) = save_rooms_to_delegate().await {
                                                        error!("Failed to save rooms after removal: {}", e);
                                                    }
                                                });
                                            }
                                            // Reset confirmation state regardless
                                            show_leave_confirmation.set(false);
                                        },
                                        "Confirm Leave"
                                    }
                                    button {
                                        class: "px-4 py-2 bg-surface hover:bg-surface-hover text-text rounded-lg transition-colors",
                                        onclick: move |_| show_leave_confirmation.set(false),
                                        "Cancel"
                                    }
                                }
                            }
                        } else {
                             // Only show Leave button if not confirming
                            div {
                                class: "mt-4",
                                button {
                                    class: "px-4 py-2 border border-red-500 text-red-500 hover:bg-red-500/10 rounded-lg transition-colors",
                                    onclick: move |_| show_leave_confirmation.set(true),
                                    "Leave Room"
                                }
                            }
                        }
                    }
                    // Close button
                    button {
                        class: "absolute top-3 right-3 p-1 text-text-muted hover:text-text transition-colors",
                        onclick: move |_| {
                            EDIT_ROOM_MODAL.write().room = None;
                        },
                        "✕"
                    }
                }
            }
        }
    } else {
        rsx! {}
    }
}

#[component]
fn RoomDescriptionField(config: Configuration, is_owner: bool) -> Element {
    let initial_desc = {
        let owner_key = CURRENT_ROOM.read().owner_key;
        let rooms = ROOMS.read();
        let secrets = owner_key
            .and_then(|key| rooms.map.get(&key))
            .map(|room_data| room_data.secrets.clone())
            .unwrap_or_default();
        config
            .display
            .description
            .as_ref()
            .map(|sealed| match unseal_bytes_with_secrets(sealed, &secrets) {
                Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                Err(_) => sealed.to_string_lossy(),
            })
            .unwrap_or_default()
    };
    let mut description = use_signal(|| initial_desc);

    let update_description = move |evt: Event<FormData>| {
        if !is_owner {
            return;
        }

        let new_desc = evt.value().to_string();
        description.set(new_desc.clone());

        let owner_key = CURRENT_ROOM.read().owner_key.expect("No owner key");

        let signing_data = ROOMS.with(|rooms| {
            rooms.map.get(&owner_key).map(|room_data| {
                (
                    room_data.room_key(),
                    room_data.self_sk.clone(),
                    room_data.room_state.clone(),
                    room_data.get_secret().map(|(s, v)| (*s, v)),
                )
            })
        });

        let Some((room_key, self_sk, room_state_clone, room_secret_opt)) = signing_data else {
            return;
        };

        let sealed_desc = if new_desc.is_empty() {
            None
        } else {
            Some(match room_secret_opt {
                Some((secret, version)) => seal_bytes(new_desc.as_bytes(), &secret, version),
                _ => SealedBytes::public(new_desc.into_bytes()),
            })
        };

        let mut new_config = config.clone();
        new_config.display = RoomDisplayMetadata {
            name: new_config.display.name.clone(),
            description: sealed_desc,
        };
        new_config.configuration_version += 1;

        spawn_local(async move {
            let mut config_bytes = Vec::new();
            if let Err(e) = ciborium::ser::into_writer(&new_config, &mut config_bytes) {
                error!("Failed to serialize config for signing: {:?}", e);
                return;
            }

            let signature =
                crate::signing::sign_config_with_fallback(room_key, config_bytes, &self_sk).await;

            let new_authorized_config =
                AuthorizedConfigurationV1::with_signature(new_config, signature);

            let delta = ChatRoomStateV1Delta {
                configuration: Some(new_authorized_config),
                ..Default::default()
            };

            ROOMS.with_mut(|rooms| {
                if let Some(room_data) = rooms.map.get_mut(&owner_key) {
                    match ComposableState::apply_delta(
                        &mut room_data.room_state,
                        &room_state_clone,
                        &ChatRoomParametersV1 { owner: owner_key },
                        &Some(delta),
                    ) {
                        Ok(_) => {
                            info!("Room description updated successfully");
                            NEEDS_SYNC.write().insert(owner_key);
                        }
                        Err(e) => error!("Failed to apply description delta: {:?}", e),
                    }
                }
            });
        });
    };

    rsx! {
        div { class: "mb-4",
            label { class: "block text-sm font-medium text-text-muted mb-2", "Room Description" }
            textarea {
                class: "w-full px-3 py-2 bg-surface border border-border rounded-lg text-text placeholder-text-muted focus:outline-none focus:ring-2 focus:ring-accent focus:border-transparent disabled:opacity-50 disabled:cursor-not-allowed resize-y",
                rows: "3",
                placeholder: "Optional room description",
                value: "{description}",
                readonly: !is_owner,
                disabled: !is_owner,
                onchange: update_description,
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
#[allow(clippy::enum_variant_names)]
enum ConfigField {
    MaxRecentMessages,
    MaxMessageSize,
    MaxUserBans,
    MaxNicknameSize,
    MaxRoomName,
    MaxRoomDescription,
}

impl ConfigField {
    fn get(self, cfg: &Configuration) -> usize {
        match self {
            Self::MaxRecentMessages => cfg.max_recent_messages,
            Self::MaxMessageSize => cfg.max_message_size,
            Self::MaxUserBans => cfg.max_user_bans,
            Self::MaxNicknameSize => cfg.max_nickname_size,
            Self::MaxRoomName => cfg.max_room_name,
            Self::MaxRoomDescription => cfg.max_room_description,
        }
    }

    fn set(self, cfg: &mut Configuration, val: usize) {
        match self {
            Self::MaxRecentMessages => cfg.max_recent_messages = val,
            Self::MaxMessageSize => cfg.max_message_size = val,
            Self::MaxUserBans => cfg.max_user_bans = val,
            Self::MaxNicknameSize => cfg.max_nickname_size = val,
            Self::MaxRoomName => cfg.max_room_name = val,
            Self::MaxRoomDescription => cfg.max_room_description = val,
        }
    }
}

#[component]
fn NumericConfigField(
    label: &'static str,
    value: usize,
    config: Configuration,
    field: ConfigField,
) -> Element {
    let mut input_value = use_signal(|| value.to_string());

    let update_value = move |evt: Event<FormData>| {
        let new_val_str = evt.value().to_string();
        input_value.set(new_val_str.clone());

        let Ok(new_val) = new_val_str.parse::<usize>() else {
            return;
        };
        if new_val == 0 || new_val == field.get(&config) {
            return;
        }

        info!("Updating {label} to {new_val}");

        let owner_key = CURRENT_ROOM.read().owner_key.expect("No owner key");

        let signing_data = ROOMS.with(|rooms| {
            rooms.map.get(&owner_key).map(|room_data| {
                (
                    room_data.room_key(),
                    room_data.self_sk.clone(),
                    room_data.room_state.clone(),
                )
            })
        });

        let Some((room_key, self_sk, room_state_clone)) = signing_data else {
            return;
        };

        let mut new_config = config.clone();
        field.set(&mut new_config, new_val);
        new_config.configuration_version += 1;

        spawn_local(async move {
            let mut config_bytes = Vec::new();
            if let Err(e) = ciborium::ser::into_writer(&new_config, &mut config_bytes) {
                error!("Failed to serialize config: {:?}", e);
                return;
            }

            let signature =
                crate::signing::sign_config_with_fallback(room_key, config_bytes, &self_sk).await;

            let new_authorized_config =
                AuthorizedConfigurationV1::with_signature(new_config, signature);

            let delta = ChatRoomStateV1Delta {
                configuration: Some(new_authorized_config),
                ..Default::default()
            };

            ROOMS.with_mut(|rooms| {
                if let Some(room_data) = rooms.map.get_mut(&owner_key) {
                    match ComposableState::apply_delta(
                        &mut room_data.room_state,
                        &room_state_clone,
                        &ChatRoomParametersV1 { owner: owner_key },
                        &Some(delta),
                    ) {
                        Ok(_) => {
                            info!("{label} updated successfully");
                            NEEDS_SYNC.write().insert(owner_key);
                        }
                        Err(e) => error!("Failed to apply {label} delta: {:?}", e),
                    }
                }
            });
        });
    };

    rsx! {
        div { class: "mb-4",
            label { class: "block text-sm font-medium text-text-muted mb-2", "{label}" }
            input {
                r#type: "number",
                min: "1",
                class: "w-full px-3 py-2 bg-surface border border-border rounded-lg text-text focus:outline-none focus:ring-2 focus:ring-accent focus:border-transparent",
                value: "{input_value}",
                onchange: update_value,
            }
        }
    }
}

#[component]
fn MaxMembersField(
    member_count: usize,
    max_members: usize,
    is_full: bool,
    is_owner: bool,
    config: Configuration,
) -> Element {
    let mut max_members_input = use_signal(|| max_members.to_string());

    let update_max_members = move |evt: Event<FormData>| {
        if !is_owner {
            return;
        }
        let new_val_str = evt.value().to_string();
        max_members_input.set(new_val_str.clone());

        let Ok(new_max) = new_val_str.parse::<usize>() else {
            return;
        };
        if new_max == 0 || new_max == config.max_members {
            return;
        }

        info!("Updating max_members to {new_max}");

        let owner_key = CURRENT_ROOM.read().owner_key.expect("No owner key");

        let signing_data = ROOMS.with(|rooms| {
            rooms.map.get(&owner_key).map(|room_data| {
                (
                    room_data.room_key(),
                    room_data.self_sk.clone(),
                    room_data.room_state.clone(),
                )
            })
        });

        let Some((room_key, self_sk, room_state_clone)) = signing_data else {
            return;
        };

        let mut new_config = config.clone();
        new_config.max_members = new_max;
        new_config.configuration_version += 1;

        wasm_bindgen_futures::spawn_local(async move {
            let mut config_bytes = Vec::new();
            if let Err(e) = ciborium::ser::into_writer(&new_config, &mut config_bytes) {
                error!("Failed to serialize config: {:?}", e);
                return;
            }

            let signature =
                crate::signing::sign_config_with_fallback(room_key, config_bytes, &self_sk).await;

            let new_authorized_config =
                AuthorizedConfigurationV1::with_signature(new_config, signature);

            let delta = ChatRoomStateV1Delta {
                configuration: Some(new_authorized_config),
                ..Default::default()
            };

            ROOMS.with_mut(|rooms| {
                if let Some(room_data) = rooms.map.get_mut(&owner_key) {
                    match ComposableState::apply_delta(
                        &mut room_data.room_state,
                        &room_state_clone,
                        &ChatRoomParametersV1 { owner: owner_key },
                        &Some(delta),
                    ) {
                        Ok(_) => {
                            info!("max_members updated successfully");
                            NEEDS_SYNC.write().insert(owner_key);
                        }
                        Err(e) => error!("Failed to apply max_members delta: {:?}", e),
                    }
                }
            });
        });
    };

    rsx! {
        div { class: "mb-4",
            label { class: "block text-sm font-medium text-text-muted mb-2",
                "Members ({member_count}/{max_members})"
            }
            if is_full {
                p { class: "text-xs text-red-400 mb-1",
                    "Room is full — new members will be rejected."
                }
            }
            if is_owner {
                input {
                    r#type: "number",
                    min: "1",
                    class: "w-full px-3 py-2 bg-surface border border-border rounded-lg text-text focus:outline-none focus:ring-2 focus:ring-accent focus:border-transparent",
                    value: "{max_members_input}",
                    onchange: update_max_members,
                }
            }
        }
    }
}
