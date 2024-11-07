use dioxus::prelude::*;
use crate::components::app::CreateRoomModalSignal;
use crate::room_data::{CurrentRoom, Rooms};

#[component]
pub fn CreateRoomModal() -> Element {
    let mut rooms = use_context::<Signal<Rooms>>();
    let mut current_room = use_context::<Signal<CurrentRoom>>();
    let mut create_room_signal = use_context::<Signal<CreateRoomModalSignal>>();
    
    let mut room_name = use_signal(String::new);
    let mut nickname = use_signal(String::new);

    let create_room = move |_| {
        let name = room_name.read().clone();
        if name.is_empty() {
            return;
        }

        let mut rooms_write = rooms.write();
        let self_sk = rooms_write.map.values().next().unwrap().self_sk.clone();
        let new_room_key = rooms_write.create_new_room_with_name(self_sk, name);
        
        // Set the owner's nickname if provided
        if let Some(room_data) = rooms_write.map.get_mut(&new_room_key) {
            if let Some(member_info) = room_data.room_state.member_info.member_info.first_mut() {
                member_info.member_info.preferred_nickname = nickname.read().clone();
            }
        }
        
        current_room.set(CurrentRoom { owner_key: Some(new_room_key) });
        
        // Reset and close modal
        room_name.set(String::new());
        nickname.set(String::new());
        create_room_signal.write().show = false;
    };

    rsx! {
        div {
            class: format_args!("modal {}", if create_room_signal.read().show { "is-active" } else { "" }),
            div {
                class: "modal-background",
                onclick: move |_| {
                    create_room_signal.write().show = false;
                }
            }
            div {
                class: "modal-content",
                div {
                    class: "box",
                    h1 { class: "title is-4 mb-3", "Create New Room" }

                    div { class: "field",
                        label { class: "label", "Room Name" }
                        div { class: "control",
                            input {
                                class: "input",
                                value: "{room_name}",
                                onchange: move |evt| room_name.set(evt.value().to_string())
                            }
                        }
                    }

                    div { class: "field",
                        label { class: "label", "Your Nickname" }
                        div { class: "control",
                            input {
                                class: "input",
                                value: "{nickname}",
                                onchange: move |evt| nickname.set(evt.value().to_string())
                            }
                        }
                    }

                    div { class: "field",
                        div { class: "control",
                            button {
                                class: "button is-primary",
                                onclick: create_room,
                                "Create Room"
                            }
                        }
                    }
                }
            }
            button {
                class: "modal-close is-large",
                onclick: move |_| {
                    create_room_signal.write().show = false;
                }
            }
        }
    }
}
