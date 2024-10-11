use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use bs58;
use web_sys;
use wasm_bindgen_futures::{spawn_local, JsFuture};
use wasm_bindgen::JsCast;

#[component]
pub fn NotMemberNotification(user_verifying_key: VerifyingKey) -> Element {
    let encoded_key = format!("river:user:vk:{}", bs58::encode(user_verifying_key.as_bytes()).into_string());
    let button_text = use_state(cx, || "Copy to Clipboard".to_string());

    rsx! {
        div { class: "notification is-info",
            p { "You are not a member of this room. You need to be invited by a current room member." }
            p { "Your verifying key: " }
            code { "{encoded_key}" }
            button {
                class: "button is-small is-primary mt-2",
                onclick: move |_| {
                    let key = encoded_key.clone();
                    let button_text_clone = button_text.clone();
                    spawn_local(async move {
                        if let Some(window) = web_sys::window() {
                            if let Ok(navigator) = window.navigator().dyn_into::<web_sys::Navigator>() {
                                let clipboard = navigator.clipboard();
                                let _ = wasm_bindgen_futures::JsFuture::from(clipboard.write_text(&key)).await;
                                button_text_clone.set("Copied!".to_string());
                            }
                        }
                    });
                },
                "{button_text}"
            }
        }
    }
}
