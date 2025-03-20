use futures::SinkExt;
use freenet_stdlib::client_api::{ClientRequest, DelegateRequest};
use freenet_stdlib::client_api::ClientRequest::DelegateOp;
use freenet_stdlib::prelude::{DelegateContainer, DelegateWasmAPIVersion};
use crate::components::app::WEB_API;

pub async fn set_up_chat_delegate() {
    // Load the chat delegate WASM bytes
    let delegate_bytes = include_bytes!("../../../../target/wasm32-unknown-unknown/release/chat_delegate.wasm");
    
    // Create a delegate container with the WASM bytes
    let delegate = DelegateContainer {
        wasm: delegate_bytes.to_vec(),
        api_version: DelegateWasmAPIVersion::V1,
    };
    
    // Register the delegate with the server
    // Note: For this simple delegate, we don't need encryption, so cipher and nonce are empty
    let _ = WEB_API.write().send(DelegateOp(DelegateRequest::RegisterDelegate {
        delegate,
        cipher: vec![],
        nonce: vec![],
    })).await;
}
