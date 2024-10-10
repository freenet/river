use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use bs58;
use web_sys::window;

#[component]
pub fn NotMemberNotification(user_verifying_key: VerifyingKey) -> Element {
    let encoded_key = format!("river:user:vk:{}", bs58::encode(user_verifying_key.as_bytes()).into_string());

    let copy_to_clipboard = move |_| {
        if let Some(window) = window() {
            if let Some(clipboard) = window.navigator().clipboard() {
                let _ = clipboard.write_text(&encoded_key);
            }
        }
    };

    rsx! {
        div { class: "notification is-info",
            p { "You are not a member of this room. You need to be invited by a current room member." }
            p { "Your verifying key: " }
            code { "{encoded_key}" }
            button {
                class: "button is-small is-primary mt-2",
                onclick: copy_to_clipboard,
                "Copy to Clipboard"
            }
        }
    }
}
