use crate::components::app::WEB_API;
use freenet_stdlib::client_api::ClientRequest::DelegateOp;
use freenet_stdlib::client_api::DelegateRequest;
use freenet_stdlib::prelude::{Delegate, DelegateCode, DelegateContainer, DelegateWasmAPIVersion, Parameters};
use crate::room_data::Rooms;

pub async fn set_up_chat_delegate() -> Result<Rooms, String> {
    let delegate = create_chat_delegate_container();

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

fn create_chat_delegate_container() -> DelegateContainer {
    let delegate_bytes = include_bytes!("../../../../target/wasm32-unknown-unknown/release/chat_delegate.wasm");
    let delegate_code = DelegateCode::from(delegate_bytes.to_vec());
    let params = Parameters::from(Vec::<u8>::new());
    let delegate = Delegate::from((&delegate_code, &params));
    DelegateContainer::Wasm(DelegateWasmAPIVersion::V1(delegate))
}