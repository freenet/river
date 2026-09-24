use crate::room_state::member::MemberId;
use crate::room_state::privacy::{PrivacyMode, RoomDisplayMetadata};
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
    /// Encoded as a CBOR byte string, accepting the legacy array form on
    /// read; see [`crate::util::sig_serde`] (freenet/river#575).
    #[serde(with = "crate::util::sig_serde")]
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
            if delta.configuration.configuration_version <= self.configuration.configuration_version
            {
                return Err(
                    "New configuration version must be greater than the current version"
                        .to_string(),
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
                || delta.configuration.max_room_name == 0
                || delta.configuration.max_room_description == 0
                || delta.configuration.max_direct_messages == Some(0)
            {
                return Err("Invalid configuration values".to_string());
            }

            // Validate display metadata declared lengths
            if delta.configuration.display.name.declared_len() > delta.configuration.max_room_name {
                return Err(format!(
                    "Room name declared length {} exceeds max_room_name {}",
                    delta.configuration.display.name.declared_len(),
                    delta.configuration.max_room_name
                ));
            }

            if let Some(desc) = &delta.configuration.display.description {
                if desc.declared_len() > delta.configuration.max_room_description {
                    return Err(format!(
                        "Room description declared length {} exceeds max_room_description {}",
                        desc.declared_len(),
                        delta.configuration.max_room_description
                    ));
                }
            }

            // In private mode, ensure display metadata is encrypted
            if delta.configuration.privacy_mode == PrivacyMode::Private
                && delta.configuration.display.name.is_public()
            {
                return Err("Private room must have encrypted display metadata".to_string());
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

    /// Create an AuthorizedConfigurationV1 with a pre-computed signature.
    /// Use this when signing is done externally (e.g., via delegate).
    pub fn with_signature(configuration: Configuration, signature: Signature) -> Self {
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
            privacy_mode: PrivacyMode::default(),
            display: RoomDisplayMetadata::default(),
            max_recent_messages: 100,
            max_user_bans: 10,
            max_message_size: 1000,
            max_nickname_size: 50,
            max_members: 200,
            max_room_name: 100,
            max_room_description: 500,
            // `None`, not `Some(DEFAULT_MAX_DIRECT_MESSAGES)`: the cap is read
            // through `effective_max_direct_messages`, so leaving it unset
            // gives new rooms the same bound while keeping the serialized
            // default configuration byte-identical to pre-#519 bytes.
            max_direct_messages: None,
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
    pub privacy_mode: PrivacyMode,
    pub display: RoomDisplayMetadata,
    pub max_recent_messages: usize,
    pub max_user_bans: usize,
    pub max_message_size: usize,
    pub max_nickname_size: usize,
    pub max_members: usize,
    pub max_room_name: usize,
    pub max_room_description: usize,

    /// Owner-tunable global bound on `direct_messages.messages`, mirroring how
    /// `max_recent_messages` bounds `recent_messages`. `None` means "this
    /// configuration was signed before the field existed"; read it through
    /// [`Configuration::effective_max_direct_messages`], which substitutes
    /// [`DEFAULT_MAX_DIRECT_MESSAGES`], so pre-existing rooms are bounded
    /// without the owner having to re-sign anything.
    ///
    /// # Why `Option` + `skip_serializing_if`, and why that is NOT optional
    ///
    /// [`AuthorizedConfigurationV1::verify_signature`] re-serializes this
    /// whole struct with ciborium and checks the owner's signature over those
    /// bytes. A plain `#[serde(default)] usize` deserializes old bytes to `0`
    /// and then re-serializes them WITH the extra map entry, so the bytes no
    /// longer match what the owner signed: every room created before this
    /// field existed would fail `verify`, which also gates the #292 migration
    /// PUT — i.e. every existing room bricked, unrecoverably.
    ///
    /// `Option` + `skip_serializing_if` makes the addition byte-neutral: an
    /// old configuration decodes to `None`, re-encodes without the key, and
    /// its signature still verifies. Pinned by
    /// `legacy_configuration_bytes_still_verify_after_adding_the_field`.
    ///
    /// Any future field added to `Configuration` MUST follow this pattern AND
    /// be appended LAST — inserting an `Option` field mid-struct reorders the
    /// CBOR map for configurations that set it.
    ///
    /// The pattern is one-directional, and deliberately so. It protects OLD
    /// bytes read by NEW code. The reverse — an old-struct client reading a
    /// configuration that actually SETS this field — still breaks: serde has no
    /// `deny_unknown_fields` here, so such a client silently drops the key,
    /// re-serializes one entry short, and the owner signature fails. That is
    /// unreachable only because the contract key is `BLAKE3(wasm, params)` and
    /// both the UI and riverctl `include_bytes!` the WASM they derive the key
    /// from: a client with the old struct also derives the OLD contract key and
    /// never sees state carrying this field. Do not weaken that coupling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_direct_messages: Option<usize>,
}

/// Global cap applied to `direct_messages.messages` when a room's
/// [`Configuration::max_direct_messages`] is unset.
///
/// Stops the DM set — and therefore the DM-pinned member set that
/// `post_apply_cleanup` refuses to prune — from growing without limit. Owners
/// who want a tighter or looser bound set `max_direct_messages` explicitly.
/// See freenet/river#519.
///
/// # Why 300
///
/// Measured on the live official room when the cap was set: 499 DMs, of which
/// 377 were already older than 24 hours, occupying ~128 KB of a ~260 KB room
/// state — about half the state was DMs. 300 sheds roughly 40% of that set and
/// roughly halves the DM share of state, without being as aggressive as 200.
///
/// # What 300 costs, stated plainly
///
/// Against the per-pair cap of 100, a global 300 fixes room-wide capacity at
/// about THREE saturated ordered pairs — and ordered pairs are directional, so
/// that is one member writing to three correspondents, not three separate
/// relationships. A busy few conversations can therefore evict every other
/// conversation in the room to zero. At 500 that took five. This is a real
/// consequence of a global (rather than per-pair) bound and is accepted for the
/// same reason as the rest: per-member DM contracts make it moot.
///
/// The adversarial form is worth knowing: `verify` cannot check timestamps (a
/// contract has no wall clock) and [`check_dm_future_skew`] only runs at
/// signing time on an honest client, so a skewed or hostile client can post 100
/// future-dated DMs into each of three pairs and evict the room's DM history
/// for everyone. That property belongs to the global bound itself, not to this
/// particular number; 300 makes it cheaper than 500 did.
///
/// This DOES delete history on rollout, including messages the recipient has
/// not read yet. That cost is accepted deliberately (Ian, 2026-07-27: "people
/// are gonna lose direct messages before they've read them, but I think we just
/// need to accept that for now"). Do NOT add machinery to avoid it — read
/// tracking, age exemptions, or a grace period would all be wall-clock-
/// dependent, which `ChatRoomStateV1::post_apply_cleanup` forbids, and would be
/// thrown away by the successor design below.
///
/// # This bound is interim
///
/// The intended long-term fix is moving DMs out of the room contract entirely,
/// into dedicated per-member contracts. This cap exists to bound the damage
/// until then, so it is deliberately the simplest correct thing: a count, a
/// deterministic order, and a horizon. The trim lives wholly inside
/// `direct_messages.rs` and `post_apply_cleanup` was not touched, so the
/// retention logic is a clean deletion later.
///
/// One piece does NOT delete cleanly, and it is worth knowing now:
/// [`Configuration::max_direct_messages`] is a signed wire field. Once any
/// owner has set it explicitly, a future struct that removes it re-serializes
/// one CBOR entry short and that owner's signature fails — see the field's own
/// note. So the field itself is effectively permanent even after DMs move out;
/// it would have to be kept and ignored, not dropped.
pub const DEFAULT_MAX_DIRECT_MESSAGES: usize = 300;

impl Configuration {
    /// The global DM cap in force for this room: the owner's explicit
    /// [`Self::max_direct_messages`], or [`DEFAULT_MAX_DIRECT_MESSAGES`] when
    /// unset. Every retention and horizon decision MUST read the cap through
    /// here so a legacy (`None`) configuration and an explicitly-defaulted one
    /// behave identically.
    pub fn effective_max_direct_messages(&self) -> usize {
        self.max_direct_messages
            .unwrap_or(DEFAULT_MAX_DIRECT_MESSAGES)
    }
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

        let parent_state = ChatRoomStateV1 {
            configuration: authorized_configuration.clone(),
            ..Default::default()
        };
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

        let parent_state = ChatRoomStateV1 {
            configuration: authorized_configuration.clone(),
            ..Default::default()
        };
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

        let parent_state = ChatRoomStateV1 {
            configuration: old_authorized_configuration.clone(),
            ..Default::default()
        };
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

        let parent_state = ChatRoomStateV1 {
            configuration: authorized_configuration.clone(),
            ..Default::default()
        };
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

        let parent_state = ChatRoomStateV1 {
            configuration: authorized_configuration.clone(),
            ..Default::default()
        };
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

        let parent_state = ChatRoomStateV1 {
            configuration: authorized_configuration.clone(),
            ..Default::default()
        };
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

        let parent_state = ChatRoomStateV1 {
            configuration: authorized_configuration.clone(),
            ..Default::default()
        };
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
