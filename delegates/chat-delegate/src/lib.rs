#![allow(unexpected_cfgs)]

mod context;
mod handlers;
mod models;
mod utils;

use context::*;
use freenet_stdlib::prelude::{
    delegate, ApplicationMessage, DelegateContext, DelegateError, DelegateInterface,
    GetSecretRequest, InboundDelegateMsg, OutboundDelegateMsg, Parameters, SecretsId,
    SetSecretRequest,
};
use handlers::*;
use models::*;
use utils::*;

// Custom logging module to handle different environments
mod logging;



use river_core::chat_delegate::*;
use serde::ser::SerializeTuple;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Chat delegate for storing and retrieving data in the Freenet secret storage.
///
/// This delegate provides a key-value store interface for chat applications,
/// maintaining an index of keys for each application and handling storage,
/// retrieval, deletion, and listing operations.
pub struct ChatDelegate;

#[delegate]
impl DelegateInterface for ChatDelegate {
    fn process(
        _parameters: Parameters<'static>,
        attested: Option<&'static [u8]>,
        message: InboundDelegateMsg,
    ) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
        let message_type = match message {
            InboundDelegateMsg::ApplicationMessage(_) => "application message",
            InboundDelegateMsg::GetSecretResponse(_) => "get secret response",
            InboundDelegateMsg::UserResponse(_) => "user response",
            InboundDelegateMsg::GetSecretRequest(_) => "get secret request",
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
                    handle_application_message(app_msg, &origin)
                }
            }

            InboundDelegateMsg::GetSecretResponse(response) => handle_get_secret_response(response),

            InboundDelegateMsg::UserResponse(_) => {
                logging::info("Received unexpected UserResponse");
                Err(DelegateError::Other(
                    "unexpected message type: UserResponse".into(),
                ))
            }

            InboundDelegateMsg::GetSecretRequest(_) => {
                // We don't handle direct get secret requests
                logging::info("Received unexpected GetSecretRequest");
                Err(DelegateError::Other(
                    "unexpected message type: GetSecretRequest".into(),
                ))
            }
        };

        match &result {
            Ok(msgs) => logging::info(&format!("Process returning {} messages", msgs.len())),
            Err(e) => logging::info(&format!("Process returning error: {e}")),
        }

        result
    }
}
