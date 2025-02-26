//! Room subscription management for Freenet API

use crate::components::app::freenet_api::types::{SyncStatus, SYNC_STATUS};
use crate::constants::ROOM_CONTRACT_WASM;
use crate::room_data::{Rooms, RoomSyncStatus};
use crate::util::to_cbor_vec;
use dioxus::prelude::{use_context, use_effect, Signal, UnboundedSender, Writable};
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::client_api::{ClientRequest, ContractRequest};
use freenet_stdlib::prelude::{ContractCode, ContractInstanceId, ContractKey, Parameters};
use futures::SinkExt;
use log::{debug, error, info};
use river_common::room_state::ChatRoomParametersV1;

/// Set up room subscription and update logic
pub fn setup_room_subscriptions(request_sender: UnboundedSender<ClientRequest<'static>>) {
    // Watch for changes to Rooms signal
    let mut rooms = use_context::<Signal<Rooms>>();
    let request_sender = request_sender.clone();

    use_effect(move || {
        {
            let mut rooms = rooms.write();
            for room in rooms.map.values_mut() {
                // Subscribe to room if not already subscribed
                if matches!(room.sync_status, RoomSyncStatus::Unsubscribed) {
                    info!("Subscribing to room with contract key: {:?}", room.contract_key);
                    room.sync_status = RoomSyncStatus::Subscribing;
                    let subscribe_request = ContractRequest::Subscribe {
                        key: room.contract_key,
                        summary: None,
                    };
                    let mut sender = request_sender.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        if let Err(e) = sender.send(subscribe_request.into()).await {
                            error!("Failed to subscribe to room: {}", e);
                        } else {
                            debug!("Successfully sent subscription request");
                        }
                    });
                }
                let state_bytes = to_cbor_vec(&room.room_state);
                let update_request = ContractRequest::Update {
                    key: room.contract_key,
                    data: freenet_stdlib::prelude::UpdateData::State(
                        state_bytes.clone().into(),
                    ),
                };
                info!("Sending room state update for key: {:?}", room.contract_key);
                debug!("Update size: {} bytes", state_bytes.len());
                let mut sender = request_sender.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    if let Err(e) = sender.send(update_request.into()).await {
                        error!("Failed to send room update: {}", e);
                    } else {
                        debug!("Successfully sent room state update");
                    }
                });
            }
        }
    });
}

/// Prepares chat room parameters for contract creation
pub fn prepare_chat_room_parameters(room_owner: &VerifyingKey) -> Parameters {
    let chat_room_params = ChatRoomParametersV1 { owner: *room_owner };
    to_cbor_vec(&chat_room_params).into()
}

/// Generates a contract key from parameters and WASM code
pub fn generate_contract_key(parameters: Parameters) -> ContractKey {
    let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
    let instance_id = ContractInstanceId::from_params_and_code(parameters, contract_code);
    ContractKey::from(instance_id)
}

/// Request room state with retry logic
pub async fn request_room_state(
    sender: &mut UnboundedSender<ClientRequest<'static>>, 
    room_owner: &VerifyingKey
) -> Result<(), String> {
    info!("Requesting room state for room owned by {:?}", room_owner);

    // Check if WebSocket is ready
    if let Ok(status_ref) = SYNC_STATUS.try_read() {
        if !matches!(*status_ref, SyncStatus::Connected | SyncStatus::Syncing) {
            let error_msg = format!("Cannot request room state: WebSocket not connected (status: {:?})", *status_ref);
            error!("{}", error_msg);
            return Err(error_msg);
        }
    } else {
        let error_msg = "Cannot request room state: Unable to read sync status".to_string();
        error!("{}", error_msg);
        return Err(error_msg);
    }

    let parameters = prepare_chat_room_parameters(room_owner);
    let contract_key = generate_contract_key(parameters);
    let get_request = ContractRequest::Get {
        key: contract_key,
        return_contract_code: false
    };
    debug!("Generated contract key: {:?}", contract_key);

    // Add retry logic for sending the request
    let mut retries = 0;
    const MAX_RETRIES: u8 = 3;

    while retries < MAX_RETRIES {
        match sender.clone().send(get_request.clone().into()).await {
            Ok(_) => {
                info!("Successfully sent request for room state");
                return Ok(());
            },
            Err(e) => {
                let error_msg = format!("Failed to send request (attempt {}/{}): {}",
                                        retries + 1, MAX_RETRIES, e);
                error!("{}", error_msg);

                if retries == MAX_RETRIES - 1 {
                    // Last attempt failed, update status and return error
                    *SYNC_STATUS.write() = SyncStatus::Error(error_msg.clone());
                    return Err(error_msg);
                }

                // Wait before retrying
                retries += 1;
                let _ = futures_timer::Delay::new(std::time::Duration::from_millis(500)).await;
            }
        }
    }

    // This should never be reached due to the return in the last retry
    Err("Failed to send request after maximum retries".to_string())
}
