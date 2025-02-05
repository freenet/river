use crate::room_state::member::MemberId;
use crate::room_state::ChatRoomParametersV1;
use crate::util::truncated_base64;
use crate::ChatRoomStateV1;
use ed25519_dalek::{Signature, SignatureError, Signer, SigningKey, Verifier, VerifyingKey};
use freenet_scaffold::util::{fast_hash, FastHash};
use freenet_scaffold::ComposableState;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct AuthorizedConfigurationV1 {
    pub configuration: Configuration,
    pub signature: Signature,
}

impl ComposableState for AuthorizedConfigurationV1 {
    type ParentState = ChatRoomStateV1;
    type Summary = u32;
    type Delta = AuthorizedConfigurationV1;
    type Parameters = ChatRoomParametersV1;

    fn verify(
        &self,
        _parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
    ) -> Result<(), String> {
        self.verify_signature(&parameters.owner)
            .map_err(|e| format!("Invalid signature: {}", e))
    }

    fn summarize(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
    ) -> Self::Summary {
        self.configuration.configuration_version
    }

    fn delta(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
        old_version: &Self::Summary,
    ) -> Option<Self::Delta> {
        if self.configuration.configuration_version > *old_version {
            Some(self.clone())
        } else {
            None
        }
    }

    fn apply_delta(
        &mut self,
        _parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
        delta: &Option<Self::Delta>,
    ) -> Result<(), String> {
        if let Some(delta) = delta {
            // Verify the delta's signature
            delta
                .verify_signature(&parameters.owner)
                .map_err(|e| format!("Invalid signature: {}", e))?;

            // Check if the new version is greater than the current version
            if delta.configuration.configuration_version <= self.configuration.configuration_version {
                return Err(
                    "New configuration version must be greater than the current version".to_string(),
                );
            }

            // Verify that the owner_member_id hasn't changed
            if delta.configuration.owner_member_id != self.configuration.owner_member_id {
                return Err("Cannot change the owner_member_id".to_string());
            }

            // Verify that the new configuration is valid
            if delta.configuration.max_recent_messages == 0
                || delta.configuration.max_user_bans == 0
                || delta.configuration.max_message_size == 0
                || delta.configuration.max_nickname_size == 0
                || delta.configuration.max_members == 0
            {
                return Err("Invalid configuration values".to_string());
            }

            // If all checks pass, apply the delta
            self.configuration = delta.configuration.clone();
            self.signature = delta.signature;
        }

        Ok(())
    }
}

impl AuthorizedConfigurationV1 {
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

    pub fn verify_signature(
        &self,
        owner_verifying_key: &VerifyingKey,
    ) -> Result<(), SignatureError> {
        let mut serialized_config = Vec::new();
        ciborium::ser::into_writer(&self.configuration, &mut serialized_config)
            .expect("Serialization should not fail");
        owner_verifying_key.verify(&serialized_config, &self.signature)
    }

    pub fn id(&self) -> FastHash {
        fast_hash(&self.signature.to_bytes())
    }
}

impl Default for AuthorizedConfigurationV1 {
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
            name: "Default Room Name".to_string(),
            max_recent_messages: 100,
            max_user_bans: 10,
            max_message_size: 1000,
            max_nickname_size: 50,
            max_members: 200,
        }
    }
}

impl fmt::Debug for AuthorizedConfigurationV1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizedConfiguration")
            .field("configuration", &self.configuration)
            .field(
                "signature",
                &format_args!("{}", truncated_base64(self.signature.to_bytes())),
            )
            .finish()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Configuration {
    pub owner_member_id: MemberId,
    pub configuration_version: u32,
    pub name: String,
    pub max_recent_messages: usize,
    pub max_user_bans: usize,
    pub max_message_size: usize,
    pub max_nickname_size: usize,
    pub max_members: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

    #[test]
    fn test_verify() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let configuration = Configuration::default();
        let authorized_configuration =
            AuthorizedConfigurationV1::new(configuration.clone(), &owner_signing_key);

        assert!(authorized_configuration
            .verify_signature(&owner_verifying_key)
            .is_ok());

        let parent_state = ChatRoomStateV1 {
            configuration: authorized_configuration.clone(),
            ..ChatRoomStateV1::default()
        };
        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        assert!(authorized_configuration
            .verify(&parent_state, &parameters)
            .is_ok());
    }

    #[test]
    fn test_verify_fail() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let configuration = Configuration::default();
        let authorized_configuration =
            AuthorizedConfigurationV1::new(configuration.clone(), &owner_signing_key);

        let wrong_owner_signing_key = SigningKey::generate(&mut OsRng);
        let wrong_owner_verifying_key = VerifyingKey::from(&wrong_owner_signing_key);

        assert!(authorized_configuration
            .verify_signature(&wrong_owner_verifying_key)
            .is_err());

        let parent_state = ChatRoomStateV1 {
            configuration: authorized_configuration.clone(),
            ..ChatRoomStateV1::default()
        };
        let parameters = ChatRoomParametersV1 {
            owner: wrong_owner_verifying_key,
        };

        assert!(authorized_configuration
            .verify(&parent_state, &parameters)
            .is_err());
    }

    #[test]
    fn test_summarize() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let configuration = Configuration::default();
        let authorized_configuration =
            AuthorizedConfigurationV1::new(configuration.clone(), &owner_signing_key);

        let mut parent_state = ChatRoomStateV1::default();
        parent_state.configuration = authorized_configuration.clone();
        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        assert_eq!(
            authorized_configuration.summarize(&parent_state, &parameters),
            configuration.configuration_version
        );
    }

    #[test]
    fn test_delta_new_version() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let configuration = Configuration::default();
        let authorized_configuration =
            AuthorizedConfigurationV1::new(configuration.clone(), &owner_signing_key);

        let mut parent_state = ChatRoomStateV1::default();
        parent_state.configuration = authorized_configuration.clone();
        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        let new_configuration = Configuration {
            configuration_version: 2,
            ..configuration.clone()
        };
        let new_authorized_configuration =
            AuthorizedConfigurationV1::new(new_configuration.clone(), &owner_signing_key);

        assert_eq!(
            new_authorized_configuration.delta(&parent_state, &parameters, &1),
            Some(new_authorized_configuration)
        );
    }

    #[test]
    fn test_delta_older_version() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        
        // Create an older configuration (version 1)
        let old_configuration = Configuration {
            configuration_version: 1,
            ..Configuration::default()
        };
        let old_authorized_configuration =
            AuthorizedConfigurationV1::new(old_configuration.clone(), &owner_signing_key);

        let mut parent_state = ChatRoomStateV1::default();
        parent_state.configuration = old_authorized_configuration.clone();
        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        // Test against a newer version (2)
        // The delta should return None since our configuration is older
        assert_eq!(
            old_authorized_configuration.delta(&parent_state, &parameters, &2),
            None
        );
    }

    #[test]
    fn test_apply_delta_should_apply() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let configuration = Configuration::default();
        let mut authorized_configuration =
            AuthorizedConfigurationV1::new(configuration.clone(), &owner_signing_key);

        let mut parent_state = ChatRoomStateV1::default();
        parent_state.configuration = authorized_configuration.clone();
        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        let new_configuration = Configuration {
            configuration_version: 2,
            ..configuration.clone()
        };
        let new_authorized_configuration =
            AuthorizedConfigurationV1::new(new_configuration.clone(), &owner_signing_key);

        authorized_configuration
            .apply_delta(
                &parent_state,
                &parameters,
                &Some(new_authorized_configuration.clone()),
            )
            .unwrap();

        assert_eq!(authorized_configuration, new_authorized_configuration);
    }

    #[test]
    fn test_apply_delta_old_version() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let configuration = Configuration::default();
        let mut authorized_configuration =
            AuthorizedConfigurationV1::new(configuration.clone(), &owner_signing_key);

        let orig_authorized_configuration = authorized_configuration.clone();

        let mut parent_state = ChatRoomStateV1::default();
        parent_state.configuration = authorized_configuration.clone();
        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        let new_configuration = Configuration {
            configuration_version: 0,
            ..configuration.clone()
        };
        let new_authorized_configuration =
            AuthorizedConfigurationV1::new(new_configuration.clone(), &owner_signing_key);

        let result = authorized_configuration.apply_delta(
            &parent_state,
            &parameters,
            &Some(new_authorized_configuration),
        );

        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            "New configuration version must be greater than the current version"
        );
        assert_eq!(authorized_configuration, orig_authorized_configuration);
    }

    #[test]
    fn test_apply_delta_change_owner() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let configuration = Configuration {
            owner_member_id: MemberId(FastHash(1)),
            ..Configuration::default()
        };
        let mut authorized_configuration =
            AuthorizedConfigurationV1::new(configuration.clone(), &owner_signing_key);

        let mut parent_state = ChatRoomStateV1::default();
        parent_state.configuration = authorized_configuration.clone();
        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        let mut new_configuration = configuration.clone();
        new_configuration.configuration_version += 1;
        new_configuration.owner_member_id = MemberId(FastHash(2));
        let new_authorized_configuration =
            AuthorizedConfigurationV1::new(new_configuration, &owner_signing_key);

        let result = authorized_configuration.apply_delta(
            &parent_state,
            &parameters,
            &Some(new_authorized_configuration),
        );

        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Cannot change the owner_member_id");
    }

    #[test]
    fn test_apply_delta_invalid_values() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let configuration = Configuration::default();
        let mut authorized_configuration =
            AuthorizedConfigurationV1::new(configuration.clone(), &owner_signing_key);

        let mut parent_state = ChatRoomStateV1::default();
        parent_state.configuration = authorized_configuration.clone();
        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        let mut new_configuration = configuration.clone();
        new_configuration.configuration_version += 1;
        new_configuration.max_recent_messages = 0;
        let new_authorized_configuration =
            AuthorizedConfigurationV1::new(new_configuration, &owner_signing_key);

        let result = authorized_configuration.apply_delta(
            &parent_state,
            &parameters,
            &Some(new_authorized_configuration),
        );

        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Invalid configuration values");
    }
}
