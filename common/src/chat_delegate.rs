use serde::{Deserialize, Serialize};

/// Messages sent from the App to the Chat Delegate
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChatDelegateRequestMsg {
    StoreRequest { key: Vec<u8>, value: Vec<u8> },
    GetRequest { key: Vec<u8> },
    DeleteRequest { key: Vec<u8> },
    ListRequest,
}

/// Responses sent from the Chat Delegate to the App
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChatDelegateResponseMsg {
    GetResponse {
        key: Vec<u8>,
        value: Option<Vec<u8>>,
    },
    ListResponse {
        keys: Vec<Vec<u8>>,
    },
    StoreResponse {
        key: Vec<u8>,
        result: Result<(), String>,
    },
    DeleteResponse {
        key: Vec<u8>,
        result: Result<(), String>,
    },
}

/// Helper functions for working with delegate data
pub mod helpers {
    use freenet_stdlib::prelude::{DelegateCode, DelegateContainer, DelegateWasmAPIVersion, Parameters};
    
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
            freenet_stdlib::prelude::Delegate::from((&delegate_code, &params))
        ));
        
        Ok(delegate)
    }
}
