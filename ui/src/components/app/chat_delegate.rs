use futures::SinkExt;
use freenet_stdlib::client_api::{ClientRequest, DelegateRequest};
use freenet_stdlib::client_api::ClientRequest::DelegateOp;
use freenet_stdlib::prelude::{DelegateContainer, DelegateWasmAPIVersion};
use crate::components::app::WEB_API;

pub async fn set_up_chat_delegate() {
    // Can you complete this function to register a delegate with the server? AI!
    WEB_API.write().send(DelegateOp(DelegateRequest::RegisterDelegate {
        delegate: todo!(),
        cipher: todo!(),
        nonce: todo!(),
    }))
}