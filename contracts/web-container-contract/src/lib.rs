use ciborium::{de::from_reader, ser::into_writer}; 
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::prelude::*;
use river_common::web_container::WebContainerMetadata;

struct WebContainerContract;

#[contract]
impl ContractInterface for WebContainerContract {
    fn validate_state(
        parameters: Parameters<'static>,
        state: State<'static>,
        _related: RelatedContracts<'static>,
    ) -> Result<ValidateResult, ContractError> {
        freenet_stdlib::log::info("Validating web container state");

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

        // Get remaining bytes after metadata (the compressed webapp)
        let compressed_webapp = &state.as_ref()[cursor.position() as usize..];

        // Create message to verify (version + compressed webapp)
        let mut message = metadata.version.to_be_bytes().to_vec();
        message.extend_from_slice(compressed_webapp);

        verifying_key.verify_strict(&message, &metadata.signature)
            .map_err(|e| ContractError::Other(format!("Signature verification failed: {}", e)))?;

        Ok(ValidateResult::Valid)
    }

    fn update_state(
        _parameters: Parameters<'static>,
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

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use ed25519_dalek::{SigningKey, Signer};
    use rand::rngs::OsRng;

    fn create_test_keypair() -> (SigningKey, VerifyingKey) {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        (signing_key, verifying_key)
    }

    fn create_test_state(version: u32, compressed_webapp: &[u8], signing_key: &SigningKey) -> Vec<u8> {
        // Create message to sign (version + compressed webapp)
        let mut message = version.to_be_bytes().to_vec();
        message.extend_from_slice(compressed_webapp);
        
        // Sign the message
        let signature = signing_key.sign(&message);
        
        // Create metadata
        let metadata = WebContainerMetadata {
            version,
            signature,
        };

        // Serialize everything
        let mut state = Vec::new();
        into_writer(&metadata, &mut state).unwrap();
        state.extend_from_slice(compressed_webapp);
        state
    }

    #[test]
    fn test_empty_state_fails_validation() {
        let result = WebContainerContract::validate_state(
            Parameters::from(vec![]),
            State::from(vec![]),
            RelatedContracts::default(),
        );
        assert!(matches!(result, Err(ContractError::Other(_))));
    }

    #[test]
    fn test_valid_state() {
        let (signing_key, verifying_key) = create_test_keypair();
        let compressed_webapp = b"Hello, World!";
        let state = create_test_state(1, compressed_webapp, &signing_key);

        let result = WebContainerContract::validate_state(
            Parameters::from(verifying_key.to_bytes().to_vec()),
            State::from(state),
            RelatedContracts::default(),
        );
        assert!(matches!(result, Ok(ValidateResult::Valid)));
    }

    #[test]
    fn test_invalid_version() {
        let (signing_key, verifying_key) = create_test_keypair();
        let compressed_webapp = b"Hello, World!";
        let state = create_test_state(0, compressed_webapp, &signing_key);

        let result = WebContainerContract::validate_state(
            Parameters::from(verifying_key.to_bytes().to_vec()),
            State::from(state),
            RelatedContracts::default(),
        );
        assert!(matches!(result, Err(ContractError::InvalidState)));
    }

    #[test]
    fn test_invalid_signature() {
        let (_, verifying_key) = create_test_keypair();
        let (wrong_signing_key, _) = create_test_keypair();
        let compressed_webapp = b"Hello, World!";
        let state = create_test_state(1, compressed_webapp, &wrong_signing_key);

        let result = WebContainerContract::validate_state(
            Parameters::from(verifying_key.to_bytes().to_vec()),
            State::from(state),
            RelatedContracts::default(),
        );
        assert!(matches!(result, Err(ContractError::Other(_))));
    }

    #[test]
    fn test_update_state_version_check() {
        let (signing_key, _) = create_test_keypair();
        
        // Create current state with version 1
        let current_state = create_test_state(1, b"Original", &signing_key);
        
        // Try to update with same version
        let new_state = create_test_state(1, b"New Content", &signing_key);
        
        let result = WebContainerContract::update_state(
            Parameters::from(vec![]),
            State::from(current_state.clone()),
            vec![UpdateData::State(State::from(new_state))],
        );
        assert!(matches!(result, Err(ContractError::InvalidUpdate)));
        
        // Try to update with higher version
        let new_state = create_test_state(2, b"New Content", &signing_key);
        
        let result = WebContainerContract::update_state(
            Parameters::from(vec![]),
            State::from(current_state),
            vec![UpdateData::State(State::from(new_state))],
        );
        assert!(matches!(result, Ok(_)));
    }

    #[test]
    fn test_summarize_and_delta() {
        let (signing_key, _) = create_test_keypair();
        let state = create_test_state(2, b"Content", &signing_key);
        
        // Test summarize
        let summary = WebContainerContract::summarize_state(
            Parameters::from(vec![]),
            State::from(state.clone()),
        ).unwrap();
        
        let summary_version: u32 = from_reader(summary.as_ref()).unwrap();
        assert_eq!(summary_version, 2);
        
        // Test delta with older summary
        let mut old_summary = Vec::new();
        into_writer(&1u32, &mut old_summary).unwrap();
        
        let delta = WebContainerContract::get_state_delta(
            Parameters::from(vec![]),
            State::from(state.clone()),
            StateSummary::from(old_summary),
        ).unwrap();
        
        assert!(!delta.as_ref().is_empty());
        
        // Test delta with same version
        let mut same_summary = Vec::new();
        into_writer(&2u32, &mut same_summary).unwrap();
        
        let delta = WebContainerContract::get_state_delta(
            Parameters::from(vec![]),
            State::from(state),
            StateSummary::from(same_summary),
        ).unwrap();
        
        assert!(delta.as_ref().is_empty());
    }
}
