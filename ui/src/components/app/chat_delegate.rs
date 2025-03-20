use crate::components::app::WEB_API;
use freenet_stdlib::client_api::ClientRequest::DelegateOp;
use freenet_stdlib::client_api::DelegateRequest;
use freenet_stdlib::prelude::{DelegateCode, DelegateContainer, Parameters};
use crate::components::app::chat_delegate_helpers;

pub async fn set_up_chat_delegate() -> Result<(), String> {
    // Load the chat delegate WASM bytes
    let delegate_bytes = include_bytes!("../../../../target/wasm32-unknown-unknown/release/chat_delegate.wasm");
    
    // Create empty parameters
    let parameters = Parameters::from(vec![]);

    // Create the delegate container using our helper function
    let delegate = chat_delegate_helpers::create_delegate_container(&delegate_bytes[..], parameters)
        .map_err(|e| e.to_string())?;

    // Register the delegate with the server
    // Note: For this simple delegate, we don't need encryption, so cipher and nonce are empty
    if let Some(mut api) = &*WEB_API.write() {
        let _ = api.send(DelegateOp(DelegateRequest::RegisterDelegate {
            delegate,
            // TODO: This is questionable
            cipher: DelegateRequest::DEFAULT_CIPHER,
            nonce: DelegateRequest::DEFAULT_NONCE,
        })).await;
    }
    
    Ok(())
}

pub fn create_chat_delegate(delegate_bytes: &[u8]) -> Result<DelegateContainer, std::io::Error> {
    // Create empty parameters for the chat delegate
    let parameters = Parameters::from(Vec::<u8>::new());

    // Create the delegate container using our helper function
    let delegate = chat_delegate_helpers::create_delegate_container(delegate_bytes, parameters)?;

    Ok(delegate)
}
