use dioxus::prelude::*;

#[component]
pub fn InviteMemberModal(is_active: Signal<bool>) -> Element {
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
                    h1 { class: "title is-4 mb-3", "Invite Member" }
                    // Placeholder elements
                    div {
                        class: "field",
                        label { class: "label", "Invitation Link" }
                        div {
                            class: "control",
                            input {
                                class: "input",
                                type: "text",
                                placeholder: "Generated invitation link will appear here",
                                readonly: true
                            }
                        }
                    }
                    div {
                        class: "field",
                        div {
                            class: "control",
                            button {
                                class: "button is-primary",
                                "Generate Invitation Link"
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
use dioxus::prelude::*;

#[component]
pub fn InviteMemberModal(is_active: Signal<bool>) -> Element {
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
                    h1 { class: "title is-4 mb-3", "Invite Member" }
                    // Placeholder elements
                    div {
                        class: "field",
                        label { class: "label", "Invitation Link" }
                        div {
                            class: "control",
                            input {
                                class: "input",
                                type: "text",
                                placeholder: "Generated invitation link will appear here",
                                readonly: true
                            }
                        }
                    }
                    div {
                        class: "field",
                        div {
                            class: "control",
                            button {
                                class: "button is-primary",
                                "Generate Invitation Link"
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
