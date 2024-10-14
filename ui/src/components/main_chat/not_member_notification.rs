use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use bs58;
use web_sys;
use wasm_bindgen_futures::{spawn_local};
use wasm_bindgen::JsCast;

#[component]
pub fn NotMemberNotification(user_verifying_key: VerifyingKey) -> Element {
    let encoded_key = format!("river:user:vk:{}", bs58::encode(user_verifying_key.as_bytes()).into_string());
    let button_text = use_signal(|| "Copy to Clipboard".to_string());

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
                        onclick: move |_| {
                            let key = encoded_key.clone();
                            let mut button_text_clone = button_text.clone();
                            spawn_local(async move {
                                if let Some(window) = web_sys::window() {
                                    if let Ok(navigator) = window.navigator().dyn_into::<web_sys::Navigator>() {
                                        let clipboard = navigator.clipboard();
                                        let _ = wasm_bindgen_futures::JsFuture::from(clipboard.write_text(&key)).await;
                                        button_text_clone.set("Copied!".to_string());
                                        // Reset the button text after 2 seconds
                                        let _ = wasm_bindgen_futures::JsFuture::from(js_sys::Promise::new(&mut |resolve, _| {
                                            web_sys::window().unwrap().set_timeout_with_callback_and_timeout_and_arguments_0(
                                                &resolve,
                                                2000,
                                            ).unwrap();
                                        })).await;
                                        button_text_clone.set("Copy".to_string());
                                    }
                                }
                            });
                        },
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
