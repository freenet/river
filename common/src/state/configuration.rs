use crate::util::{truncated_base64};
use ed25519_dalek::{Signature, SigningKey, Signer, Verifier, VerifyingKey, SignatureError};
use serde::{Deserialize, Serialize};
use std::fmt;
use freenet_scaffold::ComposableState;
use freenet_scaffold::util::{fast_hash, FastHash};
use crate::{ChatRoomState};
use crate::state::ChatRoomParameters;
use crate::state::member::MemberId;

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct AuthorizedConfiguration {
    pub configuration: Configuration,
    pub signature: Signature,
}

impl ComposableState for AuthorizedConfiguration {
    type ParentState = ChatRoomState;
    type Summary = u32; 
    type Delta = Option<AuthorizedConfiguration>;
    type Parameters = ChatRoomParameters;

    fn verify(&self, _parent_state: &Self::ParentState, parameters: &Self::Parameters) -> Result<(), String> {
        self.verify_signature(&parameters.owner).map_err(|e| format!("Invalid signature: {}", e))
    }

    fn summarize(&self, _parent_state: &Self::ParentState, _parameters: &Self::Parameters) -> Self::Summary {
        self.configuration.configuration_version
    }

    fn delta(&self, _parent_state: &Self::ParentState, _parameters: &Self::Parameters, old_version: &Self::Summary) -> Self::Delta {
        if self.configuration.configuration_version > *old_version {
            Some(self.clone())
        } else {
            None
        }
    }

    fn apply_delta(&self, _parent_state: &Self::ParentState, _parameters: &Self::Parameters, delta: &Self::Delta) -> Self {
        match delta {
            None => self.clone(),
            Some(cfg) if cfg.configuration.configuration_version > self.configuration.configuration_version => cfg.clone(),
            _ => self.clone(), // Disregard the delta unless it's newer
        }
    }
}

impl AuthorizedConfiguration {
    pub fn new(configuration: Configuration, owner_signing_key: &SigningKey) -> Self {
        let mut serialized_config = Vec::new();
        ciborium::ser::into_writer(&configuration, &mut serialized_config)
            .expect("Serialization should not fail");
        let signature = owner_signing_key.sign(&serialized_config);
        
        Self {
            configuration,
            signature,
        }
    }

    pub fn verify_signature(&self, owner_verifying_key: &VerifyingKey) -> Result<(), SignatureError> {
        let mut serialized_config = Vec::new();
        ciborium::ser::into_writer(&self.configuration, &mut serialized_config)
            .expect("Serialization should not fail");
        owner_verifying_key.verify(&serialized_config, &self.signature)
    }
    
    pub fn id(&self) -> FastHash {
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
            owner_member_id: MemberId(FastHash(0)), // Default value, should be overwritten
            configuration_version: 1,
            name: "Default Room".to_string(),
            max_recent_messages: 100,
            max_user_bans: 10,
            max_message_size: 1000,
            max_nickname_size: 50,
            max_members: 200,
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
    pub owner_member_id : MemberId,
    pub configuration_version: u32,
    pub name: String,
    pub max_recent_messages: usize,
    pub max_user_bans: u32,
    pub max_message_size: usize,
    pub max_nickname_size: usize,
    pub max_members: usize,
}

#[cfg(test)]
mod tests {
    use rand::rngs::OsRng;
    use super::*;
    
    #[test]
    fn test_verify() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let configuration = Configuration::default();
        let authorized_configuration = AuthorizedConfiguration::new(configuration.clone(), &owner_signing_key);
        
        assert!(authorized_configuration.verify_signature(&owner_verifying_key).is_ok());
        
        let mut parent_state = ChatRoomState::default();
        parent_state.configuration = authorized_configuration.clone();
        let parameters = ChatRoomParameters {
            owner: owner_verifying_key,
        };
        
        assert!(authorized_configuration.verify(&parent_state, &parameters).is_ok());
    }
    
    #[test]
    fn test_verify_fail() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let configuration = Configuration::default();
        let authorized_configuration = AuthorizedConfiguration::new(configuration.clone(), &owner_signing_key);
        
        let mut wrong_owner_signing_key = SigningKey::generate(&mut OsRng);
        let wrong_owner_verifying_key = VerifyingKey::from(&wrong_owner_signing_key);
        
        assert!(authorized_configuration.verify_signature(&wrong_owner_verifying_key).is_err());
        
        let mut parent_state = ChatRoomState::default();
        parent_state.configuration = authorized_configuration.clone();
        let parameters = ChatRoomParameters {
            owner: wrong_owner_verifying_key,
        };
        
        assert!(authorized_configuration.verify(&parent_state, &parameters).is_err());
    }
    
    #[test]
    fn test_summarize() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let configuration = Configuration::default();
        let authorized_configuration = AuthorizedConfiguration::new(configuration.clone(), &owner_signing_key);
        
        let mut parent_state = ChatRoomState::default();
        parent_state.configuration = authorized_configuration.clone();
        let parameters = ChatRoomParameters {
            owner: owner_verifying_key,
        };
        
        assert_eq!(authorized_configuration.summarize(&parent_state, &parameters), configuration.configuration_version);
    }
    
    #[test]
    fn test_delta_new_version() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let configuration = Configuration::default();
        let authorized_configuration = AuthorizedConfiguration::new(configuration.clone(), &owner_signing_key);
        
        let mut parent_state = ChatRoomState::default();
        parent_state.configuration = authorized_configuration.clone();
        let parameters = ChatRoomParameters {
            owner: owner_verifying_key,
        };
        
        let new_configuration = Configuration {
            configuration_version: 2,
            ..configuration.clone()
        };
        let new_authorized_configuration = AuthorizedConfiguration::new(new_configuration.clone(), &owner_signing_key);
        
        assert_eq!(new_authorized_configuration.delta(&parent_state, &parameters, &1), Some(new_authorized_configuration));
    }
    
    #[test]
    fn test_delta_older_version() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let configuration = Configuration::default();
        let authorized_configuration = AuthorizedConfiguration::new(configuration.clone(), &owner_signing_key);
        
        let mut parent_state = ChatRoomState::default();
        parent_state.configuration = authorized_configuration.clone();
        let parameters = ChatRoomParameters {
            owner: owner_verifying_key,
        };
        
        let new_configuration = Configuration {
            configuration_version: 0,
            ..configuration.clone()
        };
        let new_authorized_configuration = AuthorizedConfiguration::new(new_configuration.clone(), &owner_signing_key);
        
        assert_eq!(authorized_configuration.delta(&parent_state, &parameters, &1), None);
    }
    
    #[test]
    fn test_apply_delta_should_apply() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let configuration = Configuration::default();
        let authorized_configuration = AuthorizedConfiguration::new(configuration.clone(), &owner_signing_key);
        
        let mut parent_state = ChatRoomState::default();
        parent_state.configuration = authorized_configuration.clone();
        let parameters = ChatRoomParameters {
            owner: owner_verifying_key,
        };
        
        let new_configuration = Configuration {
            configuration_version: 2,
            ..configuration.clone()
        };
        let new_authorized_configuration = AuthorizedConfiguration::new(new_configuration.clone(), &owner_signing_key);
        
        assert_eq!(authorized_configuration.apply_delta(&parent_state, &parameters, &Some(new_authorized_configuration.clone())), new_authorized_configuration);
    }
    
    #[test]
    fn test_apply_delta_none() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let configuration = Configuration::default();
        let authorized_configuration = AuthorizedConfiguration::new(configuration.clone(), &owner_signing_key);
        
        let mut parent_state = ChatRoomState::default();
        parent_state.configuration = authorized_configuration.clone();
        let parameters = ChatRoomParameters {
            owner: owner_verifying_key,
        };
        
        assert_eq!(authorized_configuration.apply_delta(&parent_state, &parameters, &None), authorized_configuration);
    }
    
    #[test]
    fn test_apply_delta_old_version() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let configuration = Configuration::default();
        let authorized_configuration = AuthorizedConfiguration::new(configuration.clone(), &owner_signing_key);
        
        let mut parent_state = ChatRoomState::default();
        parent_state.configuration = authorized_configuration.clone();
        let parameters = ChatRoomParameters {
            owner: owner_verifying_key,
        };
        
        let new_configuration = Configuration {
            configuration_version: 0,
            ..configuration.clone()
        };
        let new_authorized_configuration = AuthorizedConfiguration::new(new_configuration.clone(), &owner_signing_key);
        
        assert_eq!(authorized_configuration.apply_delta(&parent_state, &parameters, &Some(new_authorized_configuration)), authorized_configuration);
    }
}