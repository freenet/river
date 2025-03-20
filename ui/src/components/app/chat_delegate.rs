use freenet_stdlib::client_api::DelegateRequest;
use freenet_stdlib::client_api::ClientRequest::DelegateOp;
use freenet_stdlib::prelude::{Delegate, DelegateContainer, DelegateWasmAPIVersion};
use dioxus::prelude::Readable;
use crate::components::app::WEB_API;

pub async fn set_up_chat_delegate() {
    // Load the chat delegate WASM bytes
    let delegate_bytes = include_bytes!("../../../../target/wasm32-unknown-unknown/release/chat_delegate.wasm");
    
    // Create a delegate container with the WASM bytes
    let delegate = DelegateContainer::Wasm(
        DelegateWasmAPIVersion::V1(Delegate::from(delegate_bytes.to_vec()))
    );
    
    // Register the delegate with the server
    // Note: For this simple delegate, we don't need encryption, so cipher and nonce are empty
    if let Some(mut api) = &*WEB_API.write() {
        let _ = api.send(DelegateOp(DelegateRequest::RegisterDelegate {
            delegate,
            cipher: [0u8; 32],
            nonce: [0u8; 24],
        })).await;
    }
}
