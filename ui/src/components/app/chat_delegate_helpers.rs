use freenet_stdlib::prelude::{DelegateCode, DelegateContainer, DelegateWasmAPIVersion, Delegate, Parameters};

/// Create a DelegateContainer from raw bytes and parameters
pub fn create_delegate_container(
    delegate_bytes: &[u8],
    parameters: Parameters<'_>,
) -> Result<DelegateContainer, std::io::Error> {
    // Convert parameters to owned version
    let params = parameters.clone().into_owned();
    
    // Load the delegate code with version information
    let (delegate_code, version) = DelegateCode::load_versioned_from_bytes(delegate_bytes.to_vec())?;
    
    // Create the delegate container
    let delegate = DelegateContainer::Wasm(DelegateWasmAPIVersion::V1(
        Delegate::from((&delegate_code, &params))
    ));
    
    Ok(delegate)
}
