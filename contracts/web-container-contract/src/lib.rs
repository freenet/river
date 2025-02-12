use byteorder::{BigEndian, ReadBytesExt};
use ciborium::{de::from_reader, ser::into_writer};
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::prelude::*;
use river_common::web_container::WebContainerMetadata;
use std::io::{Cursor, Read};

const MAX_METADATA_SIZE: u64 = 1024;  // 1KB
const MAX_WEB_SIZE: u64 = 1024 * 1024 * 100;  // 100MB

struct WebContainerContract;

#[contract]
impl ContractInterface for WebContainerContract {
    fn validate_state(
        parameters: Parameters<'static>,
        state: State<'static>,
        _related: RelatedContracts<'static>,
    ) -> Result<ValidateResult, ContractError> {
        #[cfg(not(test))]
        {
            freenet_stdlib::log::info("Starting validate_state");
            freenet_stdlib::log::info(&format!("Parameters length: {}", parameters.as_ref().len()));
            freenet_stdlib::log::info(&format!("State length: {}", state.as_ref().len()));
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

        // Parse WebApp format following the specification:
        // [metadata_length: u64][metadata: bytes][web_length: u64][web: bytes]
        let mut cursor = Cursor::new(state.as_ref());
        
        // Read and validate metadata length
        let metadata_size = cursor
            .read_u64::<BigEndian>()
            .map_err(|e| ContractError::Other(format!("Failed to read metadata size: {}", e)))?;
            
        // Read metadata bytes
        let mut metadata_bytes = vec![0; metadata_size as usize];
        cursor
            .read_exact(&mut metadata_bytes)
            .map_err(|e| ContractError::Other(format!("Failed to read metadata: {}", e)))?;

        // Parse metadata as CBOR
        let metadata: WebContainerMetadata = match from_reader(&metadata_bytes[..]) {
            Ok(m) => m,
            Err(e) => {
                #[cfg(not(test))]
                freenet_stdlib::log::info(&format!("CBOR parsing error: {}", e));
                return Err(ContractError::Deser(e.to_string()));
            }
        };

        if metadata.version == 0 {
            return Err(ContractError::InvalidState);
        }

        // Read web content length
        let web_size = cursor
            .read_u64::<BigEndian>()
            .map_err(|e| ContractError::Other(format!("Failed to read web size: {}", e)))?;

        if web_size > MAX_WEB_SIZE {
            return Err(ContractError::Other(format!(
                "Web size {} exceeds maximum allowed size of {} bytes",
                web_size, MAX_WEB_SIZE
            )));
        }

        // Read the actual web content
        let mut webapp_bytes = vec![0; web_size as usize];
        cursor
            .read_exact(&mut webapp_bytes)
            .map_err(|e| ContractError::Other(format!("Failed to read web bytes: {}", e)))?;

        // Create message to verify (version + web content only)
        // This matches the signing tool's message construction
        let mut message = metadata.version.to_be_bytes().to_vec();
        message.extend_from_slice(&webapp_bytes);

        #[cfg(not(test))]
        {
            freenet_stdlib::log::info(&format!("Verifying signature for version: {}", metadata.version));
            freenet_stdlib::log::info(&format!("Version bytes (hex): {:02x?}", &metadata.version.to_be_bytes()));
            freenet_stdlib::log::info(&format!("First 100 webapp bytes (hex): {:02x?}", &webapp_bytes[..100.min(webapp_bytes.len())]));
            freenet_stdlib::log::info(&format!("First 100 message bytes (hex): {:02x?}", &message[..100.min(message.len())]));
            freenet_stdlib::log::info(&format!("Message length: {} bytes", message.len()));
            freenet_stdlib::log::info(&format!("Signature bytes (hex): {:02x?}", metadata.signature.to_bytes()));
            freenet_stdlib::log::info(&format!("Verifying key bytes (hex): {:02x?}", verifying_key.to_bytes()));
            freenet_stdlib::log::info(&format!("Verifying key (base58): {}", bs58::encode(verifying_key.to_bytes()).into_string()));
        }

        // Verify signature
        let verify_result = verifying_key.verify_strict(&message, &metadata.signature);
        
        if let Err(e) = verify_result {
            #[cfg(not(test))]
            {
                freenet_stdlib::log::info("Signature verification failed");
                freenet_stdlib::log::info(&format!("Error details: {}", e));
                freenet_stdlib::log::info(&format!("Expected verifying key (hex): {:02x?}", verifying_key.to_bytes()));
                freenet_stdlib::log::info(&format!("Message length: {} bytes", message.len()));
                freenet_stdlib::log::info(&format!("First 32 message bytes (hex): {:02x?}", &message[..32.min(message.len())]));
                freenet_stdlib::log::info(&format!("Last 32 message bytes (hex): {:02x?}", &message[message.len().saturating_sub(32)..]));
            }
            return Err(ContractError::Other(format!("Signature verification failed: {}", e)));
        }

        #[cfg(not(test))]
        freenet_stdlib::log::info("Validation successful");

        Ok(ValidateResult::Valid)
    }

    fn update_state(
        _parameters: Parameters<'static>,
        state: State<'static>,
        data: Vec<UpdateData<'static>>,
    ) -> Result<UpdateModification<'static>, ContractError> {
        #[cfg(not(test))]
        {
            freenet_stdlib::log::info("Starting update_state");
            freenet_stdlib::log::info(&format!("Current state length: {}", state.as_ref().len()));
            freenet_stdlib::log::info(&format!("Update data count: {}", data.len()));
        }
        // Get current version
        let current_version = if state.as_ref().is_empty() {
            0
        } else {
            let mut cursor = std::io::Cursor::new(state.as_ref());
            
            // Read metadata length
            let metadata_size = cursor
                .read_u64::<BigEndian>()
                .map_err(|e| ContractError::Other(format!("Failed to read metadata size: {}", e)))?;
                
            // Read metadata bytes
            let mut metadata_bytes = vec![0; metadata_size as usize];
            cursor
                .read_exact(&mut metadata_bytes)
                .map_err(|e| ContractError::Other(format!("Failed to read metadata: {}", e)))?;

            // Parse metadata as CBOR
            let metadata: WebContainerMetadata = from_reader(&metadata_bytes[..])
                .map_err(|e| ContractError::Deser(e.to_string()))?;
            metadata.version
        };

        // Process update data
        if let Some(UpdateData::State(new_state)) = data.into_iter().next() {
            // Verify new state has higher version
            let mut cursor = std::io::Cursor::new(new_state.as_ref());
            
            // Read metadata length
            let metadata_size = cursor
                .read_u64::<BigEndian>()
                .map_err(|e| ContractError::Other(format!("Failed to read metadata size: {}", e)))?;
                
            // Read metadata bytes
            let mut metadata_bytes = vec![0; metadata_size as usize];
            cursor
                .read_exact(&mut metadata_bytes)
                .map_err(|e| ContractError::Other(format!("Failed to read metadata: {}", e)))?;

            // Parse metadata as CBOR
            let metadata: WebContainerMetadata = from_reader(&metadata_bytes[..])
                .map_err(|e| ContractError::Deser(e.to_string()))?;

            if metadata.version <= current_version {
                return Err(ContractError::InvalidUpdate);
            }

            #[cfg(not(test))]
            freenet_stdlib::log::info(&format!("Update successful: version {} -> {}", current_version, metadata.version));

            Ok(UpdateModification::valid(new_state))
        } else {
            #[cfg(not(test))]
            freenet_stdlib::log::info("Update failed: no valid update data provided");

            Err(ContractError::InvalidUpdate)
        }
    }

    fn summarize_state(
        _parameters: Parameters<'static>,
        state: State<'static>,
    ) -> Result<StateSummary<'static>, ContractError> {
        #[cfg(not(test))]
        {
            freenet_stdlib::log::info("Starting summarize_state");
            freenet_stdlib::log::info(&format!("State length: {}", state.as_ref().len()));
        }
        if state.as_ref().is_empty() {
            return Ok(StateSummary::from(Vec::new()));
        }

        // Parse WebApp format to get metadata
        let mut cursor = std::io::Cursor::new(state.as_ref());
        
        // Read metadata length
        let metadata_size = cursor
            .read_u64::<BigEndian>()
            .map_err(|e| ContractError::Other(format!("Failed to read metadata size: {}", e)))?;
            
        #[cfg(not(test))]
        freenet_stdlib::log::info(&format!("Metadata size from state: {} bytes", metadata_size));
            
        // Read metadata bytes
        let mut metadata_bytes = vec![0; metadata_size as usize];
        cursor
            .read_exact(&mut metadata_bytes)
            .map_err(|e| ContractError::Other(format!("Failed to read metadata: {}", e)))?;

        #[cfg(not(test))]
        if metadata_bytes.len() > 64 {
            freenet_stdlib::log::info(&format!("First 32 metadata bytes (hex): {:02x?}", &metadata_bytes[..32]));
            freenet_stdlib::log::info(&format!("Last 32 metadata bytes (hex): {:02x?}", &metadata_bytes[metadata_bytes.len()-32..]));
        }

        // Parse metadata as CBOR
        let metadata: WebContainerMetadata = from_reader(&metadata_bytes[..])
            .map_err(|e| ContractError::Deser(e.to_string()))?;

        let mut summary = Vec::new();
        into_writer(&metadata.version, &mut summary)
            .map_err(|e| ContractError::Deser(e.to_string()))?;

        #[cfg(not(test))]
        freenet_stdlib::log::info(&format!("Generated summary for version {}", metadata.version));

        Ok(StateSummary::from(summary))
    }

    fn get_state_delta(
        _parameters: Parameters<'static>,
        state: State<'static>,
        summary: StateSummary<'static>,
    ) -> Result<StateDelta<'static>, ContractError> {
        #[cfg(not(test))]
        {
            freenet_stdlib::log::info("Starting get_state_delta");
            freenet_stdlib::log::info(&format!("State length: {}", state.as_ref().len()));
            freenet_stdlib::log::info(&format!("Summary length: {}", summary.as_ref().len()));
        }
        if state.as_ref().is_empty() {
            return Ok(StateDelta::from(Vec::new()));
        }

        // Compare versions
        let current_version = {
            let mut cursor = std::io::Cursor::new(state.as_ref());
            
            // Read metadata length
            let metadata_size = cursor
                .read_u64::<BigEndian>()
                .map_err(|e| ContractError::Other(format!("Failed to read metadata size: {}", e)))?;
                
            // Read metadata bytes
            let mut metadata_bytes = vec![0; metadata_size as usize];
            cursor
                .read_exact(&mut metadata_bytes)
                .map_err(|e| ContractError::Other(format!("Failed to read metadata: {}", e)))?;

            // Parse metadata as CBOR
            let metadata: WebContainerMetadata = from_reader(&metadata_bytes[..])
                .map_err(|e| ContractError::Deser(e.to_string()))?;
            metadata.version
        };

        let summary_version: u32 = from_reader(summary.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;

        if current_version > summary_version {
            // Return full state if version is newer
            #[cfg(not(test))]
            freenet_stdlib::log::info(&format!("Generated delta: version {} -> {}", summary_version, current_version));

            Ok(StateDelta::from(state.as_ref().to_vec()))
        } else {
            #[cfg(not(test))]
            freenet_stdlib::log::info("No delta needed - summary version matches or is newer");

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
        let signing_key = SigningKey::from_bytes(&SigningKey::generate(&mut OsRng).to_bytes());
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

        // Serialize metadata to CBOR
        let mut metadata_bytes = Vec::new();
        into_writer(&metadata, &mut metadata_bytes).unwrap();

        // Create final state in WebApp format:
        // [metadata_length: u64][metadata: bytes][web_length: u64][web: bytes]
        let mut state = Vec::new();
        // Write metadata length as u64 BE
        state.extend_from_slice(&(metadata_bytes.len() as u64).to_be_bytes());
        // Write metadata
        state.extend_from_slice(&metadata_bytes);
        // Write webapp length as u64 BE
        state.extend_from_slice(&(compressed_webapp.len() as u64).to_be_bytes());
        // Write webapp
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
