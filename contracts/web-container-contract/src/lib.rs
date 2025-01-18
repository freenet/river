use ciborium::{de::from_reader, ser::into_writer}; 
use ed25519_dalek::{Signature, VerifyingKey};
use freenet_stdlib::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct WebContainerMetadata {
    version: u32,
    signature: Vec<u8>,  // Signature of web interface + version number
}

struct Contract;

#[contract]
impl ContractInterface for Contract {
    fn validate_state(
        parameters: Parameters<'static>,
        state: State<'static>,
        _related: RelatedContracts<'static>,
    ) -> Result<ValidateResult, ContractError> {
        if state.as_ref().is_empty() {
            return Ok(ValidateResult::Valid);
        }

        // Extract and deserialize verifying key from parameters
        let params_bytes: &[u8] = parameters.as_ref();
        if params_bytes.len() != 32 {
            return Err(ContractError::Other("Parameters must be 32 bytes".to_string()));
        }
        let mut key_bytes = [0u8; 32];
        key_bytes.copy_from_slice(params_bytes);
        
        let verifying_key = VerifyingKey::from_bytes(&key_bytes)
            .map_err(|e| ContractError::Other(format!("Invalid public key: {}", e)))?;

        // Decode metadata from state
        let mut cursor = std::io::Cursor::new(state.as_ref());
        let metadata: WebContainerMetadata = from_reader(&mut cursor)
            .map_err(|e| ContractError::Deser(e.to_string()))?;

        if metadata.version == 0 {
            return Err(ContractError::InvalidState);
        }

        // Get remaining bytes after metadata (the web interface content)
        let web_content = &state.as_ref()[cursor.position() as usize..];

        // Create message to verify (version + web content)
        let mut message = metadata.version.to_be_bytes().to_vec();
        message.extend_from_slice(web_content);

        // Verify signature
        let signature = Signature::from_slice(&metadata.signature)
            .map_err(|e| ContractError::Other(format!("Invalid signature format: {}", e)))?;
            
        verifying_key.verify_strict(&message, &signature)
            .map_err(|e| ContractError::Other(format!("Signature verification failed: {}", e)))?;

        Ok(ValidateResult::Valid)
    }

    fn update_state(
        parameters: Parameters<'static>,
        state: State<'static>,
        data: Vec<UpdateData<'static>>,
    ) -> Result<UpdateModification<'static>, ContractError> {
        // Get current version
        let current_version = if state.as_ref().is_empty() {
            0
        } else {
            let mut cursor = std::io::Cursor::new(state.as_ref());
            let metadata: WebContainerMetadata = from_reader(&mut cursor)
                .map_err(|e| ContractError::Deser(e.to_string()))?;
            metadata.version
        };

        // Process update data
        if let Some(UpdateData::State(new_state)) = data.into_iter().next() {
            // Verify new state has higher version
            let mut cursor = std::io::Cursor::new(new_state.as_ref());
            let metadata: WebContainerMetadata = from_reader(&mut cursor)
                .map_err(|e| ContractError::Deser(e.to_string()))?;

            if metadata.version <= current_version {
                return Err(ContractError::InvalidUpdate);
            }

            Ok(UpdateModification::valid(new_state))
        } else {
            Err(ContractError::InvalidUpdate)
        }
    }

    fn summarize_state(
        _parameters: Parameters<'static>,
        state: State<'static>,
    ) -> Result<StateSummary<'static>, ContractError> {
        if state.as_ref().is_empty() {
            return Ok(StateSummary::from(Vec::new()));
        }

        // Just return version as summary
        let mut cursor = std::io::Cursor::new(state.as_ref());
        let metadata: WebContainerMetadata = from_reader(&mut cursor)
            .map_err(|e| ContractError::Deser(e.to_string()))?;

        let mut summary = Vec::new();
        into_writer(&metadata.version, &mut summary)
            .map_err(|e| ContractError::Deser(e.to_string()))?;

        Ok(StateSummary::from(summary))
    }

    fn get_state_delta(
        _parameters: Parameters<'static>,
        state: State<'static>,
        summary: StateSummary<'static>,
    ) -> Result<StateDelta<'static>, ContractError> {
        if state.as_ref().is_empty() {
            return Ok(StateDelta::from(Vec::new()));
        }

        // Compare versions
        let current_version = {
            let mut cursor = std::io::Cursor::new(state.as_ref());
            let metadata: WebContainerMetadata = from_reader(&mut cursor)
                .map_err(|e| ContractError::Deser(e.to_string()))?;
            metadata.version
        };

        let summary_version: u32 = from_reader(summary.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;

        if current_version > summary_version {
            // Return full state if version is newer
            Ok(StateDelta::from(state.as_ref().to_vec()))
        } else {
            Ok(StateDelta::from(Vec::new()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_state_is_valid() {
        let result = Contract::validate_state(
            Parameters::from(vec![]),
            State::from(vec![]),
            RelatedContracts::default(),
        );
        assert!(matches!(result, Ok(ValidateResult::Valid)));
    }
}
