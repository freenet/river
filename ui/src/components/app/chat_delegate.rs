use crate::components::app::{CURRENT_ROOM, ROOMS, WEB_API};
use dioxus::logger::tracing::{error, info, warn};
use dioxus::prelude::*;
use freenet_stdlib::client_api::ClientRequest::DelegateOp;
use freenet_stdlib::client_api::DelegateRequest;
use freenet_stdlib::prelude::{
    CodeHash, ContractInstanceId, Delegate, DelegateCode, DelegateContainer, DelegateKey,
    DelegateWasmAPIVersion, Parameters,
};
use futures::channel::oneshot;
use futures::future::{select, Either};
use river_core::chat_delegate::{
    ChatDelegateKey, ChatDelegateRequestMsg, ChatDelegateResponseMsg, RequestId, RoomKey,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

// Constant for the rooms storage key
pub const ROOMS_STORAGE_KEY: &[u8] = b"rooms_data";

// =============================================================================
// LEGACY DELEGATE MIGRATION
// When the delegate WASM changes (dependency updates, code changes), the delegate
// key changes and old secrets become inaccessible. This migration code attempts
// to load room data from known previous delegate keys and migrate it to the
// current delegate.
// TODO: Remove entries older than 3 months once users have migrated.
// =============================================================================

/// Previous delegate keys for migration. Each entry is (delegate_key, code_hash).
/// Add a new entry here whenever the delegate WASM changes (e.g., dependency updates).
const LEGACY_DELEGATES: &[([u8; 32], [u8; 32])] = &[
    // V1: Before signing API was added (code_hash "8n6hw3vmym1qrvpbaunfnn5t8v1xzdmuiyaprtckwbpz")
    (
        [
            26, 147, 48, 130, 14, 128, 108, 218, 84, 236, 167, 218, 178, 43, 132, 242, 12, 250,
            121, 62, 190, 97, 162, 97, 83, 18, 204, 110, 110, 188, 255, 246,
        ],
        [
            120, 57, 150, 189, 227, 188, 34, 53, 175, 254, 201, 222, 184, 160, 247, 233, 210, 31,
            161, 49, 220, 240, 3, 0, 11, 176, 63, 70, 125, 176, 248, 49,
        ],
    ),
    // V2: After scaffold 0.2.2 update with relaxed verify (2026-02-11)
    (
        [
            227, 173, 92, 91, 26, 130, 16, 137, 83, 107, 232, 77, 103, 67, 41, 179, 127, 70, 210,
            251, 163, 231, 2, 96, 8, 250, 232, 95, 53, 86, 81, 31,
        ],
        [
            207, 185, 119, 76, 3, 205, 149, 66, 73, 85, 173, 171, 112, 164, 29, 117, 117, 205, 51,
            18, 240, 159, 211, 241, 109, 110, 245, 72, 186, 140, 240, 81,
        ],
    ),
];

/// Check if a delegate key matches any known legacy delegate
pub fn is_legacy_delegate_key(key_bytes: &[u8]) -> bool {
    LEGACY_DELEGATES
        .iter()
        .any(|(dk, _)| dk.as_slice() == key_bytes)
}

// Prefixes for different pending request types
const SIGNING_KEY_PREFIX: &[u8] = b"__signing_key:";
const PUBLIC_KEY_PREFIX: &[u8] = b"__public_key:";
const SIGN_PREFIX: &[u8] = b"__sign:";

/// Atomic counter for generating unique request IDs
static REQUEST_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Generate a unique request ID for signing requests
pub fn generate_request_id() -> RequestId {
    REQUEST_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Registry for pending delegate requests.
/// Maps request keys to oneshot senders that will receive the response.
static PENDING_REQUESTS: std::sync::LazyLock<
    Mutex<HashMap<Vec<u8>, oneshot::Sender<ChatDelegateResponseMsg>>>,
> = std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Complete a pending delegate request with the given response.
/// Called by the response handler when a delegate response is received.
pub fn complete_pending_request(key: &ChatDelegateKey, response: ChatDelegateResponseMsg) -> bool {
    let key_bytes = key.as_bytes().to_vec();
    complete_pending_request_bytes(&key_bytes, response)
}

/// Complete a pending signing key store request.
pub fn complete_pending_signing_key_request(
    room_key: &RoomKey,
    response: ChatDelegateResponseMsg,
) -> bool {
    let mut key_bytes = SIGNING_KEY_PREFIX.to_vec();
    key_bytes.extend_from_slice(room_key);
    complete_pending_request_bytes(&key_bytes, response)
}

/// Complete a pending public key request.
pub fn complete_pending_public_key_request(
    room_key: &RoomKey,
    response: ChatDelegateResponseMsg,
) -> bool {
    let mut key_bytes = PUBLIC_KEY_PREFIX.to_vec();
    key_bytes.extend_from_slice(room_key);
    complete_pending_request_bytes(&key_bytes, response)
}

/// Complete a pending signing request using room_key and request_id for correlation.
pub fn complete_pending_sign_request(
    room_key: &RoomKey,
    request_id: RequestId,
    response: ChatDelegateResponseMsg,
) -> bool {
    let mut key_bytes = SIGN_PREFIX.to_vec();
    key_bytes.extend_from_slice(room_key);
    key_bytes.extend_from_slice(&request_id.to_le_bytes());
    complete_pending_request_bytes(&key_bytes, response)
}

/// Internal function to complete a pending request by key bytes.
fn complete_pending_request_bytes(key_bytes: &[u8], response: ChatDelegateResponseMsg) -> bool {
    if let Ok(mut pending) = PENDING_REQUESTS.lock() {
        if let Some(sender) = pending.remove(key_bytes) {
            if sender.send(response).is_ok() {
                info!(
                    "Completed pending request for key: {:?}",
                    String::from_utf8_lossy(key_bytes)
                );
                return true;
            }
        }
    }
    false
}

pub async fn set_up_chat_delegate() -> Result<(), String> {
    let delegate = create_chat_delegate_container();

    // Get a write lock on the API and use it directly
    let api_result = {
        let mut web_api = WEB_API.write();
        if let Some(api) = web_api.as_mut() {
            // Perform the operation while holding the lock
            info!("Registering chat delegate");
            api.send(DelegateOp(DelegateRequest::RegisterDelegate {
                delegate,
                cipher: DelegateRequest::DEFAULT_CIPHER,
                nonce: DelegateRequest::DEFAULT_NONCE,
            }))
            .await
        } else {
            Err(freenet_stdlib::client_api::Error::ConnectionClosed)
        }
    };

    match api_result {
        Ok(_) => {
            info!("Chat delegate registered successfully");
            // NOTE: We don't await load_rooms_from_delegate() here because it would
            // deadlock - it waits for a response that comes through the same message
            // loop that called us. Instead, we fire off the request and let the
            // response be handled by the response_handler through the message loop.
            //
            // The response handler will process GetResponse and populate ROOMS.
            fire_load_rooms_request().await;

            // Also try to migrate from legacy delegate (fire and forget)
            // TODO: Remove this after 2026-03-01
            fire_legacy_migration_request().await;

            Ok(())
        }
        Err(e) => Err(format!("Failed to register chat delegate: {}", e)),
    }
}

/// Fire a request to load rooms from delegate storage without waiting for response.
/// The response will be handled by the response_handler through the message loop.
/// This avoids deadlock when called from inside the message loop.
async fn fire_load_rooms_request() {
    info!("Firing request to load rooms from delegate storage");

    let request = ChatDelegateRequestMsg::GetRequest {
        key: ChatDelegateKey::new(ROOMS_STORAGE_KEY.to_vec()),
    };

    // Serialize and send the request without waiting for response
    let mut payload = Vec::new();
    if let Err(e) = ciborium::ser::into_writer(&request, &mut payload) {
        error!("Failed to serialize load rooms request: {}", e);
        return;
    }

    let delegate_code = DelegateCode::from(
        include_bytes!("../../../../target/wasm32-unknown-unknown/release/chat_delegate.wasm")
            .to_vec(),
    );
    let params = Parameters::from(Vec::<u8>::new());
    let delegate = Delegate::from((&delegate_code, &params));
    let delegate_key = delegate.key().clone();

    let self_contract_id = ContractInstanceId::new([0u8; 32]);
    let app_msg = freenet_stdlib::prelude::ApplicationMessage::new(self_contract_id, payload);

    let delegate_request = DelegateOp(DelegateRequest::ApplicationMessages {
        key: delegate_key,
        params: Parameters::from(Vec::<u8>::new()),
        inbound: vec![freenet_stdlib::prelude::InboundDelegateMsg::ApplicationMessage(app_msg)],
    });

    // Send without waiting for response
    let api_result = {
        let mut web_api = WEB_API.write();
        if let Some(api) = web_api.as_mut() {
            api.send(delegate_request).await
        } else {
            Err(freenet_stdlib::client_api::Error::ConnectionClosed)
        }
    };

    if let Err(e) = api_result {
        error!("Failed to send load rooms request: {}", e);
    } else {
        info!("Load rooms request sent, response will be handled by message loop");
    }
}

/// Load rooms from the delegate storage (with response waiting - use outside message loop only)
pub async fn load_rooms_from_delegate() -> Result<(), String> {
    info!("Loading rooms from delegate storage");

    // Create a get request for the rooms data
    let request = ChatDelegateRequestMsg::GetRequest {
        key: ChatDelegateKey::new(ROOMS_STORAGE_KEY.to_vec()),
    };

    // Send the request to the delegate
    match send_delegate_request(request).await {
        Ok(_) => {
            info!("Sent request to load rooms from delegate");
            Ok(())
        }
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

    // Get the current rooms data - clone the data to avoid holding the read lock
    let rooms_data = {
        let mut rooms_clone = ROOMS.read().clone();
        // Include the current room selection
        rooms_clone.current_room_key = CURRENT_ROOM.read().owner_key;
        let mut buffer = Vec::new();
        ciborium::ser::into_writer(&rooms_clone, &mut buffer)
            .map_err(|e| format!("Failed to serialize rooms: {}", e))?;
        buffer
    };

    // Create a store request for the rooms data
    let request = ChatDelegateRequestMsg::StoreRequest {
        key: ChatDelegateKey::new(ROOMS_STORAGE_KEY.to_vec()),
        value: rooms_data,
    };

    // Send the request to the delegate
    match send_delegate_request(request).await {
        Ok(ChatDelegateResponseMsg::StoreResponse { result, .. }) => result,
        Ok(other) => Err(format!("Unexpected response: {:?}", other)),
        Err(e) => Err(e),
    }
}

fn create_chat_delegate_container() -> DelegateContainer {
    let delegate_bytes =
        include_bytes!("../../../../target/wasm32-unknown-unknown/release/chat_delegate.wasm");
    let delegate_code = DelegateCode::from(delegate_bytes.to_vec());
    let params = Parameters::from(Vec::<u8>::new());
    let delegate = Delegate::from((&delegate_code, &params));
    DelegateContainer::Wasm(DelegateWasmAPIVersion::V1(delegate))
}

/// Extract the key from a request message for tracking purposes.
fn get_request_key(request: &ChatDelegateRequestMsg) -> Vec<u8> {
    match request {
        // Key-value storage operations
        ChatDelegateRequestMsg::StoreRequest { key, .. } => key.as_bytes().to_vec(),
        ChatDelegateRequestMsg::GetRequest { key } => key.as_bytes().to_vec(),
        ChatDelegateRequestMsg::DeleteRequest { key } => key.as_bytes().to_vec(),
        ChatDelegateRequestMsg::ListRequest => b"__list_request__".to_vec(),

        // Signing key management
        ChatDelegateRequestMsg::StoreSigningKey { room_key, .. } => {
            let mut key = SIGNING_KEY_PREFIX.to_vec();
            key.extend_from_slice(room_key);
            key
        }
        ChatDelegateRequestMsg::GetPublicKey { room_key } => {
            let mut key = PUBLIC_KEY_PREFIX.to_vec();
            key.extend_from_slice(room_key);
            key
        }

        // Signing operations - use prefix + room_key + request_id for uniqueness
        ChatDelegateRequestMsg::SignMessage {
            room_key,
            request_id,
            ..
        }
        | ChatDelegateRequestMsg::SignMember {
            room_key,
            request_id,
            ..
        }
        | ChatDelegateRequestMsg::SignBan {
            room_key,
            request_id,
            ..
        }
        | ChatDelegateRequestMsg::SignConfig {
            room_key,
            request_id,
            ..
        }
        | ChatDelegateRequestMsg::SignMemberInfo {
            room_key,
            request_id,
            ..
        }
        | ChatDelegateRequestMsg::SignSecretVersion {
            room_key,
            request_id,
            ..
        }
        | ChatDelegateRequestMsg::SignEncryptedSecret {
            room_key,
            request_id,
            ..
        }
        | ChatDelegateRequestMsg::SignUpgrade {
            room_key,
            request_id,
            ..
        } => {
            let mut key = SIGN_PREFIX.to_vec();
            key.extend_from_slice(room_key);
            key.extend_from_slice(&request_id.to_le_bytes());
            key
        }
    }
}

pub async fn send_delegate_request(
    request: ChatDelegateRequestMsg,
) -> Result<ChatDelegateResponseMsg, String> {
    info!("Sending delegate request: {:?}", request);

    // Get the key bytes for tracking this request
    let key_bytes = get_request_key(&request);

    // Create a oneshot channel to receive the response
    let (sender, receiver) = oneshot::channel();

    // Register the pending request
    {
        let mut pending = PENDING_REQUESTS
            .lock()
            .map_err(|e| format!("Failed to lock pending requests: {}", e))?;
        pending.insert(key_bytes.clone(), sender);
    }

    // Serialize the request
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&request, &mut payload)
        .map_err(|e| format!("Failed to serialize request: {}", e))?;

    info!("Serialized request payload size: {} bytes", payload.len());

    let delegate_code = DelegateCode::from(
        include_bytes!("../../../../target/wasm32-unknown-unknown/release/chat_delegate.wasm")
            .to_vec(),
    );
    let params = Parameters::from(Vec::<u8>::new());
    let delegate = Delegate::from((&delegate_code, &params));
    let delegate_key = delegate.key().clone(); // Get the delegate key for targeting the delegate request

    // FIXME: Not sure what this should be set to in this context
    let self_contract_id = ContractInstanceId::new([0u8; 32]);

    let app_msg = freenet_stdlib::prelude::ApplicationMessage::new(self_contract_id, payload);

    // Prepare the delegate request, targeting the delegate using its key
    let delegate_request = DelegateOp(DelegateRequest::ApplicationMessages {
        key: delegate_key, // Target the delegate instance
        params: Parameters::from(Vec::<u8>::new()),
        inbound: vec![freenet_stdlib::prelude::InboundDelegateMsg::ApplicationMessage(app_msg)],
    });

    // Get the API and send the request, releasing the lock before awaiting
    let api_result = {
        let mut web_api = WEB_API.write();
        if let Some(api) = web_api.as_mut() {
            // Send the request while holding the lock
            api.send(delegate_request).await
        } else {
            Err(freenet_stdlib::client_api::Error::ConnectionClosed)
        }
    };

    // Handle send errors - remove the pending request if send failed
    if let Err(e) = api_result {
        if let Ok(mut pending) = PENDING_REQUESTS.lock() {
            pending.remove(&key_bytes);
        }
        return Err(format!("Failed to send delegate request: {}", e));
    }

    info!("Request sent, waiting for response...");

    // Wait for the response with a timeout
    // Use WASM-compatible sleep from util module
    let timeout = Box::pin(crate::util::sleep(std::time::Duration::from_secs(30)));

    match select(receiver, timeout).await {
        Either::Left((response, _)) => match response {
            Ok(resp) => {
                info!("Received delegate response: {:?}", resp);
                Ok(resp)
            }
            Err(_) => Err("Response channel was cancelled".to_string()),
        },
        Either::Right((_, _)) => {
            // Timeout occurred - remove the pending request
            if let Ok(mut pending) = PENDING_REQUESTS.lock() {
                pending.remove(&key_bytes);
            }
            Err("Timeout waiting for delegate response".to_string())
        }
    }
}

// =============================================================================
// LEGACY DELEGATE MIGRATION IMPLEMENTATION
// =============================================================================

/// localStorage key to track whether legacy migration has been attempted
const LEGACY_MIGRATION_FLAG: &str = "river_legacy_migration_done";

/// Check if legacy migration has already been done (via localStorage)
fn is_legacy_migration_done() -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(window) = web_sys::window() {
            if let Ok(Some(storage)) = window.local_storage() {
                return storage
                    .get_item(LEGACY_MIGRATION_FLAG)
                    .ok()
                    .flatten()
                    .is_some();
            }
        }
        false
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        // Non-WASM: assume migration not needed (testing environment)
        true
    }
}

/// Mark legacy migration as done in localStorage
pub fn mark_legacy_migration_done() {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(window) = web_sys::window() {
            if let Ok(Some(storage)) = window.local_storage() {
                if let Err(e) = storage.set_item(LEGACY_MIGRATION_FLAG, "true") {
                    warn!("Failed to set legacy migration flag: {:?}", e);
                } else {
                    info!("Legacy migration marked as done");
                }
            }
        }
    }
}

/// Fire requests to load rooms from all known legacy delegates (fire and forget).
/// If any legacy delegate has room data, the response handler will migrate it.
async fn fire_legacy_migration_request() {
    // Check if migration has already been done
    if is_legacy_migration_done() {
        info!("Legacy migration already done, skipping");
        return;
    }

    info!(
        "Attempting to migrate data from {} legacy delegate(s)",
        LEGACY_DELEGATES.len()
    );

    for (i, (key_bytes, code_hash_bytes)) in LEGACY_DELEGATES.iter().enumerate() {
        let legacy_code_hash = CodeHash::new(*code_hash_bytes);
        let legacy_delegate_key = DelegateKey::new(*key_bytes, legacy_code_hash);

        let request = ChatDelegateRequestMsg::GetRequest {
            key: ChatDelegateKey::new(ROOMS_STORAGE_KEY.to_vec()),
        };

        let mut payload = Vec::new();
        if let Err(e) = ciborium::ser::into_writer(&request, &mut payload) {
            error!("Failed to serialize legacy migration request #{}: {}", i, e);
            continue;
        }

        let self_contract_id = ContractInstanceId::new([0u8; 32]);
        let app_msg = freenet_stdlib::prelude::ApplicationMessage::new(self_contract_id, payload);

        let delegate_request = DelegateOp(DelegateRequest::ApplicationMessages {
            key: legacy_delegate_key,
            params: Parameters::from(Vec::<u8>::new()),
            inbound: vec![freenet_stdlib::prelude::InboundDelegateMsg::ApplicationMessage(app_msg)],
        });

        let api_result = {
            let mut web_api = WEB_API.write();
            if let Some(api) = web_api.as_mut() {
                api.send(delegate_request).await
            } else {
                Err(freenet_stdlib::client_api::Error::ConnectionClosed)
            }
        };

        match api_result {
            Ok(_) => info!("Legacy migration request #{} sent", i),
            Err(e) => {
                info!(
                    "Could not send legacy migration request #{} (expected if delegate not present): {}",
                    i, e
                );
            }
        }
    }
}
