use std::collections::HashMap;
use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_stdlib::prelude::{delegate, DelegateError, DelegateInterface, InboundDelegateMsg, OutboundDelegateMsg, Parameters};
use serde::{Deserialize, Serialize};

pub struct RoomDelegate;

#[delegate]
impl DelegateInterface for RoomDelegate {

    fn process(
        parameters: Parameters<'static>,
        attested: Option<&'static [u8]>,
        message: InboundDelegateMsg
    ) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
        match message {
            InboundDelegateMsg::ApplicationMessage(app_msg) => {
                let context : Context = ciborium::from_reader(app_msg.context.as_ref())
                    .map_err(|e| DelegateError::Deser(format!("Failed to deserialize context: {e}")))?;

                todo!()
            }
            InboundDelegateMsg::GetSecretResponse(_get_secret_response) => {
                todo!()
            }
            InboundDelegateMsg::UserResponse(_user_resposne) => {
                todo!()
            }
            InboundDelegateMsg::GetSecretRequest(_get_secret_request) => {
                todo!()
            }
        }
    }

}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Context {
    rooms : HashMap<VerifyingKey, StoredRoom>,
}

struct StoredRoom {
    self_sk : SigningKey,
}