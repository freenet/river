#![allow(unexpected_cfgs)]

mod context;
mod handlers;
mod models;
mod subscription;
mod utils;
mod versioning;

use context::*;
use freenet_stdlib::prelude::{
    delegate, ApplicationMessage, DelegateContext, DelegateCtx, DelegateError, DelegateInterface,
    InboundDelegateMsg, MessageOrigin, OutboundDelegateMsg, Parameters,
};
use handlers::*;
use models::*;
use subscription::handle_contract_notification;
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
        // `ApplicationMessage` and `ContractNotification` anyway, so
        // behaviour is unchanged for known variants.
        let message_type = match &message {
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

        let result = match message {
            // ContractNotifications are delivered by the runtime when a
            // subscribed contract's state changes. They have no
            // client-attested origin (the runtime is the sender), so they
            // bypass the origin check.
            InboundDelegateMsg::ContractNotification(notification) => {
                handle_contract_notification(ctx, notification)
            }
            // SubscribeContractResponse is also a runtime delivery — log and
            // drop. The actual subscription bookkeeping is done in the
            // `EnsureRoomSubscription` request handler.
            InboundDelegateMsg::SubscribeContractResponse(resp) => {
                logging::info(&format!(
                    "SubscribeContractResponse for {:?}: ok={}",
                    resp.contract_id,
                    resp.result.is_ok()
                ));
                Ok(vec![])
            }
            // ApplicationMessage requires an authenticated origin.
            InboundDelegateMsg::ApplicationMessage(app_msg) => {
                let caller_origin = check_origin(origin, message_type)?;
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

/// Verify that the request carried an authenticated origin.
///
/// `MessageOrigin` became `#[non_exhaustive]` and gained a
/// `Delegate(DelegateKey)` variant in freenet-stdlib 0.5.0/0.6.0 so the
/// runtime can attest inter-delegate callers. The chat delegate is driven
/// exclusively by the River web app (which sends its calls with
/// `MessageOrigin::WebApp`), so any `Delegate` origin is rejected as
/// unauthorised.
fn check_origin(
    origin: Option<MessageOrigin>,
    message_type: &str,
) -> Result<Origin, DelegateError> {
    match origin {
        Some(MessageOrigin::WebApp(contract_id)) => Ok(Origin(contract_id.as_bytes().to_vec())),
        Some(MessageOrigin::Delegate(caller_key)) => {
            logging::info(&format!(
                "Rejecting inter-delegate call from caller {caller_key}"
            ));
            Err(DelegateError::Other(format!(
                "chat-delegate does not accept inter-delegate calls (caller: {caller_key})"
            )))
        }
        None => {
            logging::info("Missing message origin");
            Err(DelegateError::Other(format!(
                "missing message origin for message type: {message_type:?}"
            )))
        }
        _ => {
            logging::info("Unknown MessageOrigin variant");
            Err(DelegateError::Other(
                "unknown MessageOrigin variant — \
                 chat-delegate must be rebuilt against a newer freenet-stdlib"
                    .into(),
            ))
        }
    }
}
