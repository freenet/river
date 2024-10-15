use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use crate::room_data::Rooms;

#[derive(Props, PartialEq)]
pub struct EditRoomModalProps {
    pub active_room: Signal<Option<VerifyingKey>>,
    pub on_save: EventHandler<(VerifyingKey, String, String)>,
    pub on_cancel: EventHandler<()>,
}

#[component]
pub fn EditRoomModal(props: EditRoomModalProps) -> Element {
    let rooms = use_shared_state::<Rooms>().unwrap();
    let room_name = use_state(String::new);
    let room_description = use_state(String::new);

    use_effect(move || {
        if let Some(key) = props.active_room.read().as_ref() {
            let rooms = rooms.read();
            if let Some(room) = rooms.map.get(key) {
                room_name.set(room.room_state.configuration.configuration.name.clone());
                room_description.set(room.room_state.configuration.configuration.description.clone());
            }
        }
    });

    let save_room = move |_| {
        if let Some(key) = props.active_room.read().as_ref() {
            props.on_save.call((*key, room_name.get().clone(), room_description.get().clone()));
        }
    };

    rsx! {
        div {
            class: "modal {if props.active_room.read().is_some() { "is-active" } else { "" }}",
            div {
                class: "modal-background",
                onclick: move |_| props.on_cancel.call(()),
            }
            div {
                class: "modal-card",
                header {
                    class: "modal-card-head",
                    p {
                        class: "modal-card-title",
                        "Edit Room"
                    }
                }
                section {
                    class: "modal-card-body",
                    div {
                        class: "field",
                        label {
                            class: "label",
                            "Room Name"
                        }
                        div {
                            class: "control",
                            input {
                                class: "input",
                                r#type: "text",
                                value: "{room_name}",
                                oninput: move |evt| room_name.set(evt.value.clone()),
                            }
                        }
                    }
                    div {
                        class: "field",
                        label {
                            class: "label",
                            "Room Description"
                        }
                        div {
                            class: "control",
                            textarea {
                                class: "textarea",
                                value: "{room_description}",
                                oninput: move |evt| room_description.set(evt.value.clone()),
                            }
                        }
                    }
                }
                footer {
                    class: "modal-card-foot",
                    button {
                        class: "button is-success",
                        onclick: save_room,
                        "Save"
                    }
                    button {
                        class: "button",
                        onclick: move |_| props.on_cancel.call(()),
                        "Cancel"
                    }
                }
            }
        }
    }
}
