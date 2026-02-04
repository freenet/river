use super::*;
use ed25519_dalek::{Signer, SigningKey};
use freenet_stdlib::prelude::{ContractInstanceId, DelegateCtx};
use river_core::chat_delegate::{RequestId, RoomKey};

/// Handle an application message using the host function API for direct secret access.
pub(crate) fn handle_application_message(
    ctx: &mut DelegateCtx,
    app_msg: ApplicationMessage,
    origin: &Origin,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    // Deserialize the request message
    let request: ChatDelegateRequestMsg = ciborium::from_reader(app_msg.payload.as_slice())
        .map_err(|e| DelegateError::Deser(format!("Failed to deserialize request: {e}")))?;

    match request {
        // Key-value storage operations
        ChatDelegateRequestMsg::StoreRequest { key, value } => {
            logging::info(
                format!(
                    "Delegate received StoreRequest key: {key:?}, value_len: {}",
                    value.len()
                )
                .as_str(),
            );
            handle_store_request(ctx, origin, key, value, app_msg.app)
        }
        ChatDelegateRequestMsg::GetRequest { key } => {
            logging::info(format!("Delegate received GetRequest key: {key:?}").as_str());
            handle_get_request(ctx, origin, key, app_msg.app)
        }
        ChatDelegateRequestMsg::DeleteRequest { key } => {
            logging::info(format!("Delegate received DeleteRequest key: {key:?}").as_str());
            handle_delete_request(ctx, origin, key, app_msg.app)
        }
        ChatDelegateRequestMsg::ListRequest => {
            logging::info("Delegate received ListRequest");
            handle_list_request(ctx, origin, app_msg.app)
        }

        // Signing key management
        ChatDelegateRequestMsg::StoreSigningKey {
            room_key,
            signing_key_bytes,
        } => {
            logging::info(
                format!("Delegate received StoreSigningKey for room: {room_key:?}").as_str(),
            );
            handle_store_signing_key(ctx, origin, room_key, signing_key_bytes, app_msg.app)
        }
        ChatDelegateRequestMsg::GetPublicKey { room_key } => {
            logging::info(
                format!("Delegate received GetPublicKey for room: {room_key:?}").as_str(),
            );
            handle_get_public_key(ctx, origin, room_key, app_msg.app)
        }

        // Signing operations - all include request_id for correlation
        ChatDelegateRequestMsg::SignMessage {
            room_key,
            request_id,
            message_bytes,
        } => {
            logging::info(format!("Delegate received SignMessage for room: {room_key:?}").as_str());
            handle_sign_request(
                ctx,
                origin,
                room_key,
                request_id,
                message_bytes,
                app_msg.app,
            )
        }
        ChatDelegateRequestMsg::SignMember {
            room_key,
            request_id,
            member_bytes,
        } => {
            logging::info(format!("Delegate received SignMember for room: {room_key:?}").as_str());
            handle_sign_request(ctx, origin, room_key, request_id, member_bytes, app_msg.app)
        }
        ChatDelegateRequestMsg::SignBan {
            room_key,
            request_id,
            ban_bytes,
        } => {
            logging::info(format!("Delegate received SignBan for room: {room_key:?}").as_str());
            handle_sign_request(ctx, origin, room_key, request_id, ban_bytes, app_msg.app)
        }
        ChatDelegateRequestMsg::SignConfig {
            room_key,
            request_id,
            config_bytes,
        } => {
            logging::info(format!("Delegate received SignConfig for room: {room_key:?}").as_str());
            handle_sign_request(ctx, origin, room_key, request_id, config_bytes, app_msg.app)
        }
        ChatDelegateRequestMsg::SignMemberInfo {
            room_key,
            request_id,
            member_info_bytes,
        } => {
            logging::info(
                format!("Delegate received SignMemberInfo for room: {room_key:?}").as_str(),
            );
            handle_sign_request(
                ctx,
                origin,
                room_key,
                request_id,
                member_info_bytes,
                app_msg.app,
            )
        }
        ChatDelegateRequestMsg::SignSecretVersion {
            room_key,
            request_id,
            record_bytes,
        } => {
            logging::info(
                format!("Delegate received SignSecretVersion for room: {room_key:?}").as_str(),
            );
            handle_sign_request(ctx, origin, room_key, request_id, record_bytes, app_msg.app)
        }
        ChatDelegateRequestMsg::SignEncryptedSecret {
            room_key,
            request_id,
            secret_bytes,
        } => {
            logging::info(
                format!("Delegate received SignEncryptedSecret for room: {room_key:?}").as_str(),
            );
            handle_sign_request(ctx, origin, room_key, request_id, secret_bytes, app_msg.app)
        }
        ChatDelegateRequestMsg::SignUpgrade {
            room_key,
            request_id,
            upgrade_bytes,
        } => {
            logging::info(format!("Delegate received SignUpgrade for room: {room_key:?}").as_str());
            handle_sign_request(
                ctx,
                origin,
                room_key,
                request_id,
                upgrade_bytes,
                app_msg.app,
            )
        }
    }
}

// ============================================================================
// Key-Value Storage Handlers
// ============================================================================

/// Handle a store request - stores value and updates the index
fn handle_store_request(
    ctx: &mut DelegateCtx,
    origin: &Origin,
    key: ChatDelegateKey,
    value: Vec<u8>,
    app: ContractInstanceId,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    // Create a unique key for this origin's data
    let secret_key = create_origin_key(origin, &key);
    let index_key = create_index_key(origin);

    // Store the value directly via host function
    // Note: In WASM, set_secret returns true on success. In non-WASM tests, it always returns false.
    #[cfg(target_family = "wasm")]
    if !ctx.set_secret(&secret_key, &value) {
        return Err(DelegateError::Other(
            "Failed to store secret via host function".into(),
        ));
    }
    #[cfg(not(target_family = "wasm"))]
    let _ = ctx.set_secret(&secret_key, &value);

    logging::info(&format!(
        "Stored secret with key length {}",
        secret_key.len()
    ));

    // Update the key index
    let mut key_index = get_key_index(ctx, &index_key);
    if !key_index.keys.contains(&key) {
        key_index.keys.push(key.clone());
        set_key_index(ctx, &index_key, &key_index)?;
        logging::info(&format!(
            "Added key to index, now has {} keys",
            key_index.keys.len()
        ));
    }

    // Create response for the client
    let response = ChatDelegateResponseMsg::StoreResponse {
        key,
        result: Ok(()),
        value_size: value.len(),
    };

    Ok(vec![create_app_response(&response, app)?])
}

/// Handle a get request - retrieves value directly
fn handle_get_request(
    ctx: &mut DelegateCtx,
    origin: &Origin,
    key: ChatDelegateKey,
    app: ContractInstanceId,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    // Create a unique key for this origin's data
    let secret_key = create_origin_key(origin, &key);

    // Get the value directly via host function
    let value = ctx.get_secret(&secret_key);
    logging::info(&format!(
        "Retrieved secret, value present: {}",
        value.is_some()
    ));

    // Create response for the client
    let response = ChatDelegateResponseMsg::GetResponse { key, value };

    Ok(vec![create_app_response(&response, app)?])
}

/// Handle a delete request - removes value and updates the index
fn handle_delete_request(
    ctx: &mut DelegateCtx,
    origin: &Origin,
    key: ChatDelegateKey,
    app: ContractInstanceId,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    // Create keys
    let secret_key = create_origin_key(origin, &key);
    let index_key = create_index_key(origin);

    // Remove the secret via host function
    ctx.remove_secret(&secret_key);
    logging::info("Removed secret");

    // Update the key index
    let mut key_index = get_key_index(ctx, &index_key);
    key_index.keys.retain(|k| k != &key);
    set_key_index(ctx, &index_key, &key_index)?;
    logging::info(&format!(
        "Removed key from index, now has {} keys",
        key_index.keys.len()
    ));

    // Create response for the client
    let response = ChatDelegateResponseMsg::DeleteResponse {
        key,
        result: Ok(()),
    };

    Ok(vec![create_app_response(&response, app)?])
}

/// Handle a list request - returns all keys for this origin
fn handle_list_request(
    ctx: &mut DelegateCtx,
    origin: &Origin,
    app: ContractInstanceId,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    let index_key = create_index_key(origin);

    // Get the key index directly
    let key_index = get_key_index(ctx, &index_key);
    logging::info(&format!(
        "Returning list with {} keys",
        key_index.keys.len()
    ));

    // Create response for the client
    let response = ChatDelegateResponseMsg::ListResponse {
        keys: key_index.keys,
    };

    Ok(vec![create_app_response(&response, app)?])
}

// ============================================================================
// Signing Operations Handlers
// ============================================================================
//
// SECURITY MODEL:
//
// Signing keys are stored and retrieved using origin attestation to ensure that
// only the contract that stored a key can request signatures with it.
//
// The `attested` parameter passed to the delegate is cryptographically verified
// by Freenet - it contains the ContractInstanceId of the webapp that sent the
// message. This cannot be spoofed.
//
// Keys are stored under: "signing_key:{origin_base58}:{room_key_base58}"
//
// This means:
// - When River (contract A) stores a signing key, it's stored under A's origin
// - When River requests a signature, it looks up using A's origin -> found
// - If malicious contract B requests a signature, it looks up using B's origin -> not found
//
// The private key never leaves the delegate. The UI only receives:
// - Public keys (via GetPublicKey)
// - Signatures (via Sign* operations)
//
// ============================================================================

/// Create a secret key for storing a signing key for a room.
/// Format: "signing_key:{origin_base58}:{room_key_base58}"
fn create_signing_key_secret_key(origin: &Origin, room_key: &RoomKey) -> Vec<u8> {
    let origin_b58 = bs58::encode(&origin.0).into_string();
    let room_key_b58 = bs58::encode(room_key).into_string();
    format!("signing_key:{origin_b58}:{room_key_b58}").into_bytes()
}

/// Handle a store signing key request
fn handle_store_signing_key(
    ctx: &mut DelegateCtx,
    origin: &Origin,
    room_key: RoomKey,
    signing_key_bytes: [u8; 32],
    app: ContractInstanceId,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    let secret_key = create_signing_key_secret_key(origin, &room_key);

    // Store the signing key directly via host function
    // Note: In WASM, set_secret returns true on success. In non-WASM tests, it always returns false.
    #[cfg(target_family = "wasm")]
    if !ctx.set_secret(&secret_key, &signing_key_bytes) {
        return Err(DelegateError::Other(
            "Failed to store signing key via host function".into(),
        ));
    }
    #[cfg(not(target_family = "wasm"))]
    let _ = ctx.set_secret(&secret_key, &signing_key_bytes);

    logging::info("Stored signing key for room");

    // Create response for the client
    let response = ChatDelegateResponseMsg::StoreSigningKeyResponse {
        room_key,
        result: Ok(()),
    };

    Ok(vec![create_app_response(&response, app)?])
}

/// Handle a get public key request
fn handle_get_public_key(
    ctx: &mut DelegateCtx,
    origin: &Origin,
    room_key: RoomKey,
    app: ContractInstanceId,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    let secret_key = create_signing_key_secret_key(origin, &room_key);

    // Get the signing key directly via host function
    let public_key = ctx.get_secret(&secret_key).and_then(|sk_bytes| {
        if sk_bytes.len() == 32 {
            let sk_array: [u8; 32] = sk_bytes.try_into().ok()?;
            let signing_key = SigningKey::from_bytes(&sk_array);
            Some(signing_key.verifying_key().to_bytes())
        } else {
            None
        }
    });

    logging::info(&format!(
        "Retrieved public key for room, key present: {}",
        public_key.is_some()
    ));

    // Create response for the client
    let response = ChatDelegateResponseMsg::GetPublicKeyResponse {
        room_key,
        public_key,
    };

    Ok(vec![create_app_response(&response, app)?])
}

/// Handle a sign request (for any signable type)
fn handle_sign_request(
    ctx: &mut DelegateCtx,
    origin: &Origin,
    room_key: RoomKey,
    request_id: RequestId,
    data_to_sign: Vec<u8>,
    app: ContractInstanceId,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    let secret_key = create_signing_key_secret_key(origin, &room_key);

    // Get the signing key and sign directly
    let signature: Result<Vec<u8>, String> = match ctx.get_secret(&secret_key) {
        Some(sk_bytes) => {
            if sk_bytes.len() == 32 {
                // Safe: we just checked length is 32
                let sk_array: [u8; 32] = sk_bytes.try_into().expect("length verified");
                let signing_key = SigningKey::from_bytes(&sk_array);
                let sig = signing_key.sign(&data_to_sign);
                Ok(sig.to_bytes().to_vec())
            } else {
                Err(format!(
                    "Invalid signing key length: expected 32, got {}",
                    sk_bytes.len()
                ))
            }
        }
        None => Err("Signing key not found for this room".to_string()),
    };

    logging::info(&format!(
        "Sign request for room, signature created: {}",
        signature.is_ok()
    ));

    // Create response for the client
    let response = ChatDelegateResponseMsg::SignResponse {
        room_key,
        request_id,
        signature,
    };

    Ok(vec![create_app_response(&response, app)?])
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Get the key index from secrets, or return empty if not found
fn get_key_index(ctx: &mut DelegateCtx, index_key: &[u8]) -> KeyIndex {
    ctx.get_secret(index_key)
        .and_then(|data| ciborium::from_reader::<KeyIndex, _>(data.as_slice()).ok())
        .unwrap_or_default()
}

/// Set the key index in secrets
fn set_key_index(
    ctx: &mut DelegateCtx,
    index_key: &[u8],
    key_index: &KeyIndex,
) -> Result<(), DelegateError> {
    let mut index_bytes = Vec::new();
    ciborium::ser::into_writer(key_index, &mut index_bytes)
        .map_err(|e| DelegateError::Deser(format!("Failed to serialize key index: {e}")))?;

    // Note: In WASM, set_secret returns true on success. In non-WASM tests, it always returns false.
    #[cfg(target_family = "wasm")]
    if !ctx.set_secret(index_key, &index_bytes) {
        return Err(DelegateError::Other(
            "Failed to store key index via host function".into(),
        ));
    }
    #[cfg(not(target_family = "wasm"))]
    let _ = ctx.set_secret(index_key, &index_bytes);

    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use freenet_stdlib::prelude::DelegateCtx;

    /// Helper function to create empty parameters for testing
    fn create_test_parameters() -> Parameters<'static> {
        Parameters::from(vec![])
    }

    /// Helper function to create an application message
    fn create_app_message(
        request: ChatDelegateRequestMsg,
        app_id: ContractInstanceId,
    ) -> ApplicationMessage {
        let mut payload = Vec::new();
        ciborium::ser::into_writer(&request, &mut payload)
            .map_err(|e| panic!("Failed to serialize request: {e}"))
            .unwrap();
        ApplicationMessage::new(app_id, payload)
    }

    /// Helper function to extract response from outbound messages
    fn extract_response(messages: Vec<OutboundDelegateMsg>) -> Option<ChatDelegateResponseMsg> {
        for msg in messages {
            if let OutboundDelegateMsg::ApplicationMessage(app_msg) = msg {
                return ciborium::from_reader(app_msg.payload.as_slice())
                    .map_err(|e| panic!("Failed to deserialize response: {e}"))
                    .ok();
            }
        }
        None
    }

    // Test origin bytes - using a fixed ContractInstanceId for testing
    fn get_test_origin_bytes() -> &'static [u8] {
        static ORIGIN: [u8; 32] = [42u8; 32];
        &ORIGIN
    }

    #[test]
    fn test_store_request() {
        let key = b"test_key".to_vec();
        let value = b"test_value".to_vec();

        let request = ChatDelegateRequestMsg::StoreRequest {
            key: river_core::chat_delegate::ChatDelegateKey(key.clone()),
            value: value.clone(),
        };
        let dummy_app_id = ContractInstanceId::new([1u8; 32]);
        let app_msg = create_app_message(request, dummy_app_id);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = crate::ChatDelegate::process(
            &mut DelegateCtx::default(),
            create_test_parameters(),
            Some(get_test_origin_bytes()),
            inbound_msg,
        )
        .unwrap();

        // Should have 1 message: app response (secrets are stored via host function)
        assert_eq!(result.len(), 1);

        // Check app response
        let response = extract_response(result).unwrap();
        match response {
            ChatDelegateResponseMsg::StoreResponse {
                key: resp_key,
                result,
                value_size,
            } => {
                assert_eq!(
                    resp_key,
                    river_core::chat_delegate::ChatDelegateKey(key.clone())
                );
                assert!(result.is_ok());
                assert_eq!(value_size, value.len());
            }
            _ => panic!("Expected StoreResponse, got {:?}", response),
        }
    }

    #[test]
    fn test_get_request() {
        let key = b"test_key".to_vec();

        let request = ChatDelegateRequestMsg::GetRequest {
            key: river_core::chat_delegate::ChatDelegateKey(key.clone()),
        };
        let dummy_app_id = ContractInstanceId::new([2u8; 32]);
        let app_msg = create_app_message(request, dummy_app_id);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = crate::ChatDelegate::process(
            &mut DelegateCtx::default(),
            create_test_parameters(),
            Some(get_test_origin_bytes()),
            inbound_msg,
        )
        .unwrap();

        // Should have 1 message: app response with the value (or None if not found)
        assert_eq!(result.len(), 1);

        // Check it's a GetResponse
        let response = extract_response(result).unwrap();
        match response {
            ChatDelegateResponseMsg::GetResponse {
                key: resp_key,
                value: _,
            } => {
                assert_eq!(
                    resp_key,
                    river_core::chat_delegate::ChatDelegateKey(key.clone())
                );
                // Value will be None in test since we didn't store it first
            }
            _ => panic!("Expected GetResponse, got {:?}", response),
        }
    }

    #[test]
    fn test_delete_request() {
        let key = b"test_key".to_vec();

        let request = ChatDelegateRequestMsg::DeleteRequest {
            key: river_core::chat_delegate::ChatDelegateKey(key.clone()),
        };
        let dummy_app_id = ContractInstanceId::new([3u8; 32]);
        let app_msg = create_app_message(request, dummy_app_id);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = crate::ChatDelegate::process(
            &mut DelegateCtx::default(),
            create_test_parameters(),
            Some(get_test_origin_bytes()),
            inbound_msg,
        )
        .unwrap();

        // Should have 1 message: app response
        assert_eq!(result.len(), 1);

        // Check response
        let response = extract_response(result).unwrap();
        match response {
            ChatDelegateResponseMsg::DeleteResponse {
                key: resp_key,
                result,
            } => {
                assert_eq!(
                    resp_key,
                    river_core::chat_delegate::ChatDelegateKey(key.clone())
                );
                assert!(result.is_ok());
            }
            _ => panic!("Expected DeleteResponse, got {:?}", response),
        }
    }

    #[test]
    fn test_list_request() {
        let request = ChatDelegateRequestMsg::ListRequest;
        let dummy_app_id = ContractInstanceId::new([4u8; 32]);
        let app_msg = create_app_message(request, dummy_app_id);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = crate::ChatDelegate::process(
            &mut DelegateCtx::default(),
            create_test_parameters(),
            Some(get_test_origin_bytes()),
            inbound_msg,
        )
        .unwrap();

        // Should have 1 message: app response with list
        assert_eq!(result.len(), 1);

        // Check response
        let response = extract_response(result).unwrap();
        match response {
            ChatDelegateResponseMsg::ListResponse { keys } => {
                // Empty list since we haven't stored anything
                assert!(keys.is_empty());
            }
            _ => panic!("Expected ListResponse, got {:?}", response),
        }
    }

    #[test]
    fn test_error_on_processed_message() {
        let request = ChatDelegateRequestMsg::ListRequest;
        let mut payload = Vec::new();
        ciborium::ser::into_writer(&request, &mut payload).unwrap();

        let dummy_app_id = ContractInstanceId::new([5u8; 32]);
        let app_msg = ApplicationMessage::new(dummy_app_id, payload).processed(true);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = crate::ChatDelegate::process(
            &mut DelegateCtx::default(),
            create_test_parameters(),
            Some(get_test_origin_bytes()),
            inbound_msg,
        );

        assert!(result.is_err());
        if let Err(DelegateError::Other(msg)) = result {
            assert!(msg.contains("already processed"));
        } else {
            panic!("Expected DelegateError::Other, got {:?}", result);
        }
    }

    #[test]
    fn test_error_on_missing_attested() {
        let request = ChatDelegateRequestMsg::ListRequest;
        let dummy_app_id = ContractInstanceId::new([6u8; 32]);
        let app_msg = create_app_message(request, dummy_app_id);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        // Pass None for attested
        let result = crate::ChatDelegate::process(
            &mut DelegateCtx::default(),
            create_test_parameters(),
            None,
            inbound_msg,
        );
        assert!(result.is_err());

        if let Err(DelegateError::Other(msg)) = result {
            assert!(msg.contains("missing attested origin"));
        } else {
            panic!("Expected DelegateError::Other, got {:?}", result);
        }
    }

    #[test]
    fn test_store_signing_key() {
        let room_key: river_core::chat_delegate::RoomKey = [7u8; 32];
        let signing_key_bytes: [u8; 32] = [8u8; 32];

        let request = ChatDelegateRequestMsg::StoreSigningKey {
            room_key,
            signing_key_bytes,
        };
        let dummy_app_id = ContractInstanceId::new([7u8; 32]);
        let app_msg = create_app_message(request, dummy_app_id);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = crate::ChatDelegate::process(
            &mut DelegateCtx::default(),
            create_test_parameters(),
            Some(get_test_origin_bytes()),
            inbound_msg,
        )
        .unwrap();

        // Should have 1 message: app response
        assert_eq!(result.len(), 1);

        // Check response
        let response = extract_response(result).unwrap();
        match response {
            ChatDelegateResponseMsg::StoreSigningKeyResponse {
                room_key: resp_room_key,
                result,
            } => {
                assert_eq!(resp_room_key, room_key);
                assert!(result.is_ok());
            }
            _ => panic!("Expected StoreSigningKeyResponse, got {:?}", response),
        }
    }

    #[test]
    fn test_get_public_key_not_found() {
        let room_key: river_core::chat_delegate::RoomKey = [9u8; 32];

        let request = ChatDelegateRequestMsg::GetPublicKey { room_key };
        let dummy_app_id = ContractInstanceId::new([8u8; 32]);
        let app_msg = create_app_message(request, dummy_app_id);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = crate::ChatDelegate::process(
            &mut DelegateCtx::default(),
            create_test_parameters(),
            Some(get_test_origin_bytes()),
            inbound_msg,
        )
        .unwrap();

        // Should have 1 message: app response
        assert_eq!(result.len(), 1);

        // Check response - public key should be None since no key is stored
        let response = extract_response(result).unwrap();
        match response {
            ChatDelegateResponseMsg::GetPublicKeyResponse {
                room_key: resp_room_key,
                public_key,
            } => {
                assert_eq!(resp_room_key, room_key);
                // In non-WASM test environment, get_secret returns None
                assert!(public_key.is_none());
            }
            _ => panic!("Expected GetPublicKeyResponse, got {:?}", response),
        }
    }

    #[test]
    fn test_sign_message_without_key_returns_error() {
        let room_key: river_core::chat_delegate::RoomKey = [10u8; 32];
        let request_id: river_core::chat_delegate::RequestId = 12345;
        let message_bytes = b"test message to sign".to_vec();

        let request = ChatDelegateRequestMsg::SignMessage {
            room_key,
            request_id,
            message_bytes,
        };
        let dummy_app_id = ContractInstanceId::new([9u8; 32]);
        let app_msg = create_app_message(request, dummy_app_id);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = crate::ChatDelegate::process(
            &mut DelegateCtx::default(),
            create_test_parameters(),
            Some(get_test_origin_bytes()),
            inbound_msg,
        )
        .unwrap();

        // Should have 1 message: app response
        assert_eq!(result.len(), 1);

        // Check response - signature should be an error since no key is stored
        let response = extract_response(result).unwrap();
        match response {
            ChatDelegateResponseMsg::SignResponse {
                room_key: resp_room_key,
                request_id: resp_request_id,
                signature,
            } => {
                assert_eq!(resp_room_key, room_key);
                assert_eq!(resp_request_id, request_id);
                // Should be an error because no signing key is stored
                assert!(signature.is_err());
                let err_msg = signature.unwrap_err();
                assert!(
                    err_msg.contains("not found"),
                    "Expected 'not found' error, got: {}",
                    err_msg
                );
            }
            _ => panic!("Expected SignResponse, got {:?}", response),
        }
    }
}
