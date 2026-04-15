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
        // `InboundDelegateMsg` is `#[non_exhaustive]` since freenet-stdlib 0.6.0.
        // The wildcard arm classifies future variants as "unknown" for
        // telemetry; the routing match below rejects anything other than
        // `ApplicationMessage` anyway, so behaviour is unchanged for known
        // variants.
        let message_type = match message {
            InboundDelegateMsg::ApplicationMessage(_) => "application message",
            InboundDelegateMsg::UserResponse(_) => "user response",
            InboundDelegateMsg::GetContractResponse(_) => "get contract response",
            InboundDelegateMsg::PutContractResponse(_) => "put contract response",
            InboundDelegateMsg::UpdateContractResponse(_) => "update contract response",
            InboundDelegateMsg::SubscribeContractResponse(_) => "subscribe contract response",
            InboundDelegateMsg::ContractNotification(_) => "contract notification",
            InboundDelegateMsg::DelegateMessage(_) => "delegate message",
            _ => "unknown message",
        };

        logging::info(&format!("Delegate received message of type {message_type}"));

        // Verify that origin is provided — this is the authenticated origin.
        //
        // `MessageOrigin` became `#[non_exhaustive]` and gained a
        // `Delegate(DelegateKey)` variant in freenet-stdlib 0.5.0/0.6.0 so
        // the runtime can attest inter-delegate callers. The chat delegate
        // is driven exclusively by the River web app (which sends its
        // calls with `MessageOrigin::WebApp`), so any `Delegate` origin is
        // rejected as unauthorised — there is no scenario today where
        // another delegate should be able to invoke chat_delegate.
        let caller_origin: Origin = match origin {
            Some(MessageOrigin::WebApp(contract_id)) => Origin(contract_id.as_bytes().to_vec()),
            Some(MessageOrigin::Delegate(caller_key)) => {
                logging::info(&format!(
                    "Rejecting inter-delegate call from caller {caller_key}"
                ));
                return Err(DelegateError::Other(format!(
                    "chat-delegate does not accept inter-delegate calls (caller: {caller_key})"
                )));
            }
            None => {
                logging::info("Missing message origin");
                return Err(DelegateError::Other(format!(
                    "missing message origin for message type: {:?}",
                    message_type
                )));
            }
            _ => {
                logging::info("Unknown MessageOrigin variant");
                return Err(DelegateError::Other(
                    "unknown MessageOrigin variant — \
                     chat-delegate must be rebuilt against a newer freenet-stdlib"
                        .into(),
                ));
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
            // `InboundDelegateMsg` is `#[non_exhaustive]` since
            // freenet-stdlib 0.6.0. Future variants are rejected with a
            // clear error — the chat delegate should be rebuilt against
            // the newer stdlib if it needs to handle them.
            _ => {
                logging::info(&format!(
                    "Unknown InboundDelegateMsg variant: {message_type}"
                ));
                Err(DelegateError::Other(format!(
                    "unknown InboundDelegateMsg variant: {message_type}"
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
