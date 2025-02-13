use ed25519_dalek::{SigningKey, Signer};
use freenet_stdlib::prelude::*;
use rand::rngs::OsRng;
use tar::Builder;
use web_container_contract::WebContainerContract;
use river_common::web_container::WebContainerMetadata;

fn create_test_webapp() -> Vec<u8> {
    let mut builder = Builder::new(Vec::new());
    let content = b"test content";
    let mut header = tar::Header::new_gnu();
    header.set_size(content.len() as u64);
    builder.append_data(
        &mut header,
        &std::path::Path::new("index.html"),
        content.as_ref(),
    )
    .unwrap();
    builder.into_inner().unwrap()
}

#[test]
fn test_tool_and_contract_compatibility() {
    // Generate a keypair like the tool does
    let signing_key = SigningKey::generate(&mut OsRng);
    let verifying_key = signing_key.verifying_key();

    // Create a test webapp archive
    let webapp_bytes = create_test_webapp();
    
    // Create message to sign (version + webapp) exactly as tool does
    let version: u32 = 1;
    let mut message = version.to_be_bytes().to_vec();
    message.extend_from_slice(&webapp_bytes);
    
    // Sign the message
    let signature = signing_key.sign(&message);
    
    // Create metadata struct
    let metadata = WebContainerMetadata {
        version,
        signature,
    };

    // Serialize metadata to CBOR
    let mut metadata_bytes = Vec::new();
    ciborium::ser::into_writer(&metadata, &mut metadata_bytes).unwrap();

    // Create final state in WebApp format:
    // [metadata_length: u64][metadata: bytes][web_length: u64][web: bytes]
    let mut state = Vec::new();
    state.extend_from_slice(&(metadata_bytes.len() as u64).to_be_bytes());
    state.extend_from_slice(&metadata_bytes);
    state.extend_from_slice(&(webapp_bytes.len() as u64).to_be_bytes());
    state.extend_from_slice(&webapp_bytes);

    // Verify using contract code
    let result = WebContainerContract::validate_state(
        Parameters::from(verifying_key.to_bytes().to_vec()),
        State::from(state),
        RelatedContracts::default(),
    );

    assert!(matches!(result, Ok(ValidateResult::Valid)));
}

#[test]
fn test_modified_webapp_fails_verification() {
    // Generate a keypair
    let signing_key = SigningKey::generate(&mut OsRng);
    let verifying_key = signing_key.verifying_key();

    // Create and sign original webapp
    let webapp_bytes = create_test_webapp();
    let version: u32 = 1;
    let mut message = version.to_be_bytes().to_vec();
    message.extend_from_slice(&webapp_bytes);
    let signature = signing_key.sign(&message);
    
    let metadata = WebContainerMetadata {
        version,
        signature,
    };

    let mut metadata_bytes = Vec::new();
    ciborium::ser::into_writer(&metadata, &mut metadata_bytes).unwrap();

    // Create state but with modified webapp content
    let mut modified_webapp = webapp_bytes.clone();
    modified_webapp[0] ^= 1; // Flip one bit

    let mut state = Vec::new();
    state.extend_from_slice(&(metadata_bytes.len() as u64).to_be_bytes());
    state.extend_from_slice(&metadata_bytes);
    state.extend_from_slice(&(modified_webapp.len() as u64).to_be_bytes());
    state.extend_from_slice(&modified_webapp);

    // This should fail verification
    let result = WebContainerContract::validate_state(
        Parameters::from(verifying_key.to_bytes().to_vec()),
        State::from(state),
        RelatedContracts::default(),
    );

    assert!(matches!(result, Err(ContractError::Other(_))));
}
