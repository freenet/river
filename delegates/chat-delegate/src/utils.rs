use super::*;
use freenet_stdlib::prelude::ContractInstanceId;

/// Helper function to create a unique app key
pub(crate) fn create_origin_key(origin: &Origin, key: &ChatDelegateKey) -> SecretsId {
    SecretsId::new(
        format!(
            "{}{}{}",
            origin.to_b58(),
            ORIGIN_KEY_SEPARATOR,
            String::from_utf8_lossy(key.as_bytes()).to_string()
        )
        .into_bytes(),
    )
}

/// Helper function to create an index key
pub(crate) fn create_index_key(origin: &Origin) -> SecretsId {
    SecretsId::new(
        format!(
            "{}{}{}",
            origin.to_b58(),
            ORIGIN_KEY_SEPARATOR,
            KEY_INDEX_SUFFIX
        )
        .into_bytes(),
    )
}

/// Helper function to create a get request
pub(crate) fn create_get_request(
    secret_id: SecretsId,
    context: &DelegateContext,
) -> Result<OutboundDelegateMsg, DelegateError> {
    let get_secret = OutboundDelegateMsg::GetSecretRequest(GetSecretRequest {
        key: secret_id,
        context: context.clone(),
        processed: false,
    });

    Ok(get_secret)
}

/// Helper function to create a get index request
pub(crate) fn create_get_index_request(
    index_secret_id: SecretsId,
    context: &DelegateContext,
) -> Result<OutboundDelegateMsg, DelegateError> {
    let get_index = OutboundDelegateMsg::GetSecretRequest(GetSecretRequest {
        key: index_secret_id,
        context: context.clone(),
        processed: false,
    });

    Ok(get_index)
}

/// Helper function to create an app response
pub(crate) fn create_app_response<T: Serialize>(
    response: &T,
    context: &DelegateContext,
    app: ContractInstanceId,
) -> Result<OutboundDelegateMsg, DelegateError> {
    // Serialize response
    let mut response_bytes = Vec::new();
    ciborium::ser::into_writer(response, &mut response_bytes)
        .map_err(|e| DelegateError::Deser(format!("Failed to serialize response: {e}")))?;

    logging::info(&format!(
        "Creating app response with {} bytes",
        response_bytes.len()
    ));

    // Create response message
    let app_msg = ApplicationMessage::new(app, response_bytes)
        .with_context(context.clone())
        .processed(false); //

    Ok(OutboundDelegateMsg::ApplicationMessage(app_msg))
}
