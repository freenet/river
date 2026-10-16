//! State version tracking for contract migrations.
//!
//! The version field allows contracts to:
//! 1. Detect states from older contract versions
//! 2. Reject states from unknown future versions
//! 3. Perform migration logic if needed

use super::{ChatRoomParametersV1, ChatRoomStateV1};
use freenet_scaffold::ComposableState;
use serde::{Deserialize, Serialize};

/// Current state version. Increment when making breaking changes to state format.
pub const CURRENT_STATE_VERSION: u32 = 1;

/// Wrapper for state version that implements ComposableState.
///
/// The version is metadata about the state format, not user content.
/// It doesn't change via deltas - it's set when the state is created
/// and verified to be compatible when loaded.
#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Debug)]
pub struct StateVersion(pub u32);

impl ComposableState for StateVersion {
    type ParentState = ChatRoomStateV1;
    type Summary = u32;
    /// The sender's version, offered when it is higher than the receiver's.
    /// Merging takes the higher of two versions both this contract reads:
    /// a total order's maximum, so merges of version 0 and 1 states commute
    /// and every existing state (merge(S, S) == S) is left as it is. Pinning
    /// every state to the current version instead would rewrite each
    /// version-0 state on its first merge (freenet/river#703 review).
    /// Changing this type re-keys the contract.
    type Delta = u32;
    type Parameters = ChatRoomParametersV1;

    fn verify(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
    ) -> Result<(), String> {
        // Accept version 0 (legacy states without version) and current version
        // Reject unknown future versions
        if self.0 > CURRENT_STATE_VERSION {
            return Err(format!(
                "Unknown state version {}. This contract supports versions 0-{}. \
                 Please upgrade your client.",
                self.0, CURRENT_STATE_VERSION
            ));
        }
        Ok(())
    }

    fn summarize(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
    ) -> Self::Summary {
        self.0
    }

    fn delta(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
        old_state_summary: &Self::Summary,
    ) -> Option<Self::Delta> {
        (self.0 > *old_state_summary).then_some(self.0)
    }

    fn apply_delta(
        &mut self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
        delta: &Option<Self::Delta>,
    ) -> Result<(), String> {
        // A version this contract does not read is ignored, as `verify`
        // would reject a state carrying it.
        if let Some(v) = delta {
            if *v > self.0 && *v <= CURRENT_STATE_VERSION {
                self.0 = *v;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use freenet_stdlib::prelude::serde_json;

    #[test]
    fn test_version_default_is_zero() {
        let v = StateVersion::default();
        assert_eq!(v.0, 0);
    }

    #[test]
    fn test_version_verify_accepts_current() {
        let v = StateVersion(CURRENT_STATE_VERSION);
        let parent = ChatRoomStateV1::default();
        let params = ChatRoomParametersV1 {
            owner: ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng).verifying_key(),
        };
        assert!(v.verify(&parent, &params).is_ok());
    }

    #[test]
    fn test_version_verify_accepts_legacy() {
        let v = StateVersion(0);
        let parent = ChatRoomStateV1::default();
        let params = ChatRoomParametersV1 {
            owner: ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng).verifying_key(),
        };
        assert!(v.verify(&parent, &params).is_ok());
    }

    #[test]
    fn test_version_verify_rejects_future() {
        let v = StateVersion(CURRENT_STATE_VERSION + 1);
        let parent = ChatRoomStateV1::default();
        let params = ChatRoomParametersV1 {
            owner: ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng).verifying_key(),
        };
        assert!(v.verify(&parent, &params).is_err());
    }

    #[test]
    fn test_version_serialization_roundtrip() {
        // Test that version field serializes and deserializes correctly
        let v = StateVersion(CURRENT_STATE_VERSION);
        let serialized = serde_json::to_string(&v).unwrap();
        let deserialized: StateVersion = serde_json::from_str(&serialized).unwrap();
        assert_eq!(v, deserialized);
    }

    #[test]
    fn test_state_without_version_field_deserializes_with_default() {
        // Simulate a legacy state JSON that doesn't have a version field
        // The #[serde(default)] attribute should make version = 0
        use crate::room_state::configuration::{AuthorizedConfigurationV1, Configuration};

        let owner_sk = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let owner_vk = owner_sk.verifying_key();

        // Create a minimal state and serialize it
        let mut state = ChatRoomStateV1::default();
        let config = Configuration {
            owner_member_id: owner_vk.into(),
            ..Default::default()
        };
        state.configuration = AuthorizedConfigurationV1::new(config, &owner_sk);

        // Serialize to JSON, then manually remove the version field to simulate legacy state
        let mut json_value: serde_json::Value = serde_json::to_value(&state).unwrap();
        if let serde_json::Value::Object(ref mut map) = json_value {
            map.remove("version");
        }
        let legacy_json = serde_json::to_string(&json_value).unwrap();

        // Deserialize - should use default version (0)
        let deserialized: ChatRoomStateV1 = serde_json::from_str(&legacy_json).unwrap();
        assert_eq!(
            deserialized.version.0, 0,
            "Legacy state without version field should deserialize with version=0"
        );
    }

    #[test]
    fn test_state_with_version_field_roundtrips() {
        use crate::room_state::configuration::{AuthorizedConfigurationV1, Configuration};

        let owner_sk = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let owner_vk = owner_sk.verifying_key();

        let mut state = ChatRoomStateV1::default();
        let config = Configuration {
            owner_member_id: owner_vk.into(),
            ..Default::default()
        };
        state.configuration = AuthorizedConfigurationV1::new(config, &owner_sk);
        state.version = StateVersion(CURRENT_STATE_VERSION);

        // Serialize and deserialize
        let json = serde_json::to_string(&state).unwrap();
        let deserialized: ChatRoomStateV1 = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.version.0, CURRENT_STATE_VERSION);
    }
}
