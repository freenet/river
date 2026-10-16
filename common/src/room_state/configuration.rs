use crate::room_state::member::MemberId;
use crate::room_state::member_info::{outranks, sig_digest, SigDigest};
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
    pub signature: Signature,
}

/// A configuration's place in the order every peer resolves collisions by:
/// higher `configuration_version` wins, and at an equal version the greater
/// signature digest wins (freenet/river#703 defect 1). Same rule, same
/// [`outranks`], as `member_info`'s rank.
pub type ConfigurationRank = (u32, SigDigest);

impl ComposableState for AuthorizedConfigurationV1 {
    type ParentState = ChatRoomStateV1;
    /// The [`ConfigurationRank`], not just the version. Two owner devices can
    /// each sign a DIFFERENT configuration at the SAME version; with only the
    /// version in the summary, neither peer could tell the other held
    /// something else, `delta` sent nothing, and the room stayed forked for
    /// good. The digest lets anti-entropy see the difference and both peers
    /// keep the higher-ranked one. Changing this type re-keys the contract.
    type Summary = ConfigurationRank;
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
        self.rank()
    }

    fn delta(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
        old_rank: &Self::Summary,
    ) -> Option<Self::Delta> {
        if outranks(self.rank(), *old_rank) {
            Some(self.clone())
        } else {
            None
        }
    }

    /// Adopt the delta's configuration if it OUTRANKS the held one.
    ///
    /// A delta that does not outrank (a re-delivery, an older version, or the
    /// losing side of an equal-version tie) is a no-op, not an error: a delta
    /// computed against a stale summary of this peer legitimately carries one,
    /// and rejecting it rejected every other field of the same delta
    /// (freenet/river#703 defect 2, `delta_idempotence`).
    ///
    /// An outranking configuration with a bad signature is a forgery and is
    /// rejected. One that is owner-signed but fails [`Self::check_values`] (a
    /// zero cap, an `owner_member_id` that is not the room owner's, oversized
    /// or public display metadata
    /// in a private room) is SKIPPED, not rejected: an `Err` for a validly
    /// signed record would fail every whole-state merge with a peer holding
    /// it, freezing every other field between them too. `verify` only checks
    /// the signature, so such a configuration can only arrive through a
    /// full-state PUT from a buggy or hostile owner client; a peer holding it
    /// then keeps offering it and the others keep declining it.
    fn apply_delta(
        &mut self,
        _parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
        delta: &Option<Self::Delta>,
    ) -> Result<(), String> {
        if let Some(delta) = delta {
            // A held configuration that is not the owner's (an imported
            // room's unsigned placeholder) does not compete: any valid
            // configuration replaces it, whatever their ranks. Otherwise a
            // placeholder could outrank the room's real configuration on the
            // signature-digest tie-break and survive a merge with it.
            let held_is_the_owners = self.verify_signature(&parameters.owner).is_ok();
            if held_is_the_owners && !outranks(delta.rank(), self.rank()) {
                return Ok(());
            }
            delta
                .verify_signature(&parameters.owner)
                .map_err(|e| format!("Invalid signature: {}", e))?;
            if delta.check_values(parameters).is_err() {
                // Skipped, deliberately not an error: see the doc comment.
                return Ok(());
            }
            self.configuration = delta.configuration.clone();
            self.signature = delta.signature;
        }

        Ok(())
    }
}

impl AuthorizedConfigurationV1 {
    /// This configuration's [`ConfigurationRank`].
    pub fn rank(&self) -> ConfigurationRank {
        (
            self.configuration.configuration_version,
            sig_digest(&self.signature),
        )
    }

    /// The checks a configuration must pass to be adopted, other than its
    /// signature and rank. `Err` names the first one it fails.
    ///
    /// Every check reads only this configuration and the room's parameters,
    /// never the configuration a peer currently holds: an answer that
    /// depended on the held configuration would differ between peers holding
    /// different ones, and one bad held configuration would make every later
    /// valid one fail (freenet/river#703 review). Clients run the same checks
    /// before signing.
    pub fn check_values(&self, parameters: &ChatRoomParametersV1) -> Result<(), String> {
        self.configuration.check_values(parameters)
    }

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
    /// The checks a configuration must pass to be adopted, other than its
    /// signature and rank; see [`AuthorizedConfigurationV1::check_values`].
    /// Clients run this before signing, because a configuration that fails it
    /// is not adopted (it is skipped, not rejected), so nothing else would
    /// tell the owner the edit did not take.
    pub fn check_values(&self, parameters: &ChatRoomParametersV1) -> Result<(), String> {
        let c = self;
        if c.owner_member_id != parameters.owner_id() {
            return Err("owner_member_id is not the room owner's".to_string());
        }
        if c.max_recent_messages == 0
            || c.max_user_bans == 0
            || c.max_message_size == 0
            || c.max_nickname_size == 0
            || c.max_members == 0
            || c.max_room_name == 0
            || c.max_room_description == 0
            || c.max_direct_messages == Some(0)
        {
            return Err("Invalid configuration values".to_string());
        }
        if c.display.name.declared_len() > c.max_room_name {
            return Err(format!(
                "Room name declared length {} exceeds max_room_name {}",
                c.display.name.declared_len(),
                c.max_room_name
            ));
        }
        if let Some(desc) = &c.display.description {
            if desc.declared_len() > c.max_room_description {
                return Err(format!(
                    "Room description declared length {} exceeds max_room_description {}",
                    desc.declared_len(),
                    c.max_room_description
                ));
            }
        }
        if c.privacy_mode == PrivacyMode::Private && c.display.name.is_public() {
            return Err("Private room must have encrypted display metadata".to_string());
        }
        Ok(())
    }

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
            (
                configuration.configuration_version,
                sig_digest(&authorized_configuration.signature)
            )
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

        let old_rank = authorized_configuration.rank();
        assert_eq!(
            new_authorized_configuration.delta(&parent_state, &parameters, &old_rank),
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
        let newer = AuthorizedConfigurationV1::new(
            Configuration {
                configuration_version: 2,
                ..Configuration::default()
            },
            &owner_signing_key,
        );
        assert_eq!(
            old_authorized_configuration.delta(&parent_state, &parameters, &newer.rank()),
            None
        );
    }

    #[test]
    fn test_apply_delta_should_apply() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let configuration = Configuration {
            owner_member_id: owner_verifying_key.into(),
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

        // An older configuration is a no-op, not an error: a delta computed
        // against a stale summary carries one (freenet/river#703).
        authorized_configuration
            .apply_delta(
                &parent_state,
                &parameters,
                &Some(new_authorized_configuration),
            )
            .unwrap();
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

        assert_eq!(
            new_authorized_configuration.check_values(&parameters),
            Err("owner_member_id is not the room owner's".to_string())
        );
        // Owner-signed, so skipped rather than rejected (freenet/river#703).
        let before = authorized_configuration.clone();
        authorized_configuration
            .apply_delta(
                &parent_state,
                &parameters,
                &Some(new_authorized_configuration),
            )
            .unwrap();
        assert_eq!(authorized_configuration, before);
    }

    #[test]
    fn test_apply_delta_invalid_values() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let configuration = Configuration {
            owner_member_id: owner_verifying_key.into(),
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
        new_configuration.max_recent_messages = 0;
        let new_authorized_configuration =
            AuthorizedConfigurationV1::new(new_configuration, &owner_signing_key);

        assert_eq!(
            new_authorized_configuration.check_values(&parameters),
            Err("Invalid configuration values".to_string())
        );
        // Owner-signed, so skipped rather than rejected (freenet/river#703).
        let before = authorized_configuration.clone();
        authorized_configuration
            .apply_delta(
                &parent_state,
                &parameters,
                &Some(new_authorized_configuration),
            )
            .unwrap();
        assert_eq!(authorized_configuration, before);
    }

    /// freenet/river#703 defect 1: two owner devices sign different
    /// configurations at the same version. Both summaries tell them apart,
    /// the higher-ranked one is offered and adopted in both directions, and
    /// re-delivering either afterwards changes nothing.
    #[test]
    fn equal_version_configurations_resolve_to_the_higher_rank() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let parameters = ChatRoomParametersV1 {
            owner: owner_signing_key.verifying_key(),
        };
        let parent_state = ChatRoomStateV1::default();
        let at_v2 = |max_members: usize| {
            AuthorizedConfigurationV1::new(
                Configuration {
                    configuration_version: 2,
                    owner_member_id: parameters.owner_id(),
                    max_members,
                    ..Configuration::default()
                },
                &owner_signing_key,
            )
        };
        let (x, y) = (at_v2(7), at_v2(9));
        let (hi, lo) = if outranks(x.rank(), y.rank()) {
            (x, y)
        } else {
            (y, x)
        };
        assert_ne!(
            hi.summarize(&parent_state, &parameters),
            lo.summarize(&parent_state, &parameters),
            "equal-version configurations must summarize differently"
        );
        assert_eq!(
            hi.delta(&parent_state, &parameters, &lo.rank()),
            Some(hi.clone())
        );
        assert_eq!(lo.delta(&parent_state, &parameters, &hi.rank()), None);

        let mut on_lo = lo.clone();
        on_lo
            .apply_delta(&parent_state, &parameters, &Some(hi.clone()))
            .unwrap();
        assert_eq!(on_lo, hi);
        let mut on_hi = hi.clone();
        on_hi
            .apply_delta(&parent_state, &parameters, &Some(lo.clone()))
            .unwrap();
        assert_eq!(on_hi, hi, "the lower-ranked one is a no-op");
        on_hi
            .apply_delta(&parent_state, &parameters, &Some(hi.clone()))
            .unwrap();
        assert_eq!(on_hi, hi, "re-delivery is a no-op");
    }

    /// A configuration's `owner_member_id` is checked against the room's
    /// owner (the contract parameters), not against whatever configuration
    /// the peer happens to hold: a held configuration naming the wrong owner
    /// must not stop the correct one from being adopted, or the outcome
    /// would depend on what each peer held (freenet/river#703 review).
    #[test]
    fn owner_member_id_is_checked_against_the_room_owner() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let parameters = ChatRoomParametersV1 {
            owner: owner_signing_key.verifying_key(),
        };
        let mut held = AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: MemberId(FastHash(1)),
                ..Configuration::default()
            },
            &owner_signing_key,
        );
        let correct = AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: parameters.owner_id(),
                configuration_version: 2,
                ..Configuration::default()
            },
            &owner_signing_key,
        );
        held.apply_delta(
            &ChatRoomStateV1::default(),
            &parameters,
            &Some(correct.clone()),
        )
        .unwrap();
        assert_eq!(held, correct, "the room owner's id is adopted");

        let wrong = AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: MemberId(FastHash(1)),
                configuration_version: 3,
                ..Configuration::default()
            },
            &owner_signing_key,
        );
        held.apply_delta(&ChatRoomStateV1::default(), &parameters, &Some(wrong))
            .unwrap();
        assert_eq!(
            held, correct,
            "a configuration naming another owner is skipped"
        );
    }

    /// An outranking configuration whose signature does not verify is a
    /// forgery and still rejects the delta.
    #[test]
    fn forged_outranking_configuration_is_rejected() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let parameters = ChatRoomParametersV1 {
            owner: owner_signing_key.verifying_key(),
        };
        let mut held = AuthorizedConfigurationV1::new(Configuration::default(), &owner_signing_key);
        let forged = AuthorizedConfigurationV1::new(
            Configuration {
                configuration_version: 9,
                ..Configuration::default()
            },
            &SigningKey::generate(&mut OsRng),
        );
        assert!(held
            .apply_delta(&ChatRoomStateV1::default(), &parameters, &Some(forged))
            .is_err());
    }
}
