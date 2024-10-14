use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use bs58;
use web_sys;
use wasm_bindgen_futures::spawn_local;
use wasm_bindgen::JsCast;
use js_sys;

#[component]
pub fn NotMemberNotification(user_verifying_key: VerifyingKey) -> Element {
    let encoded_key = format!("river:user:vk:{}", bs58::encode(user_verifying_key.as_bytes()).into_string());
    let button_text = use_signal(|| "Copy".to_string());
    let is_copying = use_signal(|| false);

    let encoded_key_for_closure = encoded_key.clone();
    let copy_to_clipboard = move |_| {
        let key = encoded_key_for_closure.clone();
        let mut button_text = button_text.clone();
        let mut is_copying = is_copying.clone();
        is_copying.set(true);
        spawn_local(async move {
            if let Some(window) = web_sys::window() {
                if let Ok(navigator) = window.navigator().dyn_into::<web_sys::Navigator>() {
                    let clipboard = navigator.clipboard();
                    let _ = wasm_bindgen_futures::JsFuture::from(clipboard.write_text(&key)).await;
                    button_text.set("Copied!".to_string());
                    // Reset the button text after 2 seconds
                    let promise = js_sys::Promise::new(&mut |resolve, _| {
                        let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                            &resolve,
                            2000,
                        );
                    });
                    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
                    button_text.set("Copy".to_string());
                    is_copying.set(false);
                }
            }
        });
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
