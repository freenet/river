use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use bs58;
use web_sys::*;
use wasm_bindgen_futures::spawn_local;
use wasm_bindgen::JsCast;

#[component]
pub fn NotMemberNotification(user_verifying_key: VerifyingKey) -> Element {
    let encoded_key = format!("river:user:vk:{}", bs58::encode(user_verifying_key.as_bytes()).into_string());

    rsx! {
        div { class: "notification is-info",
            p { "You are not a member of this room. You need to be invited by a current room member." }
            p { "Your verifying key: " }
            code { "{encoded_key}" }
            button {
                class: "button is-small is-primary mt-2",
                onclick: move |_| {
                    let key = encoded_key.clone();
                    spawn_local(async move {
                        if let Some(window) = web_sys::window() {
                            if let Ok(navigator) = window.navigator().dyn_into::<web_sys::Navigator>() {
                                if let Some(clipboard) = navigator.clipboard() {
                                    let _ = clipboard.write_text(&key).await;
                                }
                            }
                        }
                    });
                },
                "Copy to Clipboard"
            }
        }
    }
}
