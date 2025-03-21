use crate::components::app::WEB_API;
use dioxus::logger::tracing::info;
use freenet_stdlib::client_api::ClientRequest::DelegateOp;
use freenet_stdlib::client_api::DelegateRequest;
use freenet_stdlib::prelude::{Delegate, DelegateCode, DelegateContainer, DelegateWasmAPIVersion, Parameters};

pub async fn set_up_chat_delegate() -> Result<(), String> {
    let delegate = create_chat_delegate_container();

    // Register the delegate with the server
    // Note: For this simple delegate, we don't need encryption, so cipher and nonce are empty
    if let Some(ref mut api) = &mut *WEB_API.write() {
        match api.send(DelegateOp(DelegateRequest::RegisterDelegate {
            delegate,
            cipher: DelegateRequest::DEFAULT_CIPHER,
            nonce: DelegateRequest::DEFAULT_NONCE,
        })).await {
            Ok(_) => {
                info!("Chat delegate registered successfully");
                Ok(())
            },
            Err(e) => {
                Err(format!("Failed to register chat delegate: {}", e))
            }
        }
    } else {
        Err("Web API not initialized".to_string())
    }
}

fn create_chat_delegate_container() -> DelegateContainer {
    let delegate_bytes = include_bytes!("../../../../target/wasm32-unknown-unknown/release/chat_delegate.wasm");
    let delegate_code = DelegateCode::from(delegate_bytes.to_vec());
    let params = Parameters::from(Vec::<u8>::new());
    let delegate = Delegate::from((&delegate_code, &params));
    DelegateContainer::Wasm(DelegateWasmAPIVersion::V1(delegate))
}

pub async fn send_delegate_request(
    app_id: freenet_stdlib::prelude::ContractInstanceId,
    request: river_common::chat_delegate::ChatDelegateRequestMsg,
) -> Result<(), String> {
    // Serialize the request
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&request, &mut payload)
        .map_err(|e| format!("Failed to serialize request: {}", e))?;

    // Create the application message
    let app_msg = freenet_stdlib::prelude::ApplicationMessage::new(app_id, payload);

    // Send the request to the delegate
    if let Some(ref mut api) = &mut *WEB_API.write() {
        api.send(DelegateOp(DelegateRequest::SendMessage {
            app_msg,
            // TODO: This is questionable
            cipher: DelegateRequest::DEFAULT_CIPHER,
            nonce: DelegateRequest::DEFAULT_NONCE,
        }))
        .await
        .map_err(|e| format!("Failed to send delegate request: {}", e))?;
        
        Ok(())
    } else {
        Err("Web API not initialized".to_string())
    }
}
