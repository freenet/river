#![allow(unexpected_cfgs)]

mod context;
mod handlers;
mod models;
mod utils;

use context::*;
use freenet_stdlib::prelude::{
    delegate, ApplicationMessage, DelegateContext, DelegateCtx, DelegateError, DelegateInterface,
    InboundDelegateMsg, MessageOrigin, OutboundDelegateMsg, Parameters,
};
use handlers::*;
use models::*;
use utils::*;

// Custom logging module to handle different environments
mod logging;

use river_core::chat_delegate::*;
use serde::{Deserialize, Serialize};

// Legacy delegate migration entries are now managed in legacy_delegates.toml
// at the repo root. The CI workflow reads that file directly.
// See: cargo make add-migration, cargo make check-migration

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
        origin: Option<MessageOrigin>,
        message: InboundDelegateMsg,
    ) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
        let message_type = match message {
            InboundDelegateMsg::ApplicationMessage(_) => "application message",
            InboundDelegateMsg::UserResponse(_) => "user response",
            InboundDelegateMsg::GetContractResponse(_) => "get contract response",
            InboundDelegateMsg::PutContractResponse(_) => "put contract response",
            InboundDelegateMsg::UpdateContractResponse(_) => "update contract response",
            InboundDelegateMsg::SubscribeContractResponse(_) => "subscribe contract response",
            InboundDelegateMsg::ContractNotification(_) => "contract notification",
            InboundDelegateMsg::DelegateMessage(_) => "delegate message",
        };

        logging::info(&format!("Delegate received message of type {message_type}"));

        // Verify that origin is provided - this is the authenticated origin
        let caller_origin: Origin = match origin {
            Some(MessageOrigin::WebApp(contract_id)) => Origin(contract_id.as_bytes().to_vec()),
            None => {
                logging::info("Missing message origin");
                return Err(DelegateError::Other(format!(
                    "missing message origin for message type: {:?}",
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
                    handle_application_message(ctx, app_msg, &caller_origin)
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
            | InboundDelegateMsg::SubscribeContractResponse(_)
            | InboundDelegateMsg::ContractNotification(_)
            | InboundDelegateMsg::DelegateMessage(_) => {
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
