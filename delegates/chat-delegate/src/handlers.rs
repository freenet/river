use super::*;
use ed25519_dalek::{Signer, SigningKey};
use freenet_stdlib::prelude::ContractInstanceId;
use river_core::chat_delegate::RoomKey;

/// Handle an application message
pub(crate) fn handle_application_message(
    app_msg: ApplicationMessage,
    origin: &Origin,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    let mut context = ChatDelegateContext::try_from(app_msg.context)?;

    // Deserialize the request message
    let request: ChatDelegateRequestMsg = ciborium::from_reader(app_msg.payload.as_slice())
        .map_err(|e| DelegateError::Deser(format!("Failed to deserialize request: {e}")))?;

    match request {
        // Key-value storage operations
        ChatDelegateRequestMsg::StoreRequest { key, value } => {
            logging::info(
                format!("Delegate received StoreRequest key: {key:?}, value: {value:?}").as_str(),
            );
            handle_store_request(&mut context, origin, key, value, app_msg.app)
        }
        ChatDelegateRequestMsg::GetRequest { key } => {
            logging::info(format!("Delegate received GetRequest key: {key:?}").as_str());
            handle_get_request(&mut context, origin, key, app_msg.app)
        }
        ChatDelegateRequestMsg::DeleteRequest { key } => {
            logging::info(format!("Delegate received DeleteRequest key: {key:?}").as_str());
            handle_delete_request(&mut context, origin, key, app_msg.app)
        }
        ChatDelegateRequestMsg::ListRequest => {
            logging::info("Delegate received ListRequest");
            handle_list_request(&mut context, origin, app_msg.app)
        }

        // Signing key management
        ChatDelegateRequestMsg::StoreSigningKey {
            room_key,
            signing_key_bytes,
        } => {
            logging::info(format!("Delegate received StoreSigningKey for room: {room_key:?}").as_str());
            handle_store_signing_key(&mut context, origin, room_key, signing_key_bytes, app_msg.app)
        }
        ChatDelegateRequestMsg::GetPublicKey { room_key } => {
            logging::info(format!("Delegate received GetPublicKey for room: {room_key:?}").as_str());
            handle_get_public_key(&mut context, origin, room_key, app_msg.app)
        }

        // Signing operations
        ChatDelegateRequestMsg::SignMessage {
            room_key,
            message_bytes,
        } => {
            logging::info(format!("Delegate received SignMessage for room: {room_key:?}").as_str());
            handle_sign_request(&mut context, origin, room_key, message_bytes, app_msg.app)
        }
        ChatDelegateRequestMsg::SignMember {
            room_key,
            member_bytes,
        } => {
            logging::info(format!("Delegate received SignMember for room: {room_key:?}").as_str());
            handle_sign_request(&mut context, origin, room_key, member_bytes, app_msg.app)
        }
        ChatDelegateRequestMsg::SignBan {
            room_key,
            ban_bytes,
        } => {
            logging::info(format!("Delegate received SignBan for room: {room_key:?}").as_str());
            handle_sign_request(&mut context, origin, room_key, ban_bytes, app_msg.app)
        }
        ChatDelegateRequestMsg::SignConfig {
            room_key,
            config_bytes,
        } => {
            logging::info(format!("Delegate received SignConfig for room: {room_key:?}").as_str());
            handle_sign_request(&mut context, origin, room_key, config_bytes, app_msg.app)
        }
        ChatDelegateRequestMsg::SignMemberInfo {
            room_key,
            member_info_bytes,
        } => {
            logging::info(format!("Delegate received SignMemberInfo for room: {room_key:?}").as_str());
            handle_sign_request(&mut context, origin, room_key, member_info_bytes, app_msg.app)
        }
        ChatDelegateRequestMsg::SignSecretVersion {
            room_key,
            record_bytes,
        } => {
            logging::info(format!("Delegate received SignSecretVersion for room: {room_key:?}").as_str());
            handle_sign_request(&mut context, origin, room_key, record_bytes, app_msg.app)
        }
        ChatDelegateRequestMsg::SignEncryptedSecret {
            room_key,
            secret_bytes,
        } => {
            logging::info(format!("Delegate received SignEncryptedSecret for room: {room_key:?}").as_str());
            handle_sign_request(&mut context, origin, room_key, secret_bytes, app_msg.app)
        }
        ChatDelegateRequestMsg::SignUpgrade {
            room_key,
            upgrade_bytes,
        } => {
            logging::info(format!("Delegate received SignUpgrade for room: {room_key:?}").as_str());
            handle_sign_request(&mut context, origin, room_key, upgrade_bytes, app_msg.app)
        }
    }
}

/// Handle a store request
pub(crate) fn handle_store_request(
    context: &mut ChatDelegateContext,
    origin: &Origin,
    key: ChatDelegateKey,
    value: Vec<u8>,
    app: ContractInstanceId,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    // Create a unique key for this app's data
    let secret_id = create_origin_key(origin, &key);

    // Create the index key
    let index_key = create_index_key(origin);

    // Store the original request in context for later processing after we get the index
    context.pending_ops.insert(
        SecretIdKey::from(&index_key),
        PendingOperation::Store {
            origin: origin.clone(),
            client_key: key.clone(),
        },
    );

    // Create response for the client
    let response = ChatDelegateResponseMsg::StoreResponse {
        key: key.clone(),
        result: Ok(()),
        value_size: value.len(),
    };

    // Serialize context
    let context_bytes = DelegateContext::try_from(&*context)?;

    // Create the three messages we need to send:
    // 1. Response to the client
    let app_response = create_app_response(&response, &context_bytes, app)?;

    // 2. Store the actual value
    let set_secret = OutboundDelegateMsg::SetSecretRequest(SetSecretRequest {
        key: secret_id,
        value: Some(value),
    });

    // 3. Request the current index to update it
    let get_index = create_get_index_request(index_key, &context_bytes)?;

    // Return all messages
    Ok(vec![app_response, set_secret, get_index])
}

/// Handle a get request
pub(crate) fn handle_get_request(
    context: &mut ChatDelegateContext,
    origin: &Origin,
    key: ChatDelegateKey,
    app: ContractInstanceId, // Add app parameter
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    // Create a unique key for this origin contract's data
    let secret_id = create_origin_key(origin, &key);

    // Store the original request in context for later processing
    context.pending_ops.insert(
        SecretIdKey::from(&secret_id),
        PendingOperation::Get {
            origin: origin.clone(),
            client_key: key.clone(),
            app, // Store the passed app identifier
        },
    );

    // Serialize context
    let context_bytes = DelegateContext::try_from(&*context)?;

    // Create and return the get request
    let get_secret = create_get_request(secret_id, &context_bytes)?;

    Ok(vec![get_secret])
}

/// Handle a delete request
pub(crate) fn handle_delete_request(
    context: &mut ChatDelegateContext,
    origin: &Origin,
    key: ChatDelegateKey,
    app: ContractInstanceId,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    // Create a unique key for this app's data
    let secret_id = create_origin_key(origin, &key);

    // Create the index key
    let index_key = create_index_key(origin);

    // Store the original request in context for later processing after we get the index
    context.pending_ops.insert(
        SecretIdKey::from(&index_key),
        PendingOperation::Delete {
            origin: origin.clone(),
            client_key: key.clone(),
        },
    );

    // Create response for the client
    let response = ChatDelegateResponseMsg::DeleteResponse {
        key: key.clone(),
        result: Ok(()),
    };

    // Serialize context
    let context_bytes = DelegateContext::try_from(&*context)?;

    // Create the three messages we need to send:
    // 1. Response to the client
    let app_response = create_app_response(&response, &context_bytes, app)?;

    // 2. Delete the actual value
    let set_secret = OutboundDelegateMsg::SetSecretRequest(SetSecretRequest {
        key: secret_id,
        value: None, // Setting to None deletes the secret
    });

    // 3. Request the current index to update it
    let get_index = create_get_index_request(index_key, &context_bytes)?;

    // Return all messages
    Ok(vec![app_response, set_secret, get_index])
}

/// Handle a list request
pub(crate) fn handle_list_request(
    context: &mut ChatDelegateContext,
    origin: &Origin,
    id: ContractInstanceId,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    // Create the index key
    let index_key = create_index_key(origin);

    // Store a special marker in the context to indicate this is a list request
    context.pending_ops.insert(
        SecretIdKey::from(&index_key),
        PendingOperation::List {
            origin: origin.clone(),
            app: id, // Store the app identifier (parameter name is 'id' here)
        },
    );

    // Serialize context
    let context_bytes = DelegateContext::try_from(&*context)?;

    // Create and return the get index request
    let get_index = create_get_index_request(index_key, &context_bytes)?;

    Ok(vec![get_index])
}

/// Handle a get secret response
pub(crate) fn handle_get_secret_response(
    get_secret_response: freenet_stdlib::prelude::GetSecretResponse,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    logging::info("Received GetSecretResponse");

    // Deserialize context
    let mut context = match ChatDelegateContext::try_from(get_secret_response.context.clone()) {
        Ok(ctx) => ctx,
        Err(e) => {
            logging::info(&format!("Failed to deserialize context: {e}"));
            return Err(e);
        }
    };

    // Get the key as a string to check its type
    let key_str = String::from_utf8_lossy(get_secret_response.key.key()).to_string();
    let key_clone = get_secret_response.key.clone();

    logging::info(&format!("Processing response for key: {key_str}"));

    // Route based on key type
    let result = if key_str.ends_with(KEY_INDEX_SUFFIX) {
        logging::info("This is a key index response");
        handle_key_index_response(&key_clone, &mut context, get_secret_response)
    } else if key_str.starts_with("signing_key:") {
        logging::info("This is a signing key response");
        handle_signing_get_response(&key_clone, &mut context, get_secret_response)
    } else {
        logging::info("This is a regular get response");
        handle_regular_get_response(&key_clone, &mut context, get_secret_response)
    };

    match &result {
        Ok(msgs) => logging::info(&format!("Returning {} messages", msgs.len())),
        Err(e) => logging::info(&format!("Error handling response: {e}")),
    }

    result
}

/// Handle a key index response
pub(crate) fn handle_key_index_response(
    secret_id: &SecretsId,
    context: &mut ChatDelegateContext,
    get_secret_response: freenet_stdlib::prelude::GetSecretResponse,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    logging::info("Handling key index response");

    // This is a response to a key index request
    let secret_id_key = SecretIdKey::from(secret_id);
    if let Some(pending_op) = context.pending_ops.get(&secret_id_key).cloned() {
        let mut outbound_msgs = Vec::new();

        // Parse the key index or create a new one if it doesn't exist
        let mut key_index = if let Some(index_data) = &get_secret_response.value {
            ciborium::from_reader::<KeyIndex, _>(index_data.as_slice()).unwrap_or_else(|e| {
                logging::info(&format!(
                    "Failed to deserialize key index, creating new one: {e}"
                ));
                KeyIndex::default()
            })
        } else {
            logging::info("No index data found, creating new index");
            KeyIndex::default()
        };

        match &pending_op {
            PendingOperation::List { app, .. } => {
                // Extract app here
                // Create list response
                let response = ChatDelegateResponseMsg::ListResponse {
                    keys: key_index.keys.clone(),
                };

                // Remove the pending operation *before* creating the response
                context.pending_ops.remove(&secret_id_key);

                // Create response message using the *updated* context
                let context_bytes = DelegateContext::try_from(&*context)?;
                // Pass the retrieved app identifier
                let app_response = create_app_response(&response, &context_bytes, *app)?;
                outbound_msgs.push(app_response);
                logging::info(&format!(
                    "Created list response with {} keys",
                    key_index.keys.len()
                ));
            }
            PendingOperation::Store { client_key, .. }
            | PendingOperation::Delete { client_key, .. } => {
                // This is a store or delete operation that needs to update the index
                let is_delete = pending_op.is_delete_operation();

                if is_delete {
                    // For delete operations, remove the key
                    key_index.keys.retain(|k| k != client_key);
                    logging::info(&format!(
                        "Removed key from index, now has {} keys",
                        key_index.keys.len()
                    ));
                } else {
                    // For store operations, add the key if it doesn't exist
                    if !key_index.keys.contains(client_key) {
                        key_index.keys.push(client_key.clone());
                        logging::info(&format!(
                            "Added key to index, now has {} keys",
                            key_index.keys.len()
                        ));
                    } else {
                        logging::info("Key already exists in index, not adding");
                    }
                }

                // Serialize the updated index
                let mut index_bytes = Vec::new();
                ciborium::ser::into_writer(&key_index, &mut index_bytes).map_err(|e| {
                    DelegateError::Deser(format!("Failed to serialize key index: {e}"))
                })?;

                // Create set secret request to update the index
                let set_index = OutboundDelegateMsg::SetSecretRequest(SetSecretRequest {
                    key: secret_id.clone(),
                    value: Some(index_bytes),
                });

                outbound_msgs.push(set_index);
            }
            PendingOperation::Get { .. } => {
                return Err(DelegateError::Other(
                    "Unexpected Get operation for key index response".to_string(),
                ));
            }
            PendingOperation::GetPublicKey { .. } | PendingOperation::Sign { .. } => {
                return Err(DelegateError::Other(
                    "Unexpected signing operation for key index response".to_string(),
                ));
            }
        }

        // Remove the pending operation (moved inside List case, Store/Delete need further refactoring for Bug #1)
        context.pending_ops.remove(&secret_id_key);

        logging::info(&format!(
            "Returning {} outbound messages",
            outbound_msgs.len()
        ));
        Ok(outbound_msgs)
    } else {
        // No pending operation for this key index
        logging::info(&format!("No pending key index request for: {secret_id:?}"));
        Err(DelegateError::Other(format!(
            "No pending key index request for: {secret_id:?}"
        )))
    }
}

/// Handle a regular get response
pub(crate) fn handle_regular_get_response(
    secret_id: &SecretsId,
    context: &mut ChatDelegateContext,
    get_secret_response: freenet_stdlib::prelude::GetSecretResponse,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    logging::info("Handling regular get response");

    let secret_id_key = SecretIdKey::from(secret_id);
    // Extract app along with client_key
    if let Some(PendingOperation::Get {
        client_key, app, ..
    }) = context.pending_ops.get(&secret_id_key).cloned()
    {
        // Create response
        let response = ChatDelegateResponseMsg::GetResponse {
            key: client_key.clone(),
            value: get_secret_response.value.clone(),
        };

        // Remove the pending get request *before* creating the response
        context.pending_ops.remove(&secret_id_key);

        // Create response message using the *updated* context
        let context_bytes = DelegateContext::try_from(&*context)?;
        // Pass the retrieved app identifier
        let app_response = create_app_response(&response, &context_bytes, app)?;

        logging::info(&format!(
            "Returning get response for key: {:?}, value present: {}, to app: {:?}",
            client_key,
            get_secret_response.value.is_some(),
            app // Log the target app
        ));
        Ok(vec![app_response])
    } else {
        let key_str = String::from_utf8_lossy(secret_id.key()).to_string();
        logging::info(&format!("No pending get request for key: {key_str}"));
        Err(DelegateError::Other(format!(
            "No pending get request for key: {key_str}"
        )))
    }
}

// ============================================================================
// Signing Operations Handlers
// ============================================================================

/// Create a secret ID for storing a signing key for a room.
/// Format: "signing_key:{origin_base58}:{room_key_base58}"
fn create_signing_key_secret_id(origin: &Origin, room_key: &RoomKey) -> SecretsId {
    let origin_b58 = bs58::encode(&origin.0).into_string();
    let room_key_b58 = bs58::encode(room_key).into_string();
    let key = format!("signing_key:{origin_b58}:{room_key_b58}");
    SecretsId::new(key.into_bytes())
}

/// Handle a store signing key request
pub(crate) fn handle_store_signing_key(
    context: &mut ChatDelegateContext,
    origin: &Origin,
    room_key: RoomKey,
    signing_key_bytes: [u8; 32],
    app: ContractInstanceId,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    // Create the secret ID for storing the signing key
    let secret_id = create_signing_key_secret_id(origin, &room_key);

    // Create response for the client
    let response = ChatDelegateResponseMsg::StoreSigningKeyResponse {
        room_key,
        result: Ok(()),
    };

    // Serialize context
    let context_bytes = DelegateContext::try_from(&*context)?;

    // Create the response message
    let app_response = create_app_response(&response, &context_bytes, app)?;

    // Store the signing key in the secret storage
    let set_secret = OutboundDelegateMsg::SetSecretRequest(SetSecretRequest {
        key: secret_id,
        value: Some(signing_key_bytes.to_vec()),
    });

    logging::info(&format!(
        "Storing signing key for room, secret storage requested"
    ));

    Ok(vec![app_response, set_secret])
}

/// Handle a get public key request
pub(crate) fn handle_get_public_key(
    context: &mut ChatDelegateContext,
    origin: &Origin,
    room_key: RoomKey,
    app: ContractInstanceId,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    // Create the secret ID for the signing key
    let secret_id = create_signing_key_secret_id(origin, &room_key);

    // Store the pending operation in context
    context.pending_ops.insert(
        SecretIdKey::from(&secret_id),
        PendingOperation::GetPublicKey {
            origin: origin.clone(),
            room_key,
            app,
        },
    );

    // Serialize context
    let context_bytes = DelegateContext::try_from(&*context)?;

    // Create request to get the signing key
    let get_secret = create_get_request(secret_id, &context_bytes)?;

    logging::info("Requesting signing key from secret storage");

    Ok(vec![get_secret])
}

/// Handle a sign request (for any signable type)
pub(crate) fn handle_sign_request(
    context: &mut ChatDelegateContext,
    origin: &Origin,
    room_key: RoomKey,
    data_to_sign: Vec<u8>,
    app: ContractInstanceId,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    // Create the secret ID for the signing key
    let secret_id = create_signing_key_secret_id(origin, &room_key);

    // Store the pending operation in context with the data to sign
    context.pending_ops.insert(
        SecretIdKey::from(&secret_id),
        PendingOperation::Sign {
            origin: origin.clone(),
            room_key,
            data_to_sign,
            app,
        },
    );

    // Serialize context
    let context_bytes = DelegateContext::try_from(&*context)?;

    // Create request to get the signing key
    let get_secret = create_get_request(secret_id, &context_bytes)?;

    logging::info("Requesting signing key from secret storage for signing");

    Ok(vec![get_secret])
}

/// Handle a get secret response for signing-related operations
pub(crate) fn handle_signing_get_response(
    secret_id: &SecretsId,
    context: &mut ChatDelegateContext,
    get_secret_response: freenet_stdlib::prelude::GetSecretResponse,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    let secret_id_key = SecretIdKey::from(secret_id);

    // Check if this is a GetPublicKey or Sign operation
    if let Some(pending_op) = context.pending_ops.get(&secret_id_key).cloned() {
        match pending_op {
            PendingOperation::GetPublicKey { room_key, app, .. } => {
                // Get the public key from the stored signing key
                let public_key = if let Some(sk_bytes) = get_secret_response.value {
                    if sk_bytes.len() == 32 {
                        let sk_array: [u8; 32] = sk_bytes.try_into().map_err(|_| {
                            DelegateError::Other("Invalid signing key length".to_string())
                        })?;
                        let signing_key = SigningKey::from_bytes(&sk_array);
                        Some(signing_key.verifying_key().to_bytes())
                    } else {
                        None
                    }
                } else {
                    None
                };

                // Create response
                let response = ChatDelegateResponseMsg::GetPublicKeyResponse { room_key, public_key };

                // Remove the pending operation
                context.pending_ops.remove(&secret_id_key);

                // Serialize context
                let context_bytes = DelegateContext::try_from(&*context)?;
                let app_response = create_app_response(&response, &context_bytes, app)?;

                logging::info(&format!(
                    "Returning public key for room: {:?}, key present: {}",
                    room_key,
                    public_key.is_some()
                ));

                Ok(vec![app_response])
            }
            PendingOperation::Sign {
                room_key,
                data_to_sign,
                app,
                ..
            } => {
                // Sign the data using the stored signing key
                let signature = if let Some(sk_bytes) = get_secret_response.value {
                    if sk_bytes.len() == 32 {
                        let sk_array: [u8; 32] = sk_bytes.try_into().map_err(|_| {
                            DelegateError::Other("Invalid signing key length".to_string())
                        })?;
                        let signing_key = SigningKey::from_bytes(&sk_array);
                        let sig = signing_key.sign(&data_to_sign);
                        Ok(sig.to_bytes().to_vec())
                    } else {
                        Err(format!(
                            "Invalid signing key length: {} bytes (expected 32)",
                            sk_bytes.len()
                        ))
                    }
                } else {
                    Err("Signing key not found for room".to_string())
                };

                // Create response
                let response = ChatDelegateResponseMsg::SignResponse { room_key, signature };

                // Remove the pending operation
                context.pending_ops.remove(&secret_id_key);

                // Serialize context
                let context_bytes = DelegateContext::try_from(&*context)?;
                let app_response = create_app_response(&response, &context_bytes, app)?;

                logging::info(&format!(
                    "Returning signature for room: {:?}",
                    room_key
                ));

                Ok(vec![app_response])
            }
            _ => {
                // Not a signing operation, return error
                logging::info(&format!(
                    "Unexpected pending operation type for signing response: {:?}",
                    secret_id
                ));
                Err(DelegateError::Other(format!(
                    "Unexpected pending operation type for: {:?}",
                    secret_id
                )))
            }
        }
    } else {
        let key_str = String::from_utf8_lossy(secret_id.key()).to_string();
        logging::info(&format!("No pending signing operation for key: {key_str}"));
        Err(DelegateError::Other(format!(
            "No pending signing operation for key: {key_str}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::*;
    use crate::utils::*;
    use freenet_stdlib::prelude::DelegateContext;

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
        ApplicationMessage::new(app_id, payload) // Pass app_id here
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

    #[test]
    fn test_store_request() {
        let key = b"test_key".to_vec();
        let value = b"test_value".to_vec();

        let request = ChatDelegateRequestMsg::StoreRequest {
            key: river_core::chat_delegate::ChatDelegateKey(key.clone()),
            value: value.clone(),
        };
        let dummy_app_id = ContractInstanceId::new([1u8; 32]); // Dummy ID for test
        let app_msg = create_app_message(request, dummy_app_id);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = crate::ChatDelegate::process(
            create_test_parameters(),
            Some(get_test_origin_bytes()),
            inbound_msg,
        )
        .unwrap();

        // Should have 3 messages: app response, set secret, get index
        assert_eq!(result.len(), 3);

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
        let dummy_app_id = ContractInstanceId::new([2u8; 32]); // Dummy ID for test
        let app_msg = create_app_message(request, dummy_app_id);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = crate::ChatDelegate::process(
            create_test_parameters(),
            Some(get_test_origin_bytes()),
            inbound_msg,
        )
        .unwrap();

        // Should have 1 message: get secret request
        assert_eq!(result.len(), 1);

        // Check it's a get secret request
        match &result[0] {
            OutboundDelegateMsg::GetSecretRequest(req) => {
                // Verify the key contains our test origin and key
                let key_str = String::from_utf8(req.key.key().to_vec())
                    .map_err(|e| panic!("Invalid UTF-8 in key: {e}"))
                    .unwrap();

                // The key format is "origin:key" where origin is base58 encoded
                // Just check that it contains some part of our test key
                assert!(key_str.contains("test_key"));
            }
            _ => panic!("Expected GetSecretRequest, got {:?}", result[0]),
        }
    }

    #[test]
    fn test_delete_request() {
        let key = b"test_key".to_vec();

        let request = ChatDelegateRequestMsg::DeleteRequest {
            key: river_core::chat_delegate::ChatDelegateKey(key.clone()),
        };
        let dummy_app_id = ContractInstanceId::new([3u8; 32]); // Dummy ID for test
        let app_msg = create_app_message(request, dummy_app_id);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = crate::ChatDelegate::process(
            create_test_parameters(),
            Some(get_test_origin_bytes()),
            inbound_msg,
        )
        .unwrap();

        // Should have 3 messages: app response, set secret (with None value), get index
        assert_eq!(result.len(), 3);

        // Check app response
        let response = extract_response(result.clone()).unwrap();
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

        // Check set secret request has None value (deletion)
        let mut found_set_request = false;
        for msg in result {
            if let OutboundDelegateMsg::SetSecretRequest(req) = msg {
                if req.value.is_none() {
                    found_set_request = true;
                    break;
                }
            }
        }
        assert!(
            found_set_request,
            "No SetSecretRequest with None value found"
        );
    }

    #[test]
    fn test_list_request() {
        let request = ChatDelegateRequestMsg::ListRequest;
        let dummy_app_id = ContractInstanceId::new([4u8; 32]); // Dummy ID for test
        let app_msg = create_app_message(request, dummy_app_id);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = crate::ChatDelegate::process(
            create_test_parameters(),
            Some(get_test_origin_bytes()),
            inbound_msg,
        )
        .unwrap();

        // Should have 1 message: get index request
        assert_eq!(result.len(), 1);

        // Check it's a get secret request for the index
        match &result[0] {
            OutboundDelegateMsg::GetSecretRequest(req) => {
                // Verify the key contains our app ID and key_index suffix
                let key_str = String::from_utf8(req.key.key().to_vec())
                    .map_err(|e| panic!("Invalid UTF-8 in key: {e}"))
                    .unwrap();
                assert!(key_str.contains(KEY_INDEX_SUFFIX));
            }
            _ => panic!("Expected GetSecretRequest, got {:?}", result[0]),
        }
    }

    #[test]
    fn test_get_secret_response_for_regular_get() {
        let key = b"test_key".to_vec();
        let value = b"test_value".to_vec();

        // Create a context with a pending get
        let mut context = ChatDelegateContext::default();

        let test_origin = create_test_origin();
        // Need a dummy app id for the test context
        let dummy_app_id = ContractInstanceId::new([0u8; 32]);

        let key_delegate = river_core::chat_delegate::ChatDelegateKey(key.clone());
        let app_key = create_origin_key(&test_origin, &key_delegate);
        context.pending_ops.insert(
            SecretIdKey::from(&app_key),
            PendingOperation::Get {
                origin: test_origin.clone(),
                client_key: river_core::chat_delegate::ChatDelegateKey(key.clone()),
                app: dummy_app_id, // Add dummy app id
            },
        );

        // Serialize the context
        let context_bytes = DelegateContext::try_from(&context)
            .map_err(|e| panic!("Failed to serialize context: {e}"))
            .unwrap();

        // Create a get secret response
        let get_response = freenet_stdlib::prelude::GetSecretResponse {
            key: app_key.clone(),
            value: Some(value.clone()),
            context: context_bytes,
        };

        let inbound_msg = InboundDelegateMsg::GetSecretResponse(get_response);

        // Pass the attested origin parameter
        let result = crate::ChatDelegate::process(
            create_test_parameters(),
            Some(get_test_origin_bytes()),
            inbound_msg,
        )
        .unwrap();

        // Should have 1 message: app response
        assert_eq!(result.len(), 1);

        // Check app response
        let response = extract_response(result).unwrap();
        match response {
            ChatDelegateResponseMsg::GetResponse {
                key: resp_key,
                value: resp_value,
            } => {
                assert_eq!(
                    resp_key,
                    river_core::chat_delegate::ChatDelegateKey(key.clone())
                );
                assert_eq!(resp_value, Some(value));
            }
            _ => panic!("Expected GetResponse, got {:?}", response),
        }
    }

    #[test]
    fn test_get_secret_response_for_list_request() {
        let keys = vec![b"key1".to_vec(), b"key2".to_vec(), b"key3".to_vec()];

        // Create a key index with some keys
        let wrapped_keys: Vec<river_core::chat_delegate::ChatDelegateKey> = keys
            .clone()
            .into_iter()
            .map(river_core::chat_delegate::ChatDelegateKey)
            .collect();
        let key_index = KeyIndex { keys: wrapped_keys };
        let mut index_bytes = Vec::new();
        ciborium::ser::into_writer(&key_index, &mut index_bytes)
            .map_err(|e| panic!("Failed to serialize key index: {e}"))
            .unwrap();

        let test_origin = create_test_origin();
        // Need a dummy app id for the test context
        let dummy_app_id = ContractInstanceId::new([1u8; 32]);

        // Create a context with a pending list request
        let mut context = ChatDelegateContext::default();
        let index_key = create_index_key(&test_origin);
        context.pending_ops.insert(
            SecretIdKey::from(&index_key),
            PendingOperation::List {
                origin: test_origin.clone(),
                app: dummy_app_id, // Add dummy app id
            },
        );

        // Serialize the context
        let context_bytes = DelegateContext::try_from(&context)
            .map_err(|e| panic!("Failed to serialize context: {e}"))
            .unwrap();

        // Create a get secret response for the index
        let get_response = freenet_stdlib::prelude::GetSecretResponse {
            key: index_key.clone(),
            value: Some(index_bytes),
            context: context_bytes,
        };

        let inbound_msg = InboundDelegateMsg::GetSecretResponse(get_response);

        // Pass the attested origin parameter
        let result = crate::ChatDelegate::process(
            create_test_parameters(),
            Some(get_test_origin_bytes()),
            inbound_msg,
        )
        .unwrap();

        // Should have 1 message: app response
        assert_eq!(result.len(), 1);

        // Check app response
        let response = extract_response(result).unwrap();
        match response {
            ChatDelegateResponseMsg::ListResponse { keys: resp_keys } => {
                let wrapped_keys: Vec<river_core::chat_delegate::ChatDelegateKey> = keys
                    .clone()
                    .into_iter()
                    .map(river_core::chat_delegate::ChatDelegateKey)
                    .collect();
                assert_eq!(resp_keys, wrapped_keys);
            }
            _ => panic!("Expected ListResponse, got {:?}", response),
        }
    }

    #[test]
    fn test_get_secret_response_for_store_request() {
        let key = b"test_key".to_vec();

        // Create a key index with some existing keys
        let existing_keys = vec![b"existing_key".to_vec()];
        let key_index = KeyIndex {
            keys: existing_keys
                .into_iter()
                .map(river_core::chat_delegate::ChatDelegateKey)
                .collect(),
        };
        let mut index_bytes = Vec::new();
        ciborium::ser::into_writer(&key_index, &mut index_bytes)
            .map_err(|e| panic!("Failed to serialize key index: {e}"))
            .unwrap();

        let test_origin = create_test_origin();

        // Create a context with a pending store request
        let mut context = ChatDelegateContext::default();
        let index_key = create_index_key(&test_origin);
        context.pending_ops.insert(
            SecretIdKey::from(&index_key),
            PendingOperation::Store {
                origin: test_origin.clone(),
                client_key: river_core::chat_delegate::ChatDelegateKey(key.clone()),
            },
        );

        // Serialize the context
        let context_bytes = DelegateContext::try_from(&context)
            .map_err(|e| panic!("Failed to serialize context: {e}"))
            .unwrap();

        // Create a get secret response for the index
        let get_response = freenet_stdlib::prelude::GetSecretResponse {
            key: index_key.clone(),
            value: Some(index_bytes),
            context: context_bytes,
        };

        let inbound_msg = InboundDelegateMsg::GetSecretResponse(get_response);

        // Pass the attested origin parameter
        let result = crate::ChatDelegate::process(
            create_test_parameters(),
            Some(get_test_origin_bytes()),
            inbound_msg,
        )
        .unwrap();

        // Should have 1 message: set secret request to update the index
        assert_eq!(result.len(), 1);

        // Check it's a set secret request
        match &result[0] {
            OutboundDelegateMsg::SetSecretRequest(req) => {
                // Deserialize the value to check the updated index
                let updated_index: KeyIndex =
                    ciborium::from_reader(req.value.as_ref().unwrap().as_slice())
                        .map_err(|e| panic!("Failed to deserialize updated index: {e}"))
                        .unwrap();

                // Should contain both the existing key and our new key
                assert_eq!(updated_index.keys.len(), 2);
                let key_wrapped = river_core::chat_delegate::ChatDelegateKey(key.clone());
                let existing_key_wrapped =
                    river_core::chat_delegate::ChatDelegateKey(b"existing_key".to_vec());
                assert!(updated_index.keys.contains(&key_wrapped));
                assert!(updated_index.keys.contains(&existing_key_wrapped));
            }
            _ => panic!("Expected SetSecretRequest, got {:?}", result[0]),
        }
    }

    #[test]
    fn test_error_on_processed_message() {
        let key = b"test_key".to_vec();
        let value = b"test_value".to_vec();

        let request = ChatDelegateRequestMsg::StoreRequest {
            key: river_core::chat_delegate::ChatDelegateKey(key),
            value,
        };
        let dummy_app_id = ContractInstanceId::new([5u8; 32]); // Dummy ID for test
        let mut app_msg = create_app_message(request, dummy_app_id);
        app_msg = app_msg.processed(true); // Mark as already processed
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let _origin = create_test_origin();

        let result = crate::ChatDelegate::process(
            create_test_parameters(),
            Some(get_test_origin_bytes()),
            inbound_msg,
        );
        assert!(result.is_err());

        if let Err(DelegateError::Other(msg)) = result {
            assert!(msg.contains("cannot process an already processed message"));
        } else {
            panic!("Expected DelegateError::Other, got {:?}", result);
        }
    }

    #[test]
    fn test_error_on_unexpected_message_type() {
        let get_secret_request = GetSecretRequest {
            key: SecretsId::new(vec![1, 2, 3]),
            context: DelegateContext::default(),
            processed: false,
        };

        let inbound_msg = InboundDelegateMsg::GetSecretRequest(get_secret_request);

        let _origin = create_test_origin();

        let result = crate::ChatDelegate::process(
            create_test_parameters(),
            Some(get_test_origin_bytes()),
            inbound_msg,
        );
        assert!(result.is_err());

        if let Err(DelegateError::Other(msg)) = result {
            assert!(msg.contains("unexpected message type"));
        } else {
            panic!("Expected DelegateError::Other, got {:?}", result);
        }
    }

    #[test]
    fn test_error_on_missing_attested() {
        let key = b"test_key".to_vec();

        let request = ChatDelegateRequestMsg::GetRequest {
            key: river_core::chat_delegate::ChatDelegateKey(key),
        };
        let dummy_app_id = ContractInstanceId::new([6u8; 32]); // Dummy ID for test
        let app_msg = create_app_message(request, dummy_app_id);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        // Pass None for attested
        let result = crate::ChatDelegate::process(create_test_parameters(), None, inbound_msg);
        assert!(result.is_err());

        if let Err(DelegateError::Other(msg)) = result {
            assert!(msg.contains("missing attested origin"));
        } else {
            panic!("Expected DelegateError::Other, got {:?}", result);
        }
    }

    // Helper function to create a test origin
    fn create_test_origin() -> Origin {
        Origin(vec![0u8, 0u8, 0u8, 0u8])
    }

    // Get the bytes from the test origin for process calls
    fn get_test_origin_bytes() -> &'static [u8] {
        // Using lazy_static would be better in a real app
        // but for tests this is simpler
        static TEST_ORIGIN: [u8; 4] = [0u8, 0u8, 0u8, 0u8];
        &TEST_ORIGIN
    }
}
