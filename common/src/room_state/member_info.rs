use crate::room_state::member::MemberId;
use crate::room_state::privacy::SealedBytes;
use crate::room_state::ChatRoomParametersV1;
use crate::room_state::ChatRoomStateV1;
use crate::util::{sign_struct, verify_struct};
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use freenet_scaffold::ComposableState;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Maximum number of deputies a single member may list in their `MemberInfo`,
/// to bound state-bloat abuse (deputy ban authority, #410). A `MemberInfo`
/// whose `deputies` list exceeds this is rejected by `MemberInfoV1::verify`.
pub const MAX_DEPUTIES: usize = 64;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct MemberInfoV1 {
    pub member_info: Vec<AuthorizedMemberInfo>,
}

impl MemberInfoV1 {
    /// The deputies currently listed by `member_id`'s own signed `MemberInfo`,
    /// or an empty slice if that member has no info entry or no deputies.
    ///
    /// Deputies are a member's authenticated statement (their own signed
    /// `MemberInfo`) that the listed members may ban within the deputizing
    /// member's invite subtree (#410).
    ///
    /// If two records for the same member are ever present at once, pick the
    /// **highest-rank** one (`member_info_rank`: higher version, else greater
    /// signature) — the SAME winner `summarize`/`apply_delta` converge on. A
    /// bare `.find()` (first) could return a lower-rank record and disagree with
    /// what anti-entropy propagates, permanently diverging ban authority from
    /// the converged state (#411 round 7 / Codex P1 #3). `verify` now rejects
    /// duplicates outright, so this only guards a transient in-memory state
    /// mid-apply; it makes lookup consistent with the wire layer regardless.
    pub fn deputies_of(&self, member_id: MemberId) -> &[MemberId] {
        self.member_info
            .iter()
            .filter(|info| info.member_info.member_id == member_id)
            .max_by_key(|info| member_info_rank(info.member_info.version, &info.signature))
            .map(|info| info.member_info.deputies.as_slice())
            .unwrap_or(&[])
    }
}

/// Total, deterministic ordering used to pick the canonical `MemberInfo` when
/// two signed records for the SAME member collide (#411 round 4 item B).
///
/// Rule: **higher `version` wins; at equal version, the lexicographically-greater
/// SIGNATURE wins.** Two records with the same member and version but different
/// content (e.g. different `deputies`) have different signatures — the signature
/// is over the whole `MemberInfo` — so this breaks the tie deterministically.
///
/// It is applied IDENTICALLY in [`ComposableState::apply_delta`] (conflict
/// resolution), [`ComposableState::delta`], and [`ComposableState::summarize`]
/// (via the `(version, signature)` summary value), so anti-entropy can DETECT a
/// same-version content difference and both peers converge on the same record.
/// Without it, equal-version resolution was order-dependent AND the summary
/// carried only the version, so anti-entropy saw "same version", sent no
/// correction, and peers disagreed on ban authority permanently.
fn member_info_rank(version: u32, signature: &Signature) -> (u32, [u8; 64]) {
    (version, signature.to_bytes())
}

impl ComposableState for MemberInfoV1 {
    type ParentState = ChatRoomStateV1;
    /// `(version, signature)` per member. The signature is the equal-version
    /// tiebreak discriminator (see [`member_info_rank`]); carrying it lets
    /// anti-entropy detect a content difference at the SAME version (#411 B).
    type Summary = HashMap<MemberId, (u32, Signature)>;
    type Delta = Vec<AuthorizedMemberInfo>;
    type Parameters = ChatRoomParametersV1;

    fn verify(
        &self,
        parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
    ) -> Result<(), String> {
        let members_by_id = parent_state.members.members_by_member_id();
        let owner_id = parameters.owner_id();

        // Reject a state carrying more than one `member_info` record for the same
        // member (#411 round 7 / Codex P1 #3). Two validly-signed records for one
        // `member_id` make `deputies_of` (which reads ONE record) and
        // `summarize`/`apply_delta` (LWW by `member_info_rank`) disagree on that
        // member's deputies, so ban ENFORCEMENT and ANTI-ENTROPY permanently
        // diverge. Migration-safe: `apply_delta` keeps at most one entry per
        // member (it updates-or-inserts by `member_id`), so every legitimately
        // produced state — including the Official room's migration PUT — already
        // has <=1 per member and still verifies.
        let mut seen_member_ids: HashSet<MemberId> = HashSet::new();
        for member_info in &self.member_info {
            if !seen_member_ids.insert(member_info.member_info.member_id) {
                return Err(format!(
                    "Duplicate member_info for member: {:?}",
                    member_info.member_info.member_id
                ));
            }
        }

        for member_info in &self.member_info {
            let member_id = member_info.member_info.member_id;

            // Bound the deputy list to prevent state-bloat abuse (#410).
            if member_info.member_info.deputies.len() > MAX_DEPUTIES {
                return Err(format!(
                    "Member {:?} lists {} deputies, exceeding the maximum of {}",
                    member_id,
                    member_info.member_info.deputies.len(),
                    MAX_DEPUTIES
                ));
            }

            if member_id == owner_id {
                // If this is the owner's member info, verify against owner's key
                member_info.verify_signature(parameters)?;
            } else {
                // For non-owner members, verify they exist in members list
                let member = members_by_id.get(&member_id).ok_or_else(|| {
                    format!("MemberInfo exists for non-existent member: {:?}", member_id)
                })?;

                // Verify the signature with member's key
                member_info.verify_signature_with_key(&member.member.member_vk)?;
            }
        }
        Ok(())
    }

    fn summarize(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
    ) -> Self::Summary {
        // Carry the signature alongside the version so anti-entropy can detect a
        // SAME-version content difference and correct it (#411 round 4 B).
        self.member_info
            .iter()
            .map(|info| {
                (
                    info.member_info.member_id,
                    (info.member_info.version, info.signature),
                )
            })
            .collect()
    }

    fn delta(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
        old_state_summary: &Self::Summary,
    ) -> Option<Self::Delta> {
        let delta: Vec<AuthorizedMemberInfo> = self
            .member_info
            .iter()
            .filter(|info| {
                // Include if the member is absent from the old summary, OR this
                // record OUTRANKS what the old summary has (higher version, or
                // equal version with a greater signature). The equal-version arm
                // is what lets a same-version content difference propagate (#411
                // round 4 B) — without it, anti-entropy would never send the
                // correction and peers would disagree on deputies forever.
                match old_state_summary.get(&info.member_info.member_id) {
                    None => true,
                    Some((old_version, old_signature)) => {
                        member_info_rank(info.member_info.version, &info.signature)
                            > member_info_rank(*old_version, old_signature)
                    }
                }
            })
            .cloned()
            .collect();

        if delta.is_empty() {
            None
        } else {
            Some(delta)
        }
    }

    fn apply_delta(
        &mut self,
        parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
        delta: &Option<Self::Delta>,
    ) -> Result<(), String> {
        let max_nickname_size = parent_state.configuration.configuration.max_nickname_size;

        if let Some(delta) = delta {
            for member_info in delta {
                let member_id = &member_info.member_info.member_id;

                // Validate nickname declared length
                if member_info.member_info.preferred_nickname.declared_len() > max_nickname_size {
                    return Err(format!(
                        "Nickname declared length {} exceeds max_nickname_size {}",
                        member_info.member_info.preferred_nickname.declared_len(),
                        max_nickname_size
                    ));
                }

                // Enforce the deputy-list cap at the DELTA boundary too — `verify`
                // rejects an over-cap record on stored state, but without this an
                // over-cap self-signed record would enter state via a delta and
                // then block new-joiner / migration full-state validation (#410).
                // SKIP the offending entry (like the removed-member case below)
                // rather than erroring the whole delta: erroring would let one
                // malicious over-cap record deadlock every full-state merge that
                // carries it (the receiver would reject the entire state and never
                // converge). Skipping is deterministic across peers and drops only
                // the bad entry.
                if member_info.member_info.deputies.len() > MAX_DEPUTIES {
                    continue;
                }

                // Check if this is the room owner
                if *member_id == parameters.owner_id() {
                    // If it's the owner, verify against the room owner's key
                    member_info.verify_signature(parameters)?;
                } else {
                    // For non-owners, verify they exist and check their signature.
                    // If the member was removed (e.g. banned or max_members), skip
                    // this entry — retention cleanup below will handle it.
                    let members = parent_state.members.members_by_member_id();
                    let member = match members.get(member_id) {
                        Some(m) => m,
                        None => continue,
                    };
                    member_info.verify_signature_with_key(&member.member.member_vk)?;
                }

                // Update or add the member info. Conflict resolution uses the
                // total, deterministic `member_info_rank` order (higher version,
                // else greater signature) so that two DIFFERENT records for the
                // same member at the SAME version resolve identically regardless
                // of delta arrival order (#411 round 4 B). Using only
                // `version >` (as before) left equal-version conflicts
                // order-dependent, so peers could permanently disagree on
                // `deputies` (and therefore on ban authority).
                if let Some(existing_info) = self
                    .member_info
                    .iter_mut()
                    .find(|info| info.member_info.member_id == *member_id)
                {
                    if member_info_rank(member_info.member_info.version, &member_info.signature)
                        > member_info_rank(
                            existing_info.member_info.version,
                            &existing_info.signature,
                        )
                    {
                        *existing_info = member_info.clone();
                    }
                } else {
                    self.member_info.push(member_info.clone());
                }
            }
        }
        // Always remove any member info that is not in parent_state.members
        let member_map = parent_state.members.members_by_member_id();
        self.member_info.retain(|info| {
            parameters.owner_id() == info.member_info.member_id
                || member_map.contains_key(&info.member_info.member_id)
        });

        // Sort for deterministic ordering (CRDT convergence requirement)
        self.member_info
            .sort_by_key(|info| info.member_info.member_id);

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthorizedMemberInfo {
    pub member_info: MemberInfo,
    pub signature: Signature,
}

impl AuthorizedMemberInfo {
    pub fn new(member_info: MemberInfo, owner_signing_key: &SigningKey) -> Self {
        let signature = sign_struct(&member_info, owner_signing_key);
        Self {
            member_info,
            signature,
        }
    }

    pub fn new_with_member_key(member_info: MemberInfo, member_signing_key: &SigningKey) -> Self {
        let signature = sign_struct(&member_info, member_signing_key);
        Self {
            member_info,
            signature,
        }
    }

    /// Create an AuthorizedMemberInfo with a pre-computed signature.
    /// Use this when signing is done externally (e.g., via delegate).
    pub fn with_signature(member_info: MemberInfo, signature: Signature) -> Self {
        Self {
            member_info,
            signature,
        }
    }

    pub fn verify_signature(&self, parameters: &ChatRoomParametersV1) -> Result<(), String> {
        self.verify_signature_with_key(&parameters.owner)
    }

    pub fn verify_signature_with_key(&self, verifying_key: &VerifyingKey) -> Result<(), String> {
        verify_struct(&self.member_info, &self.signature, verifying_key)
            .map_err(|e| format!("Invalid signature: {}", e))
    }

    // Helper method for tests
    #[cfg(test)]
    pub fn with_invalid_signature(mut self) -> Self {
        self.signature = Signature::from_bytes(&[0; 64]);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemberInfo {
    pub member_id: MemberId,
    pub version: u32,
    pub preferred_nickname: SealedBytes,
    /// Members this member has deputized to ban within this member's invite
    /// subtree (deputy ban authority, #410). Empty for the vast majority of
    /// members.
    ///
    /// LOAD-BEARING: this MUST be the LAST field and MUST keep BOTH
    /// `#[serde(default)]` (so pre-#410 records — which have no `deputies`
    /// key — still deserialize) AND `skip_serializing_if = "Vec::is_empty"`
    /// (so an EMPTY list serializes byte-identically to the old 3-field
    /// record). `MemberInfo` is INDIVIDUALLY signed over its ciborium bytes
    /// (`AuthorizedMemberInfo`), so a plain `#[serde(default)]` alone would
    /// re-serialize every existing member's record with an extra field,
    /// breaking their signature on migration and stranding every existing
    /// room. Never reorder the first three fields. Pinned by
    /// `empty_deputies_serializes_identically_to_legacy_member_info`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deputies: Vec<MemberId>,
}

impl MemberInfo {
    /// Create a new member info with a public nickname
    pub fn new_public(member_id: MemberId, version: u32, nickname: String) -> Self {
        Self {
            member_id,
            version,
            preferred_nickname: SealedBytes::public(nickname.into_bytes()),
            deputies: Vec::new(),
        }
    }

    /// Create a new member info with a private nickname
    pub fn new_private(
        member_id: MemberId,
        version: u32,
        ciphertext: Vec<u8>,
        nonce: [u8; 12],
        secret_version: u32,
        declared_len: u32,
    ) -> Self {
        Self {
            member_id,
            version,
            preferred_nickname: SealedBytes::private(
                ciphertext,
                nonce,
                secret_version,
                declared_len,
            ),
            deputies: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room_state::member::{AuthorizedMember, Member};
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;

    fn create_test_member_info(member_id: MemberId) -> MemberInfo {
        MemberInfo::new_public(member_id, 1, "TestUser".to_string())
    }

    /// LOAD-BEARING regression test (issue #410).
    ///
    /// `MemberInfo` is individually signed over its ciborium bytes
    /// (`AuthorizedMemberInfo::new*` -> `sign_struct`; `verify_signature`
    /// re-serializes and checks). Adding `deputies` with a PLAIN
    /// `#[serde(default)]` would make the new WASM re-serialize a 4-field
    /// struct, changing the bytes and breaking every existing member's
    /// signature -> `validate_state` rejects the permissionless migration PUT
    /// -> every existing room migrates to empty. The
    /// `skip_serializing_if = "Vec::is_empty"` attribute makes an empty
    /// `deputies` list serialize byte-identically to the old 3-field record,
    /// so old signatures still verify.
    ///
    /// This test constructs the OLD 3-field shape, signs its ciborium bytes,
    /// and asserts the new `MemberInfo` with an empty `deputies` list (a)
    /// serializes to byte-identical bytes and (b) still verifies against that
    /// old signature. It MUST fail if `skip_serializing_if` is dropped.
    #[test]
    fn empty_deputies_serializes_identically_to_legacy_member_info() {
        use crate::util::{sign_struct, verify_struct};

        // Exact mirror of the pre-#410 3-field MemberInfo layout, in order.
        #[derive(Serialize)]
        struct OldMemberInfo {
            member_id: MemberId,
            version: u32,
            preferred_nickname: SealedBytes,
        }

        let signing_key = SigningKey::generate(&mut OsRng);
        let member_id: MemberId = signing_key.verifying_key().into();
        let nickname = SealedBytes::public("LegacyNick".to_string().into_bytes());

        let old = OldMemberInfo {
            member_id,
            version: 7,
            preferred_nickname: nickname.clone(),
        };
        // New struct: same first three fields, EMPTY deputies.
        let new_empty = MemberInfo {
            member_id,
            version: 7,
            preferred_nickname: nickname.clone(),
            deputies: Vec::new(),
        };

        // (a) direct byte-identity of the ciborium serialization.
        let mut old_bytes = Vec::new();
        ciborium::ser::into_writer(&old, &mut old_bytes).unwrap();
        let mut new_bytes = Vec::new();
        ciborium::ser::into_writer(&new_empty, &mut new_bytes).unwrap();
        assert_eq!(
            old_bytes, new_bytes,
            "MemberInfo with empty deputies MUST serialize byte-identically to \
             the legacy 3-field record; dropping skip_serializing_if breaks this \
             and strands every existing room (issue #410)"
        );

        // (b) a signature over the OLD record still verifies against the NEW struct.
        let signature = sign_struct(&old, &signing_key);
        assert!(
            verify_struct(&new_empty, &signature, &signing_key.verifying_key()).is_ok(),
            "signature over legacy MemberInfo bytes must still verify against the \
             new struct with empty deputies (proves byte-identical serialization)"
        );

        // Sanity: a NON-empty deputies list MUST change the bytes (proves the
        // field really is serialized when populated, so it is not a silent no-op).
        let with_deputy = MemberInfo {
            member_id,
            version: 7,
            preferred_nickname: nickname,
            deputies: vec![member_id],
        };
        let mut with_deputy_bytes = Vec::new();
        ciborium::ser::into_writer(&with_deputy, &mut with_deputy_bytes).unwrap();
        assert_ne!(
            old_bytes, with_deputy_bytes,
            "a populated deputies list must change the serialized bytes"
        );
    }

    #[test]
    fn test_member_info_v1_default() {
        let default_member_info = MemberInfoV1::default();
        assert!(default_member_info.member_info.is_empty());
    }

    #[test]
    fn test_member_info_v1_verify() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = owner_signing_key.verifying_key();
        let owner_id = owner_verifying_key.into();

        let member_signing_key = SigningKey::generate(&mut OsRng);
        let member_verifying_key = member_signing_key.verifying_key();
        let member_id = member_verifying_key.into();

        let member_info = create_test_member_info(member_id);
        let authorized_member_info = AuthorizedMemberInfo::new(member_info, &member_signing_key);

        let mut member_info_v1 = MemberInfoV1::default();
        member_info_v1
            .member_info
            .push(authorized_member_info.clone());

        let mut parent_state = ChatRoomStateV1::default();
        let member = Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: member_verifying_key,
        };
        let authorized_member = AuthorizedMember::new(member, &owner_signing_key);
        parent_state.members.members.push(authorized_member);

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        let result = member_info_v1.verify(&parent_state, &parameters);
        assert!(
            result.is_ok(),
            "Verification failed: {}",
            result.unwrap_err()
        );

        // Test with non-existent member
        let non_existent_member_id = SigningKey::generate(&mut OsRng).verifying_key().into();
        let non_existent_member_info = create_test_member_info(non_existent_member_id);
        let non_existent_authorized_member_info =
            AuthorizedMemberInfo::new(non_existent_member_info, &owner_signing_key);
        member_info_v1
            .member_info
            .push(non_existent_authorized_member_info);

        let verify_result = member_info_v1.verify(&parent_state, &parameters);
        assert!(
            verify_result.is_err(),
            "Expected verification to fail, but it succeeded"
        );
        if let Err(err) = verify_result {
            assert!(
                err.contains("MemberInfo exists for non-existent member"),
                "Unexpected error message: {}",
                err
            );
        }

        // Test with invalid signature
        let invalid_authorized_member_info = authorized_member_info.with_invalid_signature();
        member_info_v1.member_info.clear();
        member_info_v1
            .member_info
            .push(invalid_authorized_member_info);

        let verify_result = member_info_v1.verify(&parent_state, &parameters);
        assert!(
            verify_result.is_err(),
            "Expected verification to fail, but it succeeded"
        );
        if let Err(err) = verify_result {
            assert!(
                err.contains("Invalid signature"),
                "Unexpected error message: {}",
                err
            );
        }
    }

    #[test]
    fn test_member_info_v1_summarize() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let member_id = SigningKey::generate(&mut OsRng).verifying_key().into();
        let member_info = create_test_member_info(member_id);
        let authorized_member_info = AuthorizedMemberInfo::new(member_info, &owner_signing_key);

        let mut member_info_v1 = MemberInfoV1::default();
        member_info_v1.member_info.push(authorized_member_info);

        let parent_state = ChatRoomStateV1::default();
        let parameters = ChatRoomParametersV1 {
            owner: owner_signing_key.verifying_key(),
        };

        let summary = member_info_v1.summarize(&parent_state, &parameters);
        assert_eq!(summary.len(), 1);
        assert!(summary.contains_key(&member_id));
        assert_eq!(summary.get(&member_id).unwrap().0, 1); // Version should be 1
    }

    #[test]
    fn test_member_info_v1_delta() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let member_id1 = SigningKey::generate(&mut OsRng).verifying_key().into();
        let member_id2 = SigningKey::generate(&mut OsRng).verifying_key().into();

        let member_info1 = create_test_member_info(member_id1);
        let member_info2 = create_test_member_info(member_id2);

        let authorized_member_info1 = AuthorizedMemberInfo::new(member_info1, &owner_signing_key);
        let authorized_member_info2 = AuthorizedMemberInfo::new(member_info2, &owner_signing_key);
        // Capture member1's signature for the summary tiebreak (#411 round 4 B).
        let sig1 = authorized_member_info1.signature;

        let mut member_info_v1 = MemberInfoV1::default();
        member_info_v1.member_info.push(authorized_member_info1);
        member_info_v1.member_info.push(authorized_member_info2);

        let parent_state = ChatRoomStateV1::default();
        let parameters = ChatRoomParametersV1 {
            owner: owner_signing_key.verifying_key(),
        };

        // Summary says the peer already holds member1 at (version 1, sig1), so
        // member1 does not outrank it and only member2 appears in the delta.
        let mut old_summary = HashMap::new();
        old_summary.insert(member_id1, (1, sig1));

        let delta = member_info_v1.delta(&parent_state, &parameters, &old_summary);

        assert!(delta.is_some());
        let delta = delta.unwrap();
        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0].member_info.member_id, member_id2);
    }

    #[test]
    fn test_member_info_v1_apply_delta() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = owner_signing_key.verifying_key();
        let owner_id = owner_verifying_key.into();

        let member_signing_key = SigningKey::generate(&mut OsRng);
        let member_verifying_key = member_signing_key.verifying_key();
        let member_id = member_verifying_key.into();

        let member_info = create_test_member_info(member_id);
        let authorized_member_info =
            AuthorizedMemberInfo::new_with_member_key(member_info, &member_signing_key);

        let mut member_info_v1 = MemberInfoV1::default();
        let delta = vec![authorized_member_info.clone()];

        let mut parent_state = ChatRoomStateV1::default();
        parent_state.members.members.push(AuthorizedMember {
            member: Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: member_verifying_key,
            },
            signature: owner_signing_key
                .sign("TestUser".as_bytes())
                .to_bytes()
                .into(),
        });

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        // Test applying delta with a new member
        println!("Applying delta with a new member");
        let result = member_info_v1.apply_delta(&parent_state, &parameters, &Some(delta));
        println!("Result: {:?}", result);
        assert!(result.is_ok(), "Failed to apply delta: {:?}", result.err());
        assert_eq!(member_info_v1.member_info.len(), 1);
        assert_eq!(member_info_v1.member_info[0], authorized_member_info);

        // Test applying delta with an existing member (update)
        println!("Applying delta with an existing member (update)");
        let updated_member_info =
            MemberInfo::new_public(member_id, 2, "UpdatedNickname".to_string());
        let updated_authorized_member_info =
            AuthorizedMemberInfo::new_with_member_key(updated_member_info, &member_signing_key);
        let update_delta = vec![updated_authorized_member_info.clone()];

        let result = member_info_v1.apply_delta(&parent_state, &parameters, &Some(update_delta));
        println!("Result: {:?}", result);
        assert!(
            result.is_ok(),
            "Failed to apply update delta: {:?}",
            result.err()
        );
        assert_eq!(member_info_v1.member_info.len(), 1);
        assert_eq!(
            member_info_v1.member_info[0],
            updated_authorized_member_info
        );

        // Test applying delta with a non-existent member (should succeed, entry silently dropped)
        println!("Applying delta with a non-existent member");
        let non_existent_member_id = SigningKey::generate(&mut OsRng).verifying_key().into();
        let non_existent_member_info = create_test_member_info(non_existent_member_id);
        let non_existent_authorized_member_info = AuthorizedMemberInfo::new_with_member_key(
            non_existent_member_info,
            &SigningKey::generate(&mut OsRng),
        );
        let non_existent_delta = vec![non_existent_authorized_member_info];

        let prev_len = member_info_v1.member_info.len();
        let result =
            member_info_v1.apply_delta(&parent_state, &parameters, &Some(non_existent_delta));
        println!("Result: {:?}", result);
        assert!(
            result.is_ok(),
            "Non-existent member should be silently skipped"
        );
        assert_eq!(
            member_info_v1.member_info.len(),
            prev_len,
            "Entry should not be added"
        );

        // Test applying delta with an older version (should not update)
        println!("Applying delta with an older version");
        let older_member_info = MemberInfo::new_public(member_id, 1, "TestUser".to_string());
        let older_authorized_member_info =
            AuthorizedMemberInfo::new_with_member_key(older_member_info, &member_signing_key);
        let older_delta = vec![older_authorized_member_info];

        let result = member_info_v1.apply_delta(&parent_state, &parameters, &Some(older_delta));
        println!("Result: {:?}", result);
        assert!(
            result.is_ok(),
            "Failed to apply older version delta: {:?}",
            result.err()
        );
        assert_eq!(member_info_v1.member_info.len(), 1);
        assert_eq!(member_info_v1.member_info[0].member_info.version, 2);

        // Test applying delta with multiple members
        println!("Applying delta with multiple members");
        let new_member_signing_key = SigningKey::generate(&mut OsRng);
        let new_member_verifying_key = new_member_signing_key.verifying_key();
        let new_member_id = new_member_verifying_key.into();
        let new_member_info = create_test_member_info(new_member_id);
        let new_authorized_member_info =
            AuthorizedMemberInfo::new_with_member_key(new_member_info, &new_member_signing_key);

        parent_state.members.members.push(AuthorizedMember {
            member: Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: new_member_verifying_key,
            },
            signature: owner_signing_key
                .sign("NewTestUser".as_bytes())
                .to_bytes()
                .into(),
        });

        let multi_delta = vec![
            updated_authorized_member_info.clone(),
            new_authorized_member_info.clone(),
        ];

        let result = member_info_v1.apply_delta(&parent_state, &parameters, &Some(multi_delta));
        println!("Result: {:?}", result);
        assert!(
            result.is_ok(),
            "Failed to apply multi-member delta: {:?}",
            result.err()
        );
        assert_eq!(member_info_v1.member_info.len(), 2);
        assert!(member_info_v1
            .member_info
            .contains(&updated_authorized_member_info));
        assert!(member_info_v1
            .member_info
            .contains(&new_authorized_member_info));
    }

    #[test]
    fn test_authorized_member_info_new_and_verify() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let member_id = SigningKey::generate(&mut OsRng).verifying_key().into();
        let member_info = create_test_member_info(member_id);

        let authorized_member_info =
            AuthorizedMemberInfo::new(member_info.clone(), &owner_signing_key);

        let parameters = ChatRoomParametersV1 {
            owner: owner_signing_key.verifying_key(),
        };

        assert!(authorized_member_info.verify_signature(&parameters).is_ok());

        // Test with wrong key
        let wrong_key = SigningKey::generate(&mut OsRng).verifying_key();
        let wrong_parameters = ChatRoomParametersV1 { owner: wrong_key };
        assert!(authorized_member_info
            .verify_signature(&wrong_parameters)
            .is_err());
    }

    #[test]
    fn test_member_info_v1_delta_scenarios() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = owner_signing_key.verifying_key();

        let mut member_info_v1 = MemberInfoV1::default();
        let parent_state = ChatRoomStateV1::default();
        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        // Generate 5 member infos
        let member_infos: Vec<AuthorizedMemberInfo> = (0..5)
            .map(|_| {
                let member_id = SigningKey::generate(&mut OsRng).verifying_key().into();
                let member_info = create_test_member_info(member_id);
                AuthorizedMemberInfo::new(member_info, &owner_signing_key)
            })
            .collect();

        // Test when all members are new
        member_info_v1.member_info = member_infos.clone();
        let delta = member_info_v1.delta(&parent_state, &parameters, &HashMap::new());
        assert_eq!(delta.unwrap().len(), 5);

        // Test when all members are old with the same (version, signature) —
        // nothing outranks the summary, so the delta is empty (#411 round 4 B).
        let old_summary: HashMap<MemberId, (u32, Signature)> = member_infos
            .iter()
            .map(|info| {
                (
                    info.member_info.member_id,
                    (info.member_info.version, info.signature),
                )
            })
            .collect();
        let delta = member_info_v1.delta(&parent_state, &parameters, &old_summary);
        assert!(delta.is_none());

        // Test with a mix of new and old members
        let mut old_summary = HashMap::new();
        old_summary.insert(
            member_infos[0].member_info.member_id,
            (1, member_infos[0].signature),
        );
        old_summary.insert(
            member_infos[1].member_info.member_id,
            (1, member_infos[1].signature),
        );
        let delta = member_info_v1.delta(&parent_state, &parameters, &old_summary);
        assert_eq!(delta.unwrap().len(), 3);

        // Test with updated version
        let mut updated_member_info = member_infos[0].clone();
        updated_member_info.member_info.version = 2;
        member_info_v1.member_info[0] = updated_member_info;

        let delta = member_info_v1.delta(&parent_state, &parameters, &old_summary);
        assert_eq!(delta.unwrap().len(), 4); // 3 new members + 1 updated member
    }

    #[test]
    fn test_member_info_version_handling() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = owner_signing_key.verifying_key();
        let owner_id = owner_verifying_key.into();

        // Create a member
        let member_signing_key = SigningKey::generate(&mut OsRng);
        let member_verifying_key = member_signing_key.verifying_key();
        let member_id = member_verifying_key.into();

        // Create initial member info with version 1
        let member_info_v1 = create_test_member_info(member_id);
        let authorized_member_info_v1 =
            AuthorizedMemberInfo::new_with_member_key(member_info_v1, &member_signing_key);

        // Create updated member info with version 2
        let member_info_v2 = MemberInfo::new_public(member_id, 2, "UpdatedNickname".to_string());
        let authorized_member_info_v2 =
            AuthorizedMemberInfo::new_with_member_key(member_info_v2, &member_signing_key);

        // Set up state with version 1
        let mut member_info_state = MemberInfoV1::default();
        member_info_state
            .member_info
            .push(authorized_member_info_v1.clone());

        // Create parent state with the member
        let mut parent_state = ChatRoomStateV1::default();
        let member = Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: member_verifying_key,
        };
        let authorized_member = AuthorizedMember::new(member, &owner_signing_key);
        parent_state.members.members.push(authorized_member);

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        // Create summary with version 1
        let summary = member_info_state.summarize(&parent_state, &parameters);
        assert_eq!(summary.get(&member_id).unwrap().0, 1);

        // Create delta with version 2
        let mut updated_state = MemberInfoV1::default();
        updated_state
            .member_info
            .push(authorized_member_info_v2.clone());

        let delta = updated_state.delta(&parent_state, &parameters, &summary);
        assert!(delta.is_some());
        assert_eq!(delta.as_ref().unwrap().len(), 1);
        assert_eq!(delta.as_ref().unwrap()[0].member_info.version, 2);

        // Apply delta and verify version is updated
        member_info_state
            .apply_delta(&parent_state, &parameters, &delta)
            .unwrap();
        assert_eq!(member_info_state.member_info.len(), 1);
        assert_eq!(member_info_state.member_info[0].member_info.version, 2);
        assert_eq!(
            member_info_state.member_info[0]
                .member_info
                .preferred_nickname,
            SealedBytes::public("UpdatedNickname".to_string().into_bytes())
        );
    }

    #[test]
    fn test_room_owner_member_info() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = owner_signing_key.verifying_key();
        let owner_id = owner_verifying_key.into();

        let owner_member_info = create_test_member_info(owner_id);
        let authorized_owner_info =
            AuthorizedMemberInfo::new(owner_member_info, &owner_signing_key);

        let mut member_info_v1 = MemberInfoV1::default();
        member_info_v1.member_info.push(authorized_owner_info);

        let mut parent_state = ChatRoomStateV1::default();
        parent_state.members.members.push(AuthorizedMember {
            member: Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: owner_verifying_key,
            },
            signature: owner_signing_key
                .sign("TestOwner".as_bytes())
                .to_bytes()
                .into(),
        });

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        let result = member_info_v1.verify(&parent_state, &parameters);
        assert!(
            result.is_ok(),
            "Room owner should be allowed to have member info: {:?}",
            result
        );
    }

    #[test]
    fn test_member_info_retention() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = owner_signing_key.verifying_key();
        let owner_id = owner_verifying_key.into();

        // Create owner's member info
        let owner_member_info = create_test_member_info(owner_id);
        let authorized_owner_info =
            AuthorizedMemberInfo::new(owner_member_info, &owner_signing_key);

        // Create regular member's info
        let member_signing_key = SigningKey::generate(&mut OsRng);
        let member_verifying_key = member_signing_key.verifying_key();
        let member_id = member_verifying_key.into();
        let member_info = create_test_member_info(member_id);
        let authorized_member_info =
            AuthorizedMemberInfo::new_with_member_key(member_info, &member_signing_key);

        // Set up MemberInfoV1 with both owner and member info
        let mut member_info_v1 = MemberInfoV1::default();
        member_info_v1
            .member_info
            .push(authorized_owner_info.clone());
        member_info_v1
            .member_info
            .push(authorized_member_info.clone());

        // Set up parent state with only the regular member
        let mut parent_state = ChatRoomStateV1::default();
        parent_state.members.members.push(AuthorizedMember {
            member: Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: member_verifying_key,
            },
            signature: owner_signing_key
                .sign("TestMember".as_bytes())
                .to_bytes()
                .into(),
        });

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        // Apply an empty delta to trigger retention logic
        let result = member_info_v1.apply_delta(&parent_state, &parameters, &Some(vec![]));
        assert!(result.is_ok(), "Failed to apply delta: {:?}", result.err());

        // Verify that owner's info is retained even though not in members list
        assert!(
            member_info_v1
                .member_info
                .iter()
                .any(|info| info.member_info.member_id == owner_id),
            "Owner's member info should be retained"
        );

        // Remove the regular member from parent state
        parent_state.members.members.clear();

        // Apply another empty delta
        let result = member_info_v1.apply_delta(&parent_state, &parameters, &Some(vec![]));
        assert!(
            result.is_ok(),
            "Failed to apply second delta: {:?}",
            result.err()
        );

        // Verify that only owner's info remains
        assert_eq!(
            member_info_v1.member_info.len(),
            1,
            "Should only contain owner's info"
        );
        assert_eq!(
            member_info_v1.member_info[0].member_info.member_id, owner_id,
            "Remaining info should be owner's"
        );
    }

    /// Regression test: apply_delta should succeed when the delta contains
    /// member_info for a member that was simultaneously removed from
    /// parent_state.members (e.g. ban or max_members eviction).
    #[test]
    fn test_apply_delta_with_removed_member_info() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = owner_signing_key.verifying_key();
        let owner_id = owner_verifying_key.into();

        let member_signing_key = SigningKey::generate(&mut OsRng);
        let member_verifying_key = member_signing_key.verifying_key();
        let member_id = member_verifying_key.into();

        // Start with the member present in both member_info and members list
        let member_info = create_test_member_info(member_id);
        let authorized_member_info =
            AuthorizedMemberInfo::new_with_member_key(member_info, &member_signing_key);

        let mut member_info_v1 = MemberInfoV1 {
            member_info: vec![authorized_member_info.clone()],
        };

        // Parent state with member REMOVED (simulates ban/max_members)
        let parent_state = ChatRoomStateV1::default();
        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        // Delta includes member_info for the now-removed member
        let updated_info = MemberInfo::new_public(member_id, 2, "NewNick".to_string());
        let updated_authorized =
            AuthorizedMemberInfo::new_with_member_key(updated_info, &member_signing_key);
        let delta = vec![updated_authorized];

        // Previously this would error; now it should succeed
        let result = member_info_v1.apply_delta(&parent_state, &parameters, &Some(delta));
        assert!(
            result.is_ok(),
            "apply_delta should skip removed member's info, got: {:?}",
            result.err()
        );

        // The removed member's info should be cleaned up by retention
        assert!(
            !member_info_v1
                .member_info
                .iter()
                .any(|info| info.member_info.member_id == member_id),
            "Removed member's info should be pruned"
        );

        // Owner info (if any) should be unaffected
        let owner_info = create_test_member_info(owner_id);
        let authorized_owner = AuthorizedMemberInfo::new(owner_info, &owner_signing_key);
        member_info_v1.member_info.push(authorized_owner.clone());

        let result = member_info_v1.apply_delta(&parent_state, &parameters, &None);
        assert!(result.is_ok());
        assert_eq!(member_info_v1.member_info.len(), 1);
        assert_eq!(
            member_info_v1.member_info[0].member_info.member_id,
            owner_id
        );
    }

    /// #411 round 7 / Codex P1 #3: a state carrying TWO validly-signed
    /// `member_info` records for the SAME member must be rejected by `verify`.
    /// Otherwise `deputies_of` (one record) and `summarize`/`apply_delta` (LWW)
    /// disagree on that member's deputies, diverging ban authority from
    /// anti-entropy.
    #[test]
    fn verify_rejects_duplicate_member_info() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = owner_signing_key.verifying_key();
        let owner_id = owner_verifying_key.into();

        let member_signing_key = SigningKey::generate(&mut OsRng);
        let member_verifying_key = member_signing_key.verifying_key();
        let member_id = member_verifying_key.into();

        // Two DIFFERENT but individually-valid records for the same member
        // (distinct versions → distinct signatures → both self-signed).
        let info_a = AuthorizedMemberInfo::new_with_member_key(
            MemberInfo::new_public(member_id, 1, "NickA".to_string()),
            &member_signing_key,
        );
        let info_b = AuthorizedMemberInfo::new_with_member_key(
            MemberInfo::new_public(member_id, 2, "NickB".to_string()),
            &member_signing_key,
        );

        let member_info_v1 = MemberInfoV1 {
            member_info: vec![info_a, info_b],
        };

        let mut parent_state = ChatRoomStateV1::default();
        parent_state.members.members.push(AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: member_verifying_key,
            },
            &owner_signing_key,
        ));

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        let result = member_info_v1.verify(&parent_state, &parameters);
        assert!(
            result.is_err(),
            "verify must reject duplicate member_info for one member"
        );
        assert!(
            result.unwrap_err().contains("Duplicate member_info"),
            "error should name the duplicate"
        );

        // Sanity: a single record for that member still verifies.
        let single = MemberInfoV1 {
            member_info: vec![AuthorizedMemberInfo::new_with_member_key(
                MemberInfo::new_public(member_id, 2, "NickB".to_string()),
                &member_signing_key,
            )],
        };
        assert!(
            single.verify(&parent_state, &parameters).is_ok(),
            "a single member_info record must still verify"
        );
    }

    /// #411 round 7 / Codex P1 #3: when two records for one member are present,
    /// `deputies_of` must return the HIGHEST-rank record's deputies (higher
    /// version, else greater signature) — the same winner `apply_delta` /
    /// `summarize` converge on — regardless of vector order. A bare `.find()`
    /// (first) could disagree with the converged state.
    #[test]
    fn deputies_of_picks_highest_rank_record() {
        let member_signing_key = SigningKey::generate(&mut OsRng);
        let member_id = member_signing_key.verifying_key().into();

        let deputy_x = SigningKey::generate(&mut OsRng).verifying_key().into();
        let deputy_y = SigningKey::generate(&mut OsRng).verifying_key().into();

        // Lower-version record lists deputy_x; higher-version lists deputy_y.
        let mut mi_v1 = MemberInfo::new_public(member_id, 1, "nick".to_string());
        mi_v1.deputies = vec![deputy_x];
        let low = AuthorizedMemberInfo::new_with_member_key(mi_v1, &member_signing_key);

        let mut mi_v2 = MemberInfo::new_public(member_id, 2, "nick".to_string());
        mi_v2.deputies = vec![deputy_y];
        let high = AuthorizedMemberInfo::new_with_member_key(mi_v2, &member_signing_key);

        // Put the LOWER-rank record FIRST so a naive `.find()` would pick it.
        let member_info_v1 = MemberInfoV1 {
            member_info: vec![low, high],
        };

        assert_eq!(
            member_info_v1.deputies_of(member_id),
            &[deputy_y],
            "deputies_of must return the highest-version record's deputies"
        );

        // Also robust to the reverse vector order.
        let mut mi_v1b = MemberInfo::new_public(member_id, 1, "nick".to_string());
        mi_v1b.deputies = vec![deputy_x];
        let low_b = AuthorizedMemberInfo::new_with_member_key(mi_v1b, &member_signing_key);
        let mut mi_v2b = MemberInfo::new_public(member_id, 2, "nick".to_string());
        mi_v2b.deputies = vec![deputy_y];
        let high_b = AuthorizedMemberInfo::new_with_member_key(mi_v2b, &member_signing_key);
        let reversed = MemberInfoV1 {
            member_info: vec![high_b, low_b],
        };
        assert_eq!(
            reversed.deputies_of(member_id),
            &[deputy_y],
            "deputies_of result must be independent of vector order"
        );
    }
}
