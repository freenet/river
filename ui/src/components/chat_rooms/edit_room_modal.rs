use dioxus::prelude::*;

#[component]
pub fn EditRoomModal(is_active: Signal<bool>) -> Element {
    rsx! {
        div {
            class: if *is_active.read() { "modal is-active" } else { "modal" },
            div {
                class: "modal-background",
                onclick: move |_| {
                    is_active.set(false);
                }
            }
            div {
                class: "modal-content",
                div {
                    class: "box",
                    h1 { class: "title is-4 mb-3", "Edit Room" }
                    // Placeholder elements
                    div {
                        class: "field",
                        label { class: "label", "Room Name" }
                        div {
                            class: "control",
                            input {
                                class: "input",
                                r#type: "text",
                                placeholder: "Enter room name"
                            }
                        }
                    }
                    div {
                        class: "field",
                        div {
                            class: "control",
                            button {
                                class: "button is-primary",
                                "Save Changes"
                            }
                        }
                    }
                }
            }
            button {
                class: "modal-close is-large",
                onclick: move |_| {
                    is_active.set(false);
                }
            }
        }
    }
}
