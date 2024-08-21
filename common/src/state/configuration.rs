use crate::util::{truncated_base64, fast_hash};
use ed25519_dalek::{Signature, SigningKey, Signer, Verifier, VerifyingKey, SignatureError};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct AuthorizedConfiguration {
    pub configuration: Configuration,
    pub signature: Signature,
}

impl AuthorizedConfiguration {
    pub fn new(configuration: Configuration, signing_key: &SigningKey) -> Self {
        let mut serialized_config = Vec::new();
        ciborium::ser::into_writer(&configuration, &mut serialized_config)
            .expect("Serialization should not fail");
        let signature = signing_key.sign(&serialized_config);
        
        Self {
            configuration,
            signature,
        }
    }

    pub fn validate(&self, owner_verifying_key: &VerifyingKey) -> Result<(), SignatureError> {
        let mut serialized_config = Vec::new();
        ciborium::ser::into_writer(&self.configuration, &mut serialized_config)
            .expect("Serialization should not fail");
        owner_verifying_key.verify(&serialized_config, &self.signature)
    }
    
    pub fn id(&self) -> i32 {
        fast_hash(&self.signature.to_bytes())
    }
}

impl Default for AuthorizedConfiguration {
    fn default() -> Self {
        let default_config = Configuration::default();
        let default_key = SigningKey::from_bytes(&[0; 32]);
        Self::new(default_config, &default_key)
    }
}

impl Default for Configuration {
    fn default() -> Self {
        Configuration {
            configuration_version: 1,
            name: "Default Room".to_string(),
            max_recent_messages: 100,
            max_user_bans: 10,
            max_message_size: 1000,
            max_nickname_size: 50,
        }
    }
}

impl fmt::Debug for AuthorizedConfiguration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizedConfiguration")
            .field("configuration", &self.configuration)
            .field("signature", &format_args!("{}", truncated_base64(self.signature.to_bytes())))
            .finish()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Configuration {
    pub room_fhash : i32, // fast hash of room owner verifying key
    pub configuration_version: u32,
    pub name: String,
    pub max_recent_messages: u32,
    pub max_user_bans: u32,
    pub max_message_size: usize,
    pub max_nickname_size: usize,
}
