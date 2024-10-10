use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use bs58;
use web_sys;
use wasm_bindgen_futures::{spawn_local, JsFuture};

#[component]
pub fn NotMemberNotification(user_verifying_key: VerifyingKey) -> Element {
    let encoded_key = format!("river:user:vk:{}", bs58::encode(user_verifying_key.as_bytes()).into_string());
    
    let clipboard_opt = web_sys::window().map(|window| window.navigator().clipboard());

    rsx! {
        div { class: "notification is-info",
            p { "You are not a member of this room. You need to be invited by a current room member." }
            p { "Your verifying key: " }
            code { "{encoded_key}" }
            button {
                class: "button is-small is-primary mt-2",
                onclick: move |_| {
                    let clipboard_opt = clipboard_opt.clone();
                    let key = encoded_key.clone();
                    spawn_local(async move {
                        if let Some(clipboard) = clipboard_opt {
                            let promise = clipboard.write_text(&key);
                            let _ = JsFuture::from(promise).await;
                        }
                    });
                },
                "Copy to Clipboard"
            }
        }
    }
}
