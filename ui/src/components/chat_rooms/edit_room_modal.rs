use dioxus::prelude::*;
use ed25519_dalek::SigningKey;
use crate::components::app::EditRoomModalActive;
use crate::room_data::{RoomData, Rooms};

#[component]
fn EditRoomModal() -> Element {
    let rooms = use_context::<Signal<Rooms>>();
    let edit_room_signal: Signal<EditRoomModalActive> = use_context::<Signal<EditRoomModalActive>>();
    let editing_room: Memo<Option<RoomData>> = use_memo(move || {
        if let Some(editing_room_vk) = edit_room_signal.read().room {
            rooms.read().map.iter().find_map(|(room_vk, room_data)| {
                if &editing_room_vk == room_vk {
                    Some(room_data.clone())
                } else {
                    None
                }
            })
        } else {
            None
        }
    });
    
    rsx! {
        div {
            class: if edit_room_signal.read().active { "modal is-active" } else { "modal" },
            div {
                class: "modal-background",
                onclick: move |_| {
                    edit_room_signal.write().active = false;
                }
            }
            div {
                class: "modal-content",
                div {
                    class: "box",
                    if let Some(room_data) = editing_room() {
                        rsx! {
                            h1 { class: "title is-4 mb-3", "Edit Room" }
                            
                            div {
                                class: "field",
                                label { class: "label is-medium", "Room Name" }
                                div {
                                    class: "control",
                                    input {
                                        class: "input",
                                        value: "{room_data.room_state.configuration.configuration.room_name}",
                                        readonly: true
                                    }
                                }
                            }

                            div {
                                class: "field",
                                label { class: "label is-medium", "Room ID" }
                                div {
                                    class: "control",
                                    input {
                                        class: "input",
                                        value: "{room_data.room_state.configuration.configuration.room_id}",
                                        readonly: true
                                    }
                                }
                            }

                            div {
                                class: "field",
                                label { class: "label is-medium", "Max Members" }
                                div {
                                    class: "control",
                                    input {
                                        class: "input",
                                        value: "{room_data.room_state.configuration.configuration.max_members}",
                                        readonly: true
                                    }
                                }
                            }

                            div {
                                class: "field",
                                label { class: "label is-medium", "Owner Member ID" }
                                div {
                                    class: "control",
                                    input {
                                        class: "input",
                                        value: "{room_data.room_state.configuration.configuration.owner_member_id}",
                                        readonly: true
                                    }
                                }
                            }

                            // Add more fields as needed
                        }
                    } else {
                        rsx! {
                            div { 
                                class: "notification is-warning",
                                "Room information not available" 
                            }
                        }
                    }
                }
            }
            button {
                class: "modal-close is-large",
                onclick: move |_| {
                    edit_room_signal.write().active = false;
                }
            }
        }
    }
}
