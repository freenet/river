use super::*;
use freenet_stdlib::prelude::ContractInstanceId;

/// Helper function to create a unique secret key for an origin's data
pub(crate) fn create_origin_key(origin: &Origin, key: &ChatDelegateKey) -> Vec<u8> {
    format!(
        "{}{}{}",
        origin.to_b58(),
        ORIGIN_KEY_SEPARATOR,
        String::from_utf8_lossy(key.as_bytes())
    )
    .into_bytes()
}

/// Helper function to create an index key for an origin
pub(crate) fn create_index_key(origin: &Origin) -> Vec<u8> {
    format!(
        "{}{}{}",
        origin.to_b58(),
        ORIGIN_KEY_SEPARATOR,
        KEY_INDEX_SUFFIX
    )
    .into_bytes()
}

/// Helper function to create an app response
pub(crate) fn create_app_response<T: Serialize>(
    response: &T,
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
        .with_context(DelegateContext::default())
        .processed(true);

    Ok(OutboundDelegateMsg::ApplicationMessage(app_msg))
}
