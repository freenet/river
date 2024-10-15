use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use crate::room_data::{Rooms, Room};

#[derive(Props)]
pub struct EditRoomModalProps {
    pub active_room: Option<VerifyingKey>,
    pub on_save: Box<dyn Fn(VerifyingKey, String, String)>,
    pub on_cancel: Box<dyn Fn()>,
}

#[component]
pub fn EditRoomModal(cx: Scope<EditRoomModalProps>) -> Element {
    let rooms = use_context::<Signal<Rooms>>();
    let mut room_name = use_state(|| String::new());
    let mut room_description = use_state(|| String::new());

    if let Some(key) = cx.props.active_room {
        let room = rooms.get().get(&key).unwrap();
        room_name.set(room.name.clone());
        room_description.set(room.description.clone());
    }

    let save_room = move |_| {
        if let Some(key) = cx.props.active_room {
            (cx.props.on_save)(key, room_name.get().clone(), room_description.get().clone());
        }
    };

    cx.render(rsx! {
        div {
            class: "modal",
            class: if cx.props.active_room.is_some() { "is-active" } else { "" },
            div {
                class: "modal-background",
                onclick: move |_| (cx.props.on_cancel)(),
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
                        onclick: move |_| (cx.props.on_cancel)(),
                        "Cancel"
                    }
                }
            }
        }
    })
}
