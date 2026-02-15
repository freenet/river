use crate::components::app::{CURRENT_ROOM, NEEDS_SYNC, ROOMS};
use crate::util::ecies::{seal_bytes, unseal_bytes_with_secrets};
use dioxus::logger::tracing::*;
use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::FaPencil;
use dioxus_free_icons::Icon;
use freenet_scaffold::ComposableState;
use river_core::room_state::member::MemberId;
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_core::room_state::privacy::SealedBytes;
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};
use std::collections::HashMap;
use std::rc::Rc;

#[component]
pub fn NicknameField(member_info: AuthorizedMemberInfo) -> Element {
    // Compute values
    let (self_signing_key, room_secrets, current_secret_opt) = {
        let current_room = CURRENT_ROOM.read();
        if let Some(key) = current_room.owner_key.as_ref() {
            let rooms = ROOMS.read();
            rooms
                .map
                .get(key)
                .map(|room_data| {
                    (
                        Some(room_data.self_sk.clone()),
                        room_data.secrets.clone(),
                        room_data.get_secret().map(|(s, v)| (*s, v)),
                    )
                })
                .unwrap_or((None, HashMap::new(), None))
        } else {
            (None, HashMap::new(), None)
        }
    };

    let self_member_id = self_signing_key
        .as_ref()
        .map(|sk| MemberId::from(&sk.verifying_key()));

    let member_id = member_info.member_info.member_id;
    let is_self = self_member_id
        .as_ref()
        .map(|smi| smi == &member_id)
        .unwrap_or(false);

    // Decrypt nickname for display (version-aware)
    let initial_nickname =
        match unseal_bytes_with_secrets(&member_info.member_info.preferred_nickname, &room_secrets)
        {
            Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
            Err(_) => member_info.member_info.preferred_nickname.to_string_lossy(),
        };
    let mut temp_nickname = use_signal(|| initial_nickname);
    let mut input_element = use_signal(|| None as Option<Rc<MountedData>>);

    let save_changes = {
        info!("Saving nickname changes");

        let self_signing_key = self_signing_key.clone();
        let member_info = member_info.clone();

        move |new_value: String| {
            if new_value.is_empty() {
                warn!("Nickname cannot be empty");
                return;
            }

            let delta = if let Some(signing_key) = self_signing_key.clone() {
                // Encrypt nickname if room is private and we have a secret
                let sealed_nickname = match current_secret_opt {
                    Some((secret, version)) => seal_bytes(new_value.as_bytes(), &secret, version),
                    _ => SealedBytes::public(new_value.into_bytes()),
                };
                let new_member_info = MemberInfo {
                    member_id: member_info.member_info.member_id,
                    version: member_info.member_info.version + 1,
                    preferred_nickname: sealed_nickname,
                };
                let new_authorized_member_info =
                    AuthorizedMemberInfo::new_with_member_key(new_member_info, &signing_key);
                // Check if user needs to re-add themselves (pruned for inactivity)
                let members_delta = {
                    let rooms = ROOMS.read();
                    let current_room = CURRENT_ROOM.read();
                    if let (Some(owner_key), Some(room_data)) = (
                        current_room.owner_key,
                        current_room.owner_key.and_then(|k| rooms.map.get(&k)),
                    ) {
                        let self_vk = signing_key.verifying_key();
                        let is_in_members = self_vk == owner_key
                            || room_data
                                .room_state
                                .members
                                .members
                                .iter()
                                .any(|m| m.member.member_vk == self_vk);
                        if !is_in_members {
                            if let Some(ref authorized_member) = room_data.self_authorized_member {
                                let current_member_ids: std::collections::HashSet<_> = room_data
                                    .room_state
                                    .members
                                    .members
                                    .iter()
                                    .map(|m| m.member.id())
                                    .collect();
                                let mut members_to_add = vec![authorized_member.clone()];
                                for chain_member in &room_data.invite_chain {
                                    if !current_member_ids.contains(&chain_member.member.id()) {
                                        members_to_add.push(chain_member.clone());
                                    }
                                }
                                Some(river_core::room_state::member::MembersDelta::new(
                                    members_to_add,
                                ))
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                };

                Some(ChatRoomStateV1Delta {
                    member_info: Some(vec![new_authorized_member_info]),
                    members: members_delta,
                    ..Default::default()
                })
            } else {
                warn!("No signing key available");
                None
            };

            if let Some(delta) = delta {
                info!("Saving changes to nickname with delta: {:?}", delta);

                // Get the owner key first
                let owner_key = CURRENT_ROOM.read().owner_key;

                if let Some(owner_key) = owner_key {
                    // Use with_mut for atomic update
                    ROOMS.with_mut(|rooms| {
                        if let Some(room_data) = rooms.map.get_mut(&owner_key) {
                            info!(
                                "State before applying nickname delta: {:?}",
                                room_data.room_state
                            );
                            if let Err(e) = room_data.room_state.apply_delta(
                                &room_data.room_state.clone(),
                                &ChatRoomParametersV1 { owner: owner_key },
                                &Some(delta),
                            ) {
                                error!("Failed to apply delta: {:?}", e);
                            } else {
                                info!(
                                    "State after applying nickname delta: {:?}",
                                    room_data.room_state
                                );
                                // Mark room as needing sync after nickname change
                                NEEDS_SYNC.write().insert(owner_key);
                            }
                        } else {
                            warn!("Room state not found for current room");
                        }
                    });
                }
            }
        }
    };

    let on_input = move |evt: dioxus_core::Event<FormData>| {
        temp_nickname.set(evt.value().clone());
    };

    let on_blur = {
        let save_changes = save_changes.clone();
        move |_| {
            let new_value = temp_nickname();
            save_changes(new_value);
        }
    };

    let on_keydown = {
        let save_changes = save_changes.clone();
        move |evt: dioxus_core::Event<KeyboardData>| {
            if evt.key() == Key::Enter {
                let new_value = temp_nickname();
                save_changes(new_value);

                // Blur the input element
                if let Some(element) = input_element() {
                    wasm_bindgen_futures::spawn_local(async move {
                        let _ = element.set_focus(false).await;
                    });
                }
            }
        }
    };

    rsx! {
        div {
            class: "mb-4",
            label { class: "block text-sm font-medium text-text-muted mb-2", "Nickname" }
            div {
                class: "relative",
                input {
                    class: "w-full px-3 py-2 bg-surface border border-border rounded-lg text-text placeholder-text-muted focus:outline-none focus:ring-2 focus:ring-accent focus:border-transparent",
                    value: "{temp_nickname}",
                    readonly: !is_self,
                    oninput: on_input,
                    onblur: on_blur,
                    onkeydown: on_keydown,
                    onmounted: move |cx| input_element.set(Some(cx.data())),
                }
                if is_self {
                    span {
                        class: "absolute right-3 top-1/2 -translate-y-1/2 text-text-muted",
                        Icon { icon: FaPencil, width: 14, height: 14 }
                    }
                }
            }
        }
    }
}
