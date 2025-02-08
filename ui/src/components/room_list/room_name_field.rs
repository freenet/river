use crate::room_data::{CurrentRoom, Rooms};
use river_common::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_common::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};
use dioxus::logger::tracing::*;
use dioxus::prelude::*;
use dioxus_core::Event;
use freenet_scaffold::ComposableState;
use log::info;

#[component]
pub fn RoomNameField(config: Configuration, is_owner: bool) -> Element {
    let mut rooms = use_context::<Signal<Rooms>>();
    let current_room = use_context::<Signal<CurrentRoom>>();

    let mut room_name = use_signal(|| config.name.clone());

    let update_room_name = move |evt: Event<FormData>| {
        if !is_owner {
            return;
        }

        info!("Updating room name");
        let new_name = evt.value().to_string();
        if !new_name.is_empty() {
            room_name.set(new_name.clone());
            let mut new_config = config.clone();
            new_config.name = new_name;
            new_config.configuration_version += 1;

            let mut rooms_write_guard = rooms.write();
            let owner_key = current_room.read().owner_key.expect("No owner key");

            if let Some(room_data) = rooms_write_guard.map.get_mut(&owner_key) {
                let signing_key = &room_data.self_sk;
                let new_authorized_config = AuthorizedConfigurationV1::new(new_config, signing_key);

                let delta = ChatRoomStateV1Delta {
                    configuration: Some(new_authorized_config),
                    ..Default::default()
                };

                info!("Applying delta to room state");
                let parent_state = room_data.room_state.clone();
                match ComposableState::apply_delta(
                    &mut room_data.room_state,
                    &parent_state,
                    &ChatRoomParametersV1 { owner: owner_key },
                    &Some(delta),
                ) {
                    Ok(_) => info!("Delta applied successfully"),
                    Err(e) => error!("Failed to apply delta: {:?}", e),
                }
            } else {
                error!("Room state not found for current room");
            }
        } else {
            error!("Room name is empty");
        }
    };

    rsx! {
        div { class: "field",
            label { class: "label", "Room Name" }
            div { class: "control",
                input {
                    class: "input",
                    value: "{room_name}",
                    readonly: !is_owner,
                    onchange: update_room_name,
                }
            }
        }
    }
}
