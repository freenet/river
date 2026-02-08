#![allow(unexpected_cfgs)]

mod context;
mod handlers;
mod models;
mod utils;

use context::*;
use freenet_stdlib::prelude::{
    delegate, ApplicationMessage, DelegateContext, DelegateCtx, DelegateError, DelegateInterface,
    InboundDelegateMsg, OutboundDelegateMsg, Parameters,
};
use handlers::*;
use models::*;
use utils::*;

// Custom logging module to handle different environments
mod logging;

use river_core::chat_delegate::*;
use serde::{Deserialize, Serialize};

/// Chat delegate for storing and retrieving data in the Freenet secret storage.
///
/// This delegate provides a key-value store interface for chat applications,
/// using the host function API for direct secret access (no message round-trips).
pub struct ChatDelegate;

#[delegate]
impl DelegateInterface for ChatDelegate {
    fn process(
        ctx: &mut DelegateCtx,
        _parameters: Parameters<'static>,
        attested: Option<&'static [u8]>,
        message: InboundDelegateMsg,
    ) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
        let message_type = match message {
            InboundDelegateMsg::ApplicationMessage(_) => "application message",
            InboundDelegateMsg::UserResponse(_) => "user response",
            InboundDelegateMsg::GetContractResponse(_) => "get contract response",
            InboundDelegateMsg::PutContractResponse(_) => "put contract response",
            InboundDelegateMsg::UpdateContractResponse(_) => "update contract response",
            InboundDelegateMsg::SubscribeContractResponse(_) => "subscribe contract response",
        };

        logging::info(&format!("Delegate received message of type {message_type}"));

        // Verify that attested is provided - this is the authenticated origin
        let origin: Origin = match attested {
            Some(origin) => Origin(origin.to_vec()),
            None => {
                logging::info("Missing attested origin");
                return Err(DelegateError::Other(format!(
                    "missing attested origin for message type: {:?}",
                    message_type
                )));
            }
        };

        let result = match message {
            InboundDelegateMsg::ApplicationMessage(app_msg) => {
                if app_msg.processed {
                    logging::info("Received already processed message");
                    Err(DelegateError::Other(
                        "cannot process an already processed message".into(),
                    ))
                } else {
                    handle_application_message(ctx, app_msg, &origin)
                }
            }

            InboundDelegateMsg::UserResponse(_) => {
                logging::info("Received unexpected UserResponse");
                Err(DelegateError::Other(
                    "unexpected message type: UserResponse".into(),
                ))
            }

            InboundDelegateMsg::GetContractResponse(_)
            | InboundDelegateMsg::PutContractResponse(_)
            | InboundDelegateMsg::UpdateContractResponse(_)
            | InboundDelegateMsg::SubscribeContractResponse(_) => {
                logging::info(&format!(
                    "Received unexpected contract response: {message_type}"
                ));
                Err(DelegateError::Other(format!(
                    "unexpected message type: {message_type}"
                )))
            }
        };

        match &result {
            Ok(msgs) => logging::info(&format!("Process returning {} messages", msgs.len())),
            Err(e) => logging::info(&format!("Process returning error: {e}")),
        }

        result
    }
}
