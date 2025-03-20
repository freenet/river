use freenet_stdlib::client_api::DelegateRequest;
use freenet_stdlib::client_api::ClientRequest::DelegateOp;
use freenet_stdlib::prelude::{DelegateCode, DelegateContainer, DelegateWasmAPIVersion, Parameters};
use crate::components::app::WEB_API;

pub async fn set_up_chat_delegate() {
    // Load the chat delegate WASM bytes
    let delegate_bytes = include_bytes!("../../../../target/wasm32-unknown-unknown/release/chat_delegate.wasm");
    
    // Create delegate code from the WASM bytes
    let delegate_code = DelegateCode::from(delegate_bytes.to_vec());
    
    // Create empty parameters
    let parameters = Parameters::from(vec![]);
    
    // Create a delegate container with the WASM code and API version
    let delegate = DelegateContainer::Wasm(DelegateWasmAPIVersion::V1);
    
    // Create the delegate with code and parameters
    let delegate = delegate.with_delegate((&delegate_code, &parameters).into());
    
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
