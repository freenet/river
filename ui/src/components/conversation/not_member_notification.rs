use crate::crypto_values::CryptoValue;
use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use wasm_bindgen::JsCast;
use web_sys;

#[component]
pub fn NotMemberNotification(user_verifying_key: VerifyingKey) -> Element {
    let encoded_key = use_signal(|| {
        CryptoValue::VerifyingKey(user_verifying_key.clone()).to_encoded_string()
    });
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
                "You are not a member of this room. Share this key with a current member so they can invite you:"
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
