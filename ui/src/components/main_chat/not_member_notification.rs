use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use bs58;
use web_sys;
use wasm_bindgen::JsCast;
#[component]
pub fn NotMemberNotification(user_verifying_key: VerifyingKey) -> Element {
    let encoded_key = format!("river:user:vk:{}", bs58::encode(user_verifying_key.as_bytes()).into_string());
    let button_text = use_signal(|| "Copy".to_string());
    let is_copying = use_signal(|| false);

    let copy_to_clipboard = move |_| {
        if let Some(window) = web_sys::window() {
            if let Ok(navigator) = window.navigator().dyn_into::<web_sys::Navigator>() {
                let clipboard = navigator.clipboard();
                let _ = clipboard.write_text(&encoded_key);
                button_text.set("Copied!".to_string());
                is_copying.set(true);
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
                div { class: "control is-expanded",
                    input {
                        class: "input",
                        r#type: "text",
                        value: "{encoded_key}",
                        readonly: "true"
                    }
                }
                div { class: "control",
                    button {
                        class: "button is-info",
                        onclick: copy_to_clipboard,
                        disabled: "{*is_copying.read()}",
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
