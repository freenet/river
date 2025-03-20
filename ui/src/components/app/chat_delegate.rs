use crate::components::app::WEB_API;
use freenet_stdlib::client_api::ClientRequest::DelegateOp;
use freenet_stdlib::client_api::DelegateRequest;
use freenet_stdlib::prelude::{Delegate, DelegateCode, DelegateContainer, DelegateWasmAPIVersion, Parameters};

pub async fn set_up_chat_delegate() -> Result<(), String> {
    // Load the chat delegate WASM bytes
    let delegate_wasm = include_bytes!("../../../../target/wasm32-unknown-unknown/release/chat_delegate.wasm");
    
    let delegate_wasm = DelegateCode::from(delegate_wasm);

    let delegate = Delegate::from((delegate_wasm, Parameters::from([])));

    let delegate = DelegateContainer::Wasm(DelegateWasmAPIVersion::V1(delegate));

    // Register the delegate with the server
    // Note: For this simple delegate, we don't need encryption, so cipher and nonce are empty
    if let Some(ref mut api) = &mut *WEB_API.write() {
        let _ = api.send(DelegateOp(DelegateRequest::RegisterDelegate {
            delegate,
            // TODO: This is questionable
            cipher: DelegateRequest::DEFAULT_CIPHER,
            nonce: DelegateRequest::DEFAULT_NONCE,
        })).await;
    }
    
    Ok(())
}
