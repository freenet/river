use futures::SinkExt;
use freenet_stdlib::client_api::DelegateRequest;
use freenet_stdlib::client_api::ClientRequest::DelegateOp;
use freenet_stdlib::prelude::{DelegateContainer, DelegateWasmAPIVersion};
use dioxus::prelude::Readable;
use crate::components::app::WEB_API;

pub async fn set_up_chat_delegate() {
    // Load the chat delegate WASM bytes
    let delegate_bytes = include_bytes!("../../../../target/wasm32-unknown-unknown/release/chat_delegate.wasm");
    
    // Create a delegate container with the WASM bytes
    let delegate = DelegateContainer::new(delegate_bytes.to_vec(), DelegateWasmAPIVersion::V1);
    
    // Register the delegate with the server
    // Note: For this simple delegate, we don't need encryption, so cipher and nonce are empty
    if let Some(api) = &*WEB_API.read() {
        let _ = api.send(DelegateOp(DelegateRequest::RegisterDelegate {
            delegate,
            cipher: [0u8; 32],
            nonce: [0u8; 24],
        })).await;
    }
}
