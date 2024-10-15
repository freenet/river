use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use crate::room_data::Rooms;

#[derive(Props)]
pub struct EditRoomModalProps {
    pub active_room: Signal<Option<VerifyingKey>>,
    pub on_save: Box<dyn Fn(VerifyingKey, String, String)>,
    pub on_cancel: Box<dyn Fn()>,
}

#[component]
pub fn EditRoomModal(props: EditRoomModalProps) -> Element {
    let rooms = use_shared_state::<Rooms>()?;
    let room_name = use_state(String::new);
    let room_description = use_state(String::new);

    use_effect(move || {
        if let Some(key) = props.active_room.read().as_ref() {
            let rooms = rooms.read();
            if let Some(room) = rooms.get(key) {
                room_name.set(room.name.clone());
                room_description.set(room.description.clone());
            }
        }
    });

    let save_room = move |_| {
        if let Some(key) = props.active_room.read().as_ref() {
            (props.on_save)(*key, room_name.get().clone(), room_description.get().clone());
        }
    };

    rsx! {
        div {
            class: "modal",
            class: if props.active_room.read().is_some() { "is-active" } else { "" },
            div {
                class: "modal-background",
                onclick: move |_| (props.on_cancel)(),
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
                                oninput: move |evt| room_name.set(evt.value().to_string()),
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
                                oninput: move |evt| room_description.set(evt.value().to_string()),
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
                        onclick: move |_| (props.on_cancel)(),
                        "Cancel"
                    }
                }
            }
        }
    }
}
