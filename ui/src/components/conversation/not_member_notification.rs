use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use river_core::crypto_values::CryptoValue;
use wasm_bindgen::JsCast;
use web_sys;

#[component]
pub fn NotMemberNotification(user_verifying_key: VerifyingKey) -> Element {
    let encoded_key =
        use_signal(|| CryptoValue::VerifyingKey(user_verifying_key).to_encoded_string());
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
        div { class: "mx-4 mb-4 p-4 bg-warning-bg border-l-4 border-yellow-500 rounded-r-lg",
            p { class: "mb-3 text-sm text-text",
                "You are not a member of this room. Share this key with a current member so they can invite you:"
            }
            p { class: "mb-2 text-sm font-semibold text-text", "Your verifying key:" }
            div { class: "flex gap-2",
                input {
                    class: "flex-1 px-3 py-2 bg-surface border border-border rounded-lg text-xs text-text font-mono",
                    r#type: "text",
                    value: "{encoded_key}",
                    readonly: "true"
                }
                button {
                    class: "px-4 py-2 bg-accent hover:bg-accent-hover text-white text-sm font-medium rounded-lg transition-colors flex items-center gap-2",
                    onclick: copy_to_clipboard,
                    i { class: "fas fa-copy" }
                    span { "{button_text}" }
                }
            }
        }
    }
}
