use crate::components::app::{WEB_API, ROOMS};
use dioxus::logger::tracing::{info, warn, error};
use dioxus::prelude::Readable;
use freenet_stdlib::client_api::ClientRequest::DelegateOp;
use freenet_stdlib::client_api::DelegateRequest;
use freenet_stdlib::prelude::{Delegate, DelegateCode, DelegateContainer, DelegateWasmAPIVersion, Parameters};
use river_common::chat_delegate::ChatDelegateRequestMsg;

// Constant for the rooms storage key
pub const ROOMS_STORAGE_KEY: &[u8] = b"rooms_data";

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
                // Load rooms from delegate after successful registration
                load_rooms_from_delegate().await?;
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

/// Load rooms from the delegate storage
pub async fn load_rooms_from_delegate() -> Result<(), String> {
    info!("Loading rooms from delegate storage");
    
    // Get the contract instance ID for the app
    let app_id = get_app_instance_id()?;
    
    // Create a get request for the rooms data
    let request = ChatDelegateRequestMsg::GetRequest { 
        key: ROOMS_STORAGE_KEY.to_vec() 
    };
    
    // Send the request to the delegate
    match send_delegate_request(app_id, request).await {
        Ok(_) => {
            info!("Sent request to load rooms from delegate");
            Ok(())
        },
        Err(e) => {
            warn!("Failed to load rooms from delegate: {}", e);
            // Don't fail the app if we can't load rooms
            Ok(())
        }
    }
}

/// Save rooms to the delegate storage
pub async fn save_rooms_to_delegate() -> Result<(), String> {
    info!("Saving rooms to delegate storage");
    
    // Get the current rooms data
    let rooms_data = {
        let rooms = ROOMS.read();
        let mut buffer = Vec::new();
        ciborium::ser::into_writer(&*rooms, &mut buffer)
            .map_err(|e| format!("Failed to serialize rooms: {}", e))?;
        buffer
    };
    
    // Get the contract instance ID for the app
    let app_id = get_app_instance_id()?;
    
    // Create a store request for the rooms data
    let request = ChatDelegateRequestMsg::StoreRequest { 
        key: ROOMS_STORAGE_KEY.to_vec(),
        value: rooms_data,
    };
    
    // Send the request to the delegate
    send_delegate_request(app_id, request).await
}

/// Helper function to get the app's contract instance ID
fn get_app_instance_id() -> Result<freenet_stdlib::prelude::ContractInstanceId, String> {
    // For now, we'll use a fixed ID since we don't have a real contract ID
    // In a real app, this would be the contract ID of the app
    let mut bytes = [0u8; 32];
    bytes[0] = 1; // Just a simple identifier
    Ok(freenet_stdlib::prelude::ContractInstanceId::new(bytes))
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
        // Get the delegate key from the code in create_chat_delegate_container
        let delegate_code = DelegateCode::from(include_bytes!("../../../../target/wasm32-unknown-unknown/release/chat_delegate.wasm").to_vec());
        let params = Parameters::from(Vec::<u8>::new());
        let delegate = Delegate::from((&delegate_code, &params));
        let delegate_key = delegate.key().clone();
        
        api.send(DelegateOp(DelegateRequest::ApplicationMessages {
            key: delegate_key,
            params: Parameters::from(Vec::<u8>::new()),
            inbound: vec![freenet_stdlib::prelude::InboundDelegateMsg::ApplicationMessage(app_msg)],
        }))
        .await
        .map_err(|e| format!("Failed to send delegate request: {}", e))?;
        
        Ok(())
    } else {
        Err("Web API not initialized".to_string())
    }
}
