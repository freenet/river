use crate::components::app::{CURRENT_ROOM, ROOMS};
use crate::room_data::{CurrentRoom, Rooms};
use dioxus::events::Key;
use dioxus::logger::tracing::*;
use dioxus::prelude::*;
use freenet_scaffold::ComposableState;
use river_common::room_state::member::MemberId;
use river_common::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_common::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};
use std::rc::Rc;

#[component]
pub fn NicknameField(member_info: AuthorizedMemberInfo) -> Element {
    // Compute values
    let self_signing_key = {
        let current_room = CURRENT_ROOM.read();
        if let Some(key) = current_room.owner_key.as_ref() {
            let rooms = ROOMS.read();
            if let Some(room_data) = rooms.map.get(key) {
                Some(room_data.self_sk.clone())
            } else {
                None
            }
        } else {
            None
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

    let mut temp_nickname = use_signal(|| member_info.member_info.preferred_nickname.clone());
    let mut input_element = use_signal(|| None as Option<Rc<MountedData>>);

    let save_changes = {
        let self_signing_key = self_signing_key.clone();
        let member_info = member_info.clone();

        move |new_value: String| {
            if new_value.is_empty() {
                warn!("Nickname cannot be empty");
                return;
            }

            let delta = if let Some(signing_key) = self_signing_key.clone() {
                let new_member_info = MemberInfo {
                    member_id: member_info.member_info.member_id.clone(),
                    version: member_info.member_info.version + 1,
                    preferred_nickname: new_value,
                };
                let new_authorized_member_info =
                    AuthorizedMemberInfo::new_with_member_key(new_member_info, &signing_key);
                Some(ChatRoomStateV1Delta {
                    member_info: Some(vec![new_authorized_member_info]),
                    ..Default::default()
                })
            } else {
                warn!("No signing key available");
                None
            };

            if let Some(delta) = delta {
                info!("Saving changes to nickname with delta: {}", new_value);

                // Get the owner key first
                let owner_key = CURRENT_ROOM.read().owner_key.clone();

                if let Some(owner_key) = owner_key {
                    // Use with_mut for atomic update
                    ROOMS.with_mut(|rooms| {
                        if let Some(room_data) = rooms.map.get_mut(&owner_key) {
                            info!("State before applying nickname delta: {:?}", room_data.room_state);
                            if let Err(e) = room_data.room_state.apply_delta(
                                &room_data.room_state.clone(),
                                &ChatRoomParametersV1 { owner: owner_key },
                                &Some(delta),
                            ) {
                                error!("Failed to apply delta: {:?}", e);
                            }
                            info!("State after applying nickname delta: {:?}", room_data.room_state);
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
        let temp_nickname = temp_nickname.clone();
        move |_| {
            let new_value = temp_nickname();
            save_changes(new_value);
        }
    };

    let on_keydown = {
        let save_changes = save_changes.clone();
        let temp_nickname = temp_nickname.clone();
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
            class: "field",
            label { class: "label", "Nickname" }
            div {
                class: if is_self { "control has-icons-right" } else { "control" },
                input {
                    class: "input",
                    value: "{temp_nickname}",
                    readonly: !is_self,
                    oninput: on_input,
                    onblur: on_blur,
                    onkeydown: on_keydown,
                    onmounted: move |cx| input_element.set(Some(cx.data())),
                }
                if is_self {
                    span {
                        class: "icon is-right",
                        i {
                            class: "fa-solid fa-pencil"
                        }
                    }
                }
            }
        }
    }
}
