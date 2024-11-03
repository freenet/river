use crate::room_state::member::MemberId;
use crate::room_state::ChatRoomParametersV1;
use crate::util::{sign_struct, truncated_base64, verify_struct};
use crate::ChatRoomStateV1;
use blake3::Hash;
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use freenet_scaffold::ComposableState;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct OptionalUpgradeV1(pub Option<AuthorizedUpgradeV1>);

impl Default for OptionalUpgradeV1 {
    fn default() -> Self {
        OptionalUpgradeV1(None)
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct AuthorizedUpgradeV1 {
    pub upgrade: UpgradeV1,
    pub signature: Signature,
}

impl ComposableState for OptionalUpgradeV1 {
    type ParentState = ChatRoomStateV1;
    type Summary = Option<u8>;
    type Delta = AuthorizedUpgradeV1;
    type Parameters = ChatRoomParametersV1;

    fn verify(
        &self,
        _parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
    ) -> Result<(), String> {
        if let Some(upgrade) = &self.0 {
            upgrade
                .validate(&parameters.owner)
                .map_err(|e| format!("Invalid signature: {}", e))
        } else {
            Ok(())
        }
    }

    fn summarize(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
    ) -> Self::Summary {
        self.0.as_ref().map(|u| u.upgrade.version)
    }

    fn delta(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
        old_state_summary: &Self::Summary,
    ) -> Option<Self::Delta> {
        match &self.0 {
            Some(upgrade) => {
                // If the upgrade has a higher version than the old room_state summary or of the old summary is None
                // then return the upgrade as a delta
                if old_state_summary
                    .map_or(true, |old_version| upgrade.upgrade.version > old_version)
                {
                    Some(upgrade.clone())
                } else {
                    None
                }
            }
            None => None,
        }
    }

    fn apply_delta(
        &mut self,
        _parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
        delta: &Option<Self::Delta>,
    ) -> Result<(), String> {
        if let Some(delta) = delta {
            // Verify the delta before applying it
            delta
                .validate(&parameters.owner)
                .map_err(|e| format!("Invalid upgrade signature: {}", e))?;

            *self = OptionalUpgradeV1(Some(delta.clone()));
        }
        Ok(())
    }
}

impl AuthorizedUpgradeV1 {
    pub fn new(upgrade: UpgradeV1, signing_key: &SigningKey) -> Self {
        Self {
            upgrade: upgrade.clone(),
            signature: sign_struct(&upgrade, signing_key),
        }
    }

    pub fn validate(
        &self,
        verifying_key: &VerifyingKey,
    ) -> Result<(), ed25519_dalek::SignatureError> {
        verify_struct(&self.upgrade, &self.signature, &verifying_key)
    }
}

impl fmt::Debug for AuthorizedUpgradeV1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizedUpgrade")
            .field("upgrade", &self.upgrade)
            .field(
                "signature",
                &format_args!("{}", truncated_base64(self.signature.to_bytes())),
            )
            .finish()
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct UpgradeV1 {
    pub owner_member_id: MemberId,
    pub version: u8,
    pub new_chatroom_address: Hash,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room_state::member::MemberId;
    use ed25519_dalek::SigningKey;
    use freenet_scaffold::util::FastHash;
    use rand::rngs::OsRng;

    fn create_test_upgrade(owner_id: MemberId) -> UpgradeV1 {
        UpgradeV1 {
            owner_member_id: owner_id,
            version: 1,
            new_chatroom_address: Hash::from([0; 32]),
        }
    }

    #[test]
    fn test_authorized_upgrade_new_and_validate() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        let owner_id = MemberId(FastHash(0));

        let upgrade = create_test_upgrade(owner_id);
        let authorized_upgrade = AuthorizedUpgradeV1::new(upgrade.clone(), &signing_key);

        assert_eq!(authorized_upgrade.upgrade, upgrade);
        assert!(authorized_upgrade.validate(&verifying_key).is_ok());

        // Test with wrong key
        let wrong_key = SigningKey::generate(&mut OsRng).verifying_key();
        assert!(authorized_upgrade.validate(&wrong_key).is_err());
    }

    #[test]
    fn test_optional_upgrade_verify() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = owner_signing_key.verifying_key();
        let owner_id = MemberId::from(&owner_verifying_key);

        let upgrade = create_test_upgrade(owner_id);
        let authorized_upgrade = AuthorizedUpgradeV1::new(upgrade, &owner_signing_key);

        let optional_upgrade = OptionalUpgradeV1(Some(authorized_upgrade));

        let parent_state = ChatRoomStateV1::default();
        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        // Verify that a valid upgrade passes verification
        assert!(
            optional_upgrade.verify(&parent_state, &parameters).is_ok(),
            "Valid upgrade should pass verification"
        );

        // Test with invalid signature
        let mut invalid_upgrade = optional_upgrade.clone();
        if let Some(ref mut au) = invalid_upgrade.0 {
            au.signature = Signature::from_bytes(&[0; 64]); // Replace with an invalid signature
        }
        assert!(
            invalid_upgrade.verify(&parent_state, &parameters).is_err(),
            "Upgrade with invalid signature should fail verification"
        );

        // Test with None
        let none_upgrade = OptionalUpgradeV1(None);
        assert!(
            none_upgrade.verify(&parent_state, &parameters).is_ok(),
            "None upgrade should pass verification"
        );
    }

    #[test]
    fn test_optional_upgrade_summarize() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let owner_id = MemberId(FastHash(0));

        let upgrade = create_test_upgrade(owner_id);
        let authorized_upgrade = AuthorizedUpgradeV1::new(upgrade, &signing_key);

        let optional_upgrade = OptionalUpgradeV1(Some(authorized_upgrade));

        let parent_state = ChatRoomStateV1::default();
        let parameters = ChatRoomParametersV1 {
            owner: signing_key.verifying_key(),
        };

        let summary = optional_upgrade.summarize(&parent_state, &parameters);
        assert_eq!(summary, Some(1));

        let none_upgrade = OptionalUpgradeV1(None);
        let none_summary = none_upgrade.summarize(&parent_state, &parameters);
        assert_eq!(none_summary, None);
    }

    #[test]
    fn test_optional_upgrade_delta() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let owner_id = MemberId(FastHash(0));

        let upgrade = create_test_upgrade(owner_id);
        let authorized_upgrade = AuthorizedUpgradeV1::new(upgrade, &signing_key);

        let optional_upgrade = OptionalUpgradeV1(Some(authorized_upgrade.clone()));

        let parent_state = ChatRoomStateV1::default();
        let parameters = ChatRoomParametersV1 {
            owner: signing_key.verifying_key(),
        };

        let old_summary = None;
        let delta = optional_upgrade.delta(&parent_state, &parameters, &old_summary);

        assert_eq!(delta, Some(authorized_upgrade));

        let none_upgrade = OptionalUpgradeV1(None);
        let none_delta = none_upgrade.delta(&parent_state, &parameters, &old_summary);
        assert_eq!(none_delta, None);
    }

    #[test]
    fn test_optional_upgrade_apply_delta() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let owner_id = MemberId(FastHash(0));

        let upgrade = create_test_upgrade(owner_id);
        let authorized_upgrade = AuthorizedUpgradeV1::new(upgrade, &signing_key);

        let mut optional_upgrade = OptionalUpgradeV1(None);

        let parent_state = ChatRoomStateV1::default();
        let parameters = ChatRoomParametersV1 {
            owner: signing_key.verifying_key(),
        };

        let delta = authorized_upgrade.clone();
        assert!(optional_upgrade
            .apply_delta(&parent_state, &parameters, &Some(delta.clone()))
            .is_ok());
        assert_eq!(optional_upgrade, OptionalUpgradeV1(Some(delta)));
    }
}
