use super::*;

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
        ChatDelegateRequestMsg::StoreRequest { key, value } => {
            logging::info(
                format!("Delegate received StoreRequest key: {key:?}, value: {value:?}").as_str(),
            );
            handle_store_request(&mut context, origin, key, value)
        }
        ChatDelegateRequestMsg::GetRequest { key } => {
            logging::info(
                format!("Delegate received GetRequest key: {key:?}").as_str(),
            );
            handle_get_request(&mut context, origin, key)
        }
        ChatDelegateRequestMsg::DeleteRequest { key } => {
            logging::info(
                format!("Delegate received DeleteRequest key: {key:?}").as_str(),
            );
            handle_delete_request(&mut context, origin, key)
        }
        ChatDelegateRequestMsg::ListRequest => {
            logging::info("Delegate received ListRequest");
            handle_list_request(&mut context, origin)
        }
    }
}

/// Handle a store request
pub(crate) fn handle_store_request(
    context: &mut ChatDelegateContext,
    origin: &Origin,
    key: ChatDelegateKey,
    value: Vec<u8>,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    // Create a unique key for this app's data
    let secret_id = create_origin_key(origin, &key);

    // Create the index key
    let index_key = create_index_key(origin);

    // Store the original request in context for later processing after we get the index
    context
        .pending_ops
        .insert(SecretIdKey::from(&index_key), PendingOperation::Store {
            origin: origin.clone(),
            client_key: key.clone(),
        });

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
    let app_response = create_app_response(&response, &context_bytes)?;

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
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    // Create a unique key for this origin contract's data
    let secret_id = create_origin_key(origin, &key);

    // Store the original request in context for later processing
    context
        .pending_ops
        .insert(SecretIdKey::from(&secret_id), PendingOperation::Get {
            origin: origin.clone(),
            client_key: key.clone(),
        });

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
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    // Create a unique key for this app's data
    let secret_id = create_origin_key(origin, &key);

    // Create the index key
    let index_key = create_index_key(origin);

    // Store the original request in context for later processing after we get the index
    context
        .pending_ops
        .insert(SecretIdKey::from(&index_key), PendingOperation::Delete {
            origin: origin.clone(),
            client_key: key.clone(),
        });

    // Create response for the client
    let response = ChatDelegateResponseMsg::DeleteResponse {
        key: key.clone(),
        result: Ok(()),
    };

    // Serialize context
    let context_bytes = DelegateContext::try_from(&*context)?;

    // Create the three messages we need to send:
    // 1. Response to the client
    let app_response = create_app_response(&response, &context_bytes)?;

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
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    // Create the index key
    let index_key = create_index_key(origin);

    // Store a special marker in the context to indicate this is a list request
    context
        .pending_ops
        .insert(SecretIdKey::from(&index_key), PendingOperation::List {
            origin: origin.clone(),
        });

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

    // Get the key as a string to check if it's an index key
    let key_str = String::from_utf8_lossy(get_secret_response.key.key()).to_string();
    let key_clone = get_secret_response.key.clone();

    logging::info(&format!("Processing response for key: {key_str}"));

    // Check if this is a key index response
    let result = if key_str.ends_with(KEY_INDEX_SUFFIX) {
        logging::info("This is a key index response");
        handle_key_index_response(&key_clone, &mut context, get_secret_response)
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
                logging::info(&format!("Failed to deserialize key index, creating new one: {e}"));
                KeyIndex::default()
            })
        } else {
            logging::info("No index data found, creating new index");
            KeyIndex::default()
        };

        match &pending_op {
            PendingOperation::List { .. } => {
                // Create list response
                let response = ChatDelegateResponseMsg::ListResponse {
                    keys: key_index.keys.clone(),
                };

                // Create response message
                let context_bytes = DelegateContext::try_from(&ChatDelegateContext::default())?;
                let app_response = create_app_response(&response, &context_bytes)?;
                outbound_msgs.push(app_response);
                logging::info(&format!("Created list response with {} keys", key_index.keys.len()));
            },
            PendingOperation::Store { client_key, .. } | PendingOperation::Delete { client_key, .. } => {
                // This is a store or delete operation that needs to update the index
                let is_delete = pending_op.is_delete_operation();

                if is_delete {
                    // For delete operations, remove the key
                    key_index.keys.retain(|k| k != client_key);
                    logging::info(&format!("Removed key from index, now has {} keys", key_index.keys.len()));
                } else {
                    // For store operations, add the key if it doesn't exist
                    if !key_index.keys.contains(client_key) {
                        key_index.keys.push(client_key.clone());
                        logging::info(&format!("Added key to index, now has {} keys", key_index.keys.len()));
                    } else {
                        logging::info("Key already exists in index, not adding");
                    }
                }

                // Serialize the updated index
                let mut index_bytes = Vec::new();
                ciborium::ser::into_writer(&key_index, &mut index_bytes)
                    .map_err(|e| DelegateError::Deser(format!("Failed to serialize key index: {e}")))?;

                // Create set secret request to update the index
                let set_index = OutboundDelegateMsg::SetSecretRequest(SetSecretRequest {
                    key: secret_id.clone(),
                    value: Some(index_bytes),
                });

                outbound_msgs.push(set_index);
            },
            PendingOperation::Get { .. } => {
                return Err(DelegateError::Other(
                    "Unexpected Get operation for key index response".to_string()
                ));
            }
        }

        // Remove the pending operation
        context.pending_ops.remove(&secret_id_key);

        logging::info(&format!("Returning {} outbound messages", outbound_msgs.len()));
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
    if let Some(PendingOperation::Get { client_key, .. }) = context.pending_ops.get(&secret_id_key).cloned() {
        // Create response
        let response = ChatDelegateResponseMsg::GetResponse {
            key: client_key.clone(),
            value: get_secret_response.value.clone(),
        };

        // Create response message
        let context_bytes = DelegateContext::try_from(&ChatDelegateContext::default())?;
        let app_response = create_app_response(&response, &context_bytes)?;

        // Remove the pending get request
        context.pending_ops.remove(&secret_id_key);

        logging::info(&format!(
            "Returning get response for key: {:?}, value present: {}",
            client_key,
            get_secret_response.value.is_some()
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
    fn create_app_message(request: ChatDelegateRequestMsg) -> ApplicationMessage {
        let mut payload = Vec::new();
        ciborium::ser::into_writer(&request, &mut payload)
            .map_err(|e| panic!("Failed to serialize request: {e}"))
            .unwrap();
        ApplicationMessage::new(payload)
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
            key: river_common::chat_delegate::ChatDelegateKey(key.clone()),
            value: value.clone(),
        };

        let app_msg = create_app_message(request);
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
                assert_eq!(resp_key, river_common::chat_delegate::ChatDelegateKey(key.clone()));
                assert!(result.is_ok());
                assert_eq!(value_size, value.len());
            }
            _ => panic!("Expected StoreResponse, got {:?}", response),
        }
    }

    #[test]
    fn test_get_request() {
        let key = b"test_key".to_vec();

        let request = ChatDelegateRequestMsg::GetRequest { key: river_common::chat_delegate::ChatDelegateKey(key.clone()) };
        let app_msg = create_app_message(request);
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

        let request = ChatDelegateRequestMsg::DeleteRequest { key: river_common::chat_delegate::ChatDelegateKey(key.clone()) };
        let app_msg = create_app_message(request);
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
                assert_eq!(resp_key, river_common::chat_delegate::ChatDelegateKey(key.clone()));
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
        let app_msg = create_app_message(request);
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

        let key_delegate = river_common::chat_delegate::ChatDelegateKey(key.clone());
        let app_key = create_origin_key(&test_origin, &key_delegate);
        context
            .pending_ops
            .insert(SecretIdKey::from(&app_key), PendingOperation::Get {
                origin: test_origin.clone(),
                client_key: river_common::chat_delegate::ChatDelegateKey(key.clone()),
            });

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
                assert_eq!(resp_key, river_common::chat_delegate::ChatDelegateKey(key.clone()));
                assert_eq!(resp_value, Some(value));
            }
            _ => panic!("Expected GetResponse, got {:?}", response),
        }
    }

    #[test]
    fn test_get_secret_response_for_list_request() {
        let keys = vec![b"key1".to_vec(), b"key2".to_vec(), b"key3".to_vec()];

        // Create a key index with some keys
        let wrapped_keys: Vec<river_common::chat_delegate::ChatDelegateKey> = keys.clone()
            .into_iter()
            .map(|k| river_common::chat_delegate::ChatDelegateKey(k))
            .collect();
        let key_index = KeyIndex { keys: wrapped_keys };
        let mut index_bytes = Vec::new();
        ciborium::ser::into_writer(&key_index, &mut index_bytes)
            .map_err(|e| panic!("Failed to serialize key index: {e}"))
            .unwrap();

        let test_origin = create_test_origin();

        // Create a context with a pending list request
        let mut context = ChatDelegateContext::default();
        let index_key = create_index_key(&test_origin);
        context
            .pending_ops
            .insert(SecretIdKey::from(&index_key), PendingOperation::List {
                origin: test_origin.clone(),
            });

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
                let wrapped_keys: Vec<river_common::chat_delegate::ChatDelegateKey> = keys.clone()
                    .into_iter()
                    .map(|k| river_common::chat_delegate::ChatDelegateKey(k))
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
            keys: existing_keys.into_iter()
                .map(|k| river_common::chat_delegate::ChatDelegateKey(k))
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
        context
            .pending_ops
            .insert(SecretIdKey::from(&index_key), PendingOperation::Store {
                origin: test_origin.clone(),
                client_key: river_common::chat_delegate::ChatDelegateKey(key.clone()),
            });

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
                let key_wrapped = river_common::chat_delegate::ChatDelegateKey(key.clone());
                let existing_key_wrapped = river_common::chat_delegate::ChatDelegateKey(b"existing_key".to_vec());
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
            key: river_common::chat_delegate::ChatDelegateKey(key), 
            value 
        };

        let mut app_msg = create_app_message(request);
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
            key: river_common::chat_delegate::ChatDelegateKey(key) 
        };
        let app_msg = create_app_message(request);
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
