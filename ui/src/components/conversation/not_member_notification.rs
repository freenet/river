use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use bs58;
use web_sys;
use wasm_bindgen::JsCast;
use crate::constants::KEY_VERSION_PREFIX;

#[component]
pub fn NotMemberNotification(user_verifying_key: VerifyingKey) -> Element {
    let encoded_key = use_signal(|| format!("{}{}", KEY_VERSION_PREFIX, bs58::encode(user_verifying_key.as_bytes()).into_string()));
    let mut button_text = use_signal(|| "Copy".to_string());

    let copy_to_clipboard = move |_| {
        if let Some(window) = web_sys::window() {
            if let Ok(navigator) = window.navigator().dyn_into::<web_sys::Navigator>() {
                let clipboard = navigator.clipboard();
                let _ = clipboard.write_text(&encoded_key.read());
                button_text.set("Copied!".to_string());
            }
        }
    };

    rsx! {
        div { class: "box has-background-light border-left-warning",
            p { class: "mb-3",
                "You are not a member of this room. You need to be invited by a current room member."
            }
            p { class: "mb-2 has-text-weight-bold", "Your verifying key:" }
            div { class: "field has-addons",
                p { class: "control is-expanded",
                    input {
                        class: "input small-font-input",
                        r#type: "text",
                        value: "{encoded_key}",
                        readonly: "true"
                    }
                }
                p { class: "control",
                    button {
                        class: "button is-info copy-button",
                        onclick: copy_to_clipboard,
                        span { class: "icon",
                            i { class: "fas fa-copy" }
                        }
                        span {
                            "{button_text}"
                        }
                    }
                }
            }
        }
    }
}
