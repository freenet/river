use crate::room_state::member::MemberId;
use crate::room_state::privacy::{RoomCipherSpec, SecretVersion};
use crate::room_state::ChatRoomParametersV1;
use crate::util::{sign_struct, verify_struct};
use crate::ChatRoomStateV1;
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use freenet_scaffold::ComposableState;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

/// Room secrets state managing encrypted secret distribution
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct RoomSecretsV1 {
    pub current_version: SecretVersion,
    pub versions: Vec<AuthorizedSecretVersionRecord>,
    pub encrypted_secrets: Vec<AuthorizedEncryptedSecretForMember>,
}

impl ComposableState for RoomSecretsV1 {
    type ParentState = ChatRoomStateV1;
    type Summary = SecretsSummary;
    type Delta = SecretsDelta;
    type Parameters = ChatRoomParametersV1;

    fn verify(
        &self,
        _parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
    ) -> Result<(), String> {
        // Verify all secret version records are signed by owner
        for version_record in &self.versions {
            version_record
                .verify_signature(&parameters.owner)
                .map_err(|e| format!("Invalid version record signature: {}", e))?;
        }

        // Verify all encrypted secrets are signed by owner
        for encrypted_secret in &self.encrypted_secrets {
            encrypted_secret
                .verify_signature(&parameters.owner)
                .map_err(|e| format!("Invalid encrypted secret signature: {}", e))?;
        }

        // Verify current_version matches the maximum version in versions
        if let Some(max_version) = self.versions.iter().map(|v| v.record.version).max() {
            if self.current_version != max_version {
                return Err(format!(
                    "Current version {} does not match maximum version {}",
                    self.current_version, max_version
                ));
            }
        } else if self.current_version != 0 {
            return Err("Current version is non-zero but no version records exist".to_string());
        }

        Ok(())
    }

    fn summarize(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
    ) -> Self::Summary {
        let version_ids: HashSet<SecretVersion> =
            self.versions.iter().map(|v| v.record.version).collect();

        let member_secrets: HashSet<(SecretVersion, MemberId)> = self
            .encrypted_secrets
            .iter()
            .map(|s| (s.secret.secret_version, s.secret.member_id))
            .collect();

        SecretsSummary {
            current_version: self.current_version,
            version_ids,
            member_secrets,
        }
    }

    fn delta(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
        old_state_summary: &Self::Summary,
    ) -> Option<Self::Delta> {
        let new_versions: Vec<AuthorizedSecretVersionRecord> = self
            .versions
            .iter()
            .filter(|v| !old_state_summary.version_ids.contains(&v.record.version))
            .cloned()
            .collect();

        let new_encrypted_secrets: Vec<AuthorizedEncryptedSecretForMember> = self
            .encrypted_secrets
            .iter()
            .filter(|s| {
                !old_state_summary
                    .member_secrets
                    .contains(&(s.secret.secret_version, s.secret.member_id))
            })
            .cloned()
            .collect();

        if new_versions.is_empty()
            && new_encrypted_secrets.is_empty()
            && self.current_version == old_state_summary.current_version
        {
            None
        } else {
            Some(SecretsDelta {
                current_version: if self.current_version > old_state_summary.current_version {
                    Some(self.current_version)
                } else {
                    None
                },
                new_versions,
                new_encrypted_secrets,
            })
        }
    }

    fn apply_delta(
        &mut self,
        parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
        delta: &Option<Self::Delta>,
    ) -> Result<(), String> {
        // Transactional: validate and stage all changes against a working
        // copy of `self`. Only commit (`*self = working`) if every check
        // passes. Bug #3 PR A — previously, a failing sub-check after
        // `versions.push(...)` left `self` half-mutated: `versions` had
        // gained a record, but `current_version` / `encrypted_secrets` /
        // post-prune cleanup never ran, and `recent_messages` (later in the
        // composable `apply_delta`) was skipped entirely by the `?`
        // short-circuit. That partial state then survived as the new
        // baseline, silently corrupting the room and breaking CRDT
        // convergence. Building a working copy and only committing on
        // success makes apply_delta all-or-nothing.
        let mut working = self.clone();

        if let Some(delta) = delta {
            // Verify and stage new version records
            for version_record in &delta.new_versions {
                version_record
                    .verify_signature(&parameters.owner)
                    .map_err(|e| format!("Invalid version record signature in delta: {}", e))?;

                // Check for duplicate version
                if working
                    .versions
                    .iter()
                    .any(|v| v.record.version == version_record.record.version)
                {
                    return Err(format!(
                        "Duplicate secret version: {}",
                        version_record.record.version
                    ));
                }

                working.versions.push(version_record.clone());
            }

            // Verify and stage new encrypted secrets
            let members_by_id = parent_state.members.members_by_member_id();
            for encrypted_secret in &delta.new_encrypted_secrets {
                encrypted_secret
                    .verify_signature(&parameters.owner)
                    .map_err(|e| format!("Invalid encrypted secret signature in delta: {}", e))?;

                let member_id = encrypted_secret.secret.member_id;

                // Skip secrets for removed members — they'll be pruned below.
                if member_id != parameters.owner_id() && !members_by_id.contains_key(&member_id) {
                    continue;
                }

                // Verify secret version exists (in the staged working copy,
                // so a same-delta new_versions + new_encrypted_secrets pair
                // resolves correctly).
                if !working
                    .versions
                    .iter()
                    .any(|v| v.record.version == encrypted_secret.secret.secret_version)
                {
                    return Err(format!(
                        "Encrypted secret references non-existent version: {}",
                        encrypted_secret.secret.secret_version
                    ));
                }

                // Check for duplicate (version, member_id) pair
                if working.encrypted_secrets.iter().any(|s| {
                    s.secret.secret_version == encrypted_secret.secret.secret_version
                        && s.secret.member_id == member_id
                }) {
                    return Err(format!(
                        "Duplicate encrypted secret for member {:?} version {}",
                        member_id, encrypted_secret.secret.secret_version
                    ));
                }

                working.encrypted_secrets.push(encrypted_secret.clone());
            }

            // Update current version if provided
            if let Some(new_version) = delta.current_version {
                if new_version <= working.current_version {
                    return Err(format!(
                        "New current version {} must be greater than existing version {}",
                        new_version, working.current_version
                    ));
                }

                // Verify the new version exists in versions
                if !working
                    .versions
                    .iter()
                    .any(|v| v.record.version == new_version)
                {
                    return Err(format!(
                        "Cannot set current version to non-existent version: {}",
                        new_version
                    ));
                }

                working.current_version = new_version;
            }

            // Prune encrypted secrets for removed members
            let owner_id = parameters.owner_id();
            working.encrypted_secrets.retain(|s| {
                s.secret.member_id == owner_id || members_by_id.contains_key(&s.secret.member_id)
            });
        }

        // Sort for deterministic ordering (CRDT convergence requirement)
        working.versions.sort_by_key(|v| v.record.version);
        working
            .encrypted_secrets
            .sort_by_key(|s| (s.secret.secret_version, s.secret.member_id));

        // Commit: every check passed, so move the working copy into self.
        *self = working;
        Ok(())
    }
}

/// Summary of room secrets state for delta calculation
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SecretsSummary {
    pub current_version: SecretVersion,
    pub version_ids: HashSet<SecretVersion>,
    pub member_secrets: HashSet<(SecretVersion, MemberId)>,
}

/// Delta for room secrets state
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SecretsDelta {
    pub current_version: Option<SecretVersion>,
    pub new_versions: Vec<AuthorizedSecretVersionRecord>,
    pub new_encrypted_secrets: Vec<AuthorizedEncryptedSecretForMember>,
}

/// Metadata about a secret version
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct SecretVersionRecordV1 {
    pub version: SecretVersion,
    pub cipher_spec: RoomCipherSpec,
    pub created_at: SystemTime,
}

/// Authorized secret version record signed by room owner
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct AuthorizedSecretVersionRecord {
    pub record: SecretVersionRecordV1,
    pub owner_signature: Signature,
}

impl AuthorizedSecretVersionRecord {
    pub fn new(record: SecretVersionRecordV1, owner_signing_key: &SigningKey) -> Self {
        let signature = sign_struct(&record, owner_signing_key);
        Self {
            record,
            owner_signature: signature,
        }
    }

    /// Create an AuthorizedSecretVersionRecord with a pre-computed signature.
    /// Use this when signing is done externally (e.g., via delegate).
    pub fn with_signature(record: SecretVersionRecordV1, owner_signature: Signature) -> Self {
        Self {
            record,
            owner_signature,
        }
    }

    pub fn verify_signature(&self, owner_verifying_key: &VerifyingKey) -> Result<(), String> {
        verify_struct(&self.record, &self.owner_signature, owner_verifying_key)
            .map_err(|e| format!("Invalid signature: {}", e))
    }
}

/// Encrypted secret blob for a specific member
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct EncryptedSecretForMemberV1 {
    pub member_id: MemberId,
    pub secret_version: SecretVersion,
    pub ciphertext: Vec<u8>,
    pub nonce: [u8; 12],
    pub sender_ephemeral_public_key: [u8; 32],
    pub provider: MemberId,
}

/// Authorized encrypted secret signed by room owner
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct AuthorizedEncryptedSecretForMember {
    pub secret: EncryptedSecretForMemberV1,
    pub owner_signature: Signature,
}

impl AuthorizedEncryptedSecretForMember {
    pub fn new(secret: EncryptedSecretForMemberV1, owner_signing_key: &SigningKey) -> Self {
        let signature = sign_struct(&secret, owner_signing_key);
        Self {
            secret,
            owner_signature: signature,
        }
    }

    /// Create an AuthorizedEncryptedSecretForMember with a pre-computed signature.
    /// Use this when signing is done externally (e.g., via delegate).
    pub fn with_signature(secret: EncryptedSecretForMemberV1, owner_signature: Signature) -> Self {
        Self {
            secret,
            owner_signature,
        }
    }

    pub fn verify_signature(&self, owner_verifying_key: &VerifyingKey) -> Result<(), String> {
        verify_struct(&self.secret, &self.owner_signature, owner_verifying_key)
            .map_err(|e| format!("Invalid signature: {}", e))
    }
}

/// Build the list of `AuthorizedEncryptedSecretForMember` records to emit
/// in a rotation update.
///
/// For each current member (owner + each in `current_members_with_vks`),
/// for each version `v` in `[0..=new_version]`:
/// * If `(member, v)` is already in `existing_encrypted_secrets`, skip —
///   the room state already has that pair and emitting it again would be
///   rejected by `RoomSecretsV1::apply_delta`'s duplicate guard, wedging
///   rotation permanently.
/// * Otherwise, emit a fresh `AuthorizedEncryptedSecretForMember` that
///   encrypts the per-version secret for the member's VK.
///
/// Per-version secrets are sourced as follows:
/// * `new_version` → `new_secret` (the value the caller just derived).
/// * Any prior `v < new_version` → RECOVERED by ECIES-decrypting the
///   owner's existing `encrypted_secret`-at-v using the owner's signing
///   key. The owner has the signing key, so they can decrypt the blob
///   they originally produced for themselves and recover the actual
///   secret bytes the room is really using. We do NOT re-derive via
///   `derive_room_secret`: River's UI generates v0 randomly at room
///   creation (`ui/src/room_data.rs:create_new_room_with_name`), so a
///   derived v0 would not match what was sealed under the actual v0.
///
/// If a prior version's secret can't be recovered (no owner blob at
/// that version, or decrypt fails), entries at that version are
/// skipped. The newly-joined member won't decrypt content sealed at
/// that version, but nobody else can either — the data is irrecoverable.
///
/// Determining continuing-vs-newly-joined per `(member, version)`
/// directly from `existing_encrypted_secrets` (rather than from a
/// caller-local cache) is deliberate: the local cache can be missing
/// (fresh delegate, restart, webapp reinstall), and using it as the
/// dedup source would produce duplicate `(member, version)` pairs that
/// the contract rejects.
///
/// Pure function, no I/O — extracted so the UI's synchronous
/// `rotate_secret` fast-path and the chat-delegate's asynchronous
/// rotation pipeline produce byte-identical blob sets for the same
/// inputs. See issue #271 and Bug #3 PR B (Ivvor 2026-05-17).
#[cfg(feature = "ecies")]
#[allow(clippy::too_many_arguments)]
pub fn build_rotation_encrypted_secrets(
    signing_key: &SigningKey,
    owner_vk: &VerifyingKey,
    owner_id: MemberId,
    new_version: SecretVersion,
    new_secret: &[u8; 32],
    current_members_with_vks: &[(MemberId, VerifyingKey)],
    existing_encrypted_secrets: &[AuthorizedEncryptedSecretForMember],
) -> Result<Vec<AuthorizedEncryptedSecretForMember>, String> {
    use crate::ecies::{decrypt_secret_from_member_blob_raw, encrypt_secret_for_member};
    use std::collections::{BTreeMap, BTreeSet};

    // What's already on the wire — never re-emit any of these.
    let existing: BTreeSet<(MemberId, SecretVersion)> = existing_encrypted_secrets
        .iter()
        .map(|s| (s.secret.member_id, s.secret.secret_version))
        .collect();

    // Recover prior-version secrets by decrypting the owner's existing
    // blobs. If decrypt fails (malformed blob, unexpected sender) we just
    // skip — defensive, shouldn't happen on well-formed state.
    let mut prior_secrets: BTreeMap<SecretVersion, [u8; 32]> = BTreeMap::new();
    for blob in existing_encrypted_secrets {
        if blob.secret.member_id != owner_id {
            continue;
        }
        if blob.secret.secret_version >= new_version {
            continue;
        }
        if prior_secrets.contains_key(&blob.secret.secret_version) {
            // First-wins. Should not happen — contract dedups
            // (member, version) — but be defensive.
            //
            // Surface a warning when this actually fires in practice
            // so we can investigate. Using `eprintln!` because
            // river-core has no logging dependency (intentionally,
            // to keep the room-contract WASM small); this is a
            // defensive log so a no-op in WASM is acceptable, and
            // native tests / native delegate builds will still show
            // it. See IMPORTANT #7 on PR #272 review round 2.
            eprintln!(
                "warn(build_rotation_encrypted_secrets): duplicate owner blob \
                 at version {} (first-wins applied); contract should have \
                 dedup'd (member, version) — investigate",
                blob.secret.secret_version
            );
            continue;
        }
        if let Ok(s) = decrypt_secret_from_member_blob_raw(
            &blob.secret.ciphertext,
            &blob.secret.nonce,
            &blob.secret.sender_ephemeral_public_key,
            signing_key,
        ) {
            prior_secrets.insert(blob.secret.secret_version, s);
        }
    }
    // The new version's secret is whatever the caller just derived.
    prior_secrets.insert(new_version, *new_secret);

    let mut out: Vec<AuthorizedEncryptedSecretForMember> = Vec::new();

    // Owner + every current member.
    let all_members =
        std::iter::once((owner_id, *owner_vk)).chain(current_members_with_vks.iter().copied());

    // Iterate the versions we actually have secrets for (not the full
    // numeric range `0..=new_version`). Secret versions are NOT required
    // to be contiguous — `RoomSecretsV1::apply_delta` only enforces
    // monotonicity of `current_version`, so a valid owner-signed state
    // could legitimately jump from v0 to v1_000_000_000, and the next
    // rotation would otherwise loop a billion times per member checking
    // versions that have no recoverable secret. See Codex review of
    // PR #272 (third pass).
    for (member_id, member_vk) in all_members {
        for (&v, secret_for_version) in &prior_secrets {
            if existing.contains(&(member_id, v)) {
                continue;
            }
            let (ciphertext, nonce, ephemeral_key) =
                encrypt_secret_for_member(secret_for_version, &member_vk);
            let secret_struct = EncryptedSecretForMemberV1 {
                member_id,
                secret_version: v,
                ciphertext,
                nonce,
                sender_ephemeral_public_key: ephemeral_key.to_bytes(),
                provider: owner_id,
            };
            out.push(AuthorizedEncryptedSecretForMember::new(
                secret_struct,
                signing_key,
            ));
        }
    }

    Ok(out)
}

impl RoomSecretsV1 {
    /// Check if all current members have encrypted blobs for the current version
    pub fn has_complete_distribution(
        &self,
        members: &HashMap<MemberId, &crate::room_state::member::AuthorizedMember>,
    ) -> bool {
        if self.current_version == 0 {
            return true; // No secrets yet
        }

        let member_ids_with_current: HashSet<MemberId> = self
            .encrypted_secrets
            .iter()
            .filter(|s| s.secret.secret_version == self.current_version)
            .map(|s| s.secret.member_id)
            .collect();

        members
            .keys()
            .all(|id| member_ids_with_current.contains(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room_state::member::{AuthorizedMember, Member};
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    fn create_test_state_and_params() -> (ChatRoomStateV1, ChatRoomParametersV1, SigningKey) {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = owner_signing_key.verifying_key();

        let state = ChatRoomStateV1::default();
        let params = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        (state, params, owner_signing_key)
    }

    fn create_version_record(
        version: SecretVersion,
        owner_sk: &SigningKey,
    ) -> AuthorizedSecretVersionRecord {
        let record = SecretVersionRecordV1 {
            version,
            cipher_spec: RoomCipherSpec::Aes256Gcm,
            created_at: SystemTime::now(),
        };
        AuthorizedSecretVersionRecord::new(record, owner_sk)
    }

    fn create_encrypted_secret(
        member_id: MemberId,
        version: SecretVersion,
        owner_sk: &SigningKey,
    ) -> AuthorizedEncryptedSecretForMember {
        let secret = EncryptedSecretForMemberV1 {
            member_id,
            secret_version: version,
            ciphertext: vec![1, 2, 3, 4],
            nonce: [0u8; 12],
            sender_ephemeral_public_key: [0u8; 32],
            provider: member_id,
        };
        AuthorizedEncryptedSecretForMember::new(secret, owner_sk)
    }

    #[test]
    fn test_room_secrets_v1_default() {
        let secrets = RoomSecretsV1::default();
        assert_eq!(secrets.current_version, 0);
        assert!(secrets.versions.is_empty());
        assert!(secrets.encrypted_secrets.is_empty());
    }

    #[test]
    fn test_authorized_secret_version_record() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = owner_signing_key.verifying_key();

        let record = SecretVersionRecordV1 {
            version: 1,
            cipher_spec: RoomCipherSpec::Aes256Gcm,
            created_at: SystemTime::now(),
        };

        let authorized_record =
            AuthorizedSecretVersionRecord::new(record.clone(), &owner_signing_key);

        assert_eq!(authorized_record.record, record);
        assert!(authorized_record
            .verify_signature(&owner_verifying_key)
            .is_ok());

        // Test with wrong key
        let wrong_key = SigningKey::generate(&mut OsRng).verifying_key();
        assert!(authorized_record.verify_signature(&wrong_key).is_err());
    }

    #[test]
    fn test_authorized_encrypted_secret_for_member() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = owner_signing_key.verifying_key();
        let member_id = MemberId::from(&owner_verifying_key);

        let secret = EncryptedSecretForMemberV1 {
            member_id,
            secret_version: 1,
            ciphertext: vec![1, 2, 3, 4],
            nonce: [0u8; 12],
            sender_ephemeral_public_key: [0u8; 32],
            provider: member_id,
        };

        let authorized_secret =
            AuthorizedEncryptedSecretForMember::new(secret.clone(), &owner_signing_key);

        assert_eq!(authorized_secret.secret, secret);
        assert!(authorized_secret
            .verify_signature(&owner_verifying_key)
            .is_ok());

        // Test with wrong key
        let wrong_key = SigningKey::generate(&mut OsRng).verifying_key();
        assert!(authorized_secret.verify_signature(&wrong_key).is_err());
    }

    // ============================================================================
    // COMPREHENSIVE COMPOSABLESTATE TESTS
    // ============================================================================

    #[test]
    fn test_verify_empty_state() {
        let (state, params, _) = create_test_state_and_params();
        let secrets = RoomSecretsV1::default();

        assert!(secrets.verify(&state, &params).is_ok());
    }

    #[test]
    fn test_verify_valid_state_with_version() {
        let (state, params, owner_sk) = create_test_state_and_params();
        let owner_id = params.owner_id();

        let mut secrets = RoomSecretsV1 {
            current_version: 1,
            ..Default::default()
        };
        secrets.versions.push(create_version_record(1, &owner_sk));
        secrets
            .encrypted_secrets
            .push(create_encrypted_secret(owner_id, 1, &owner_sk));

        assert!(secrets.verify(&state, &params).is_ok());
    }

    #[test]
    fn test_verify_fails_with_invalid_version_signature() {
        let (state, params, _owner_sk) = create_test_state_and_params();
        let wrong_sk = SigningKey::generate(&mut OsRng);

        let mut secrets = RoomSecretsV1 {
            current_version: 1,
            ..Default::default()
        };
        secrets.versions.push(create_version_record(1, &wrong_sk)); // Wrong signature!

        let result = secrets.verify(&state, &params);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("Invalid version record signature"));
    }

    #[test]
    fn test_verify_fails_with_invalid_secret_signature() {
        let (state, params, owner_sk) = create_test_state_and_params();
        let owner_id = params.owner_id();
        let wrong_sk = SigningKey::generate(&mut OsRng);

        let mut secrets = RoomSecretsV1 {
            current_version: 1,
            ..Default::default()
        };
        secrets.versions.push(create_version_record(1, &owner_sk));
        secrets
            .encrypted_secrets
            .push(create_encrypted_secret(owner_id, 1, &wrong_sk)); // Wrong signature!

        let result = secrets.verify(&state, &params);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("Invalid encrypted secret signature"));
    }

    #[test]
    fn test_verify_fails_with_mismatched_current_version() {
        let (state, params, owner_sk) = create_test_state_and_params();

        let mut secrets = RoomSecretsV1 {
            current_version: 2,
            ..Default::default()
        }; // Mismatch!
        secrets.versions.push(create_version_record(1, &owner_sk));

        let result = secrets.verify(&state, &params);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("does not match maximum version"));
    }

    #[test]
    fn test_verify_fails_with_nonzero_current_but_no_versions() {
        let (state, params, _) = create_test_state_and_params();

        let secrets = RoomSecretsV1 {
            current_version: 1,
            ..Default::default()
        };
        // No versions!

        let result = secrets.verify(&state, &params);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no version records exist"));
    }

    #[test]
    fn test_summarize_empty_state() {
        let (state, params, _) = create_test_state_and_params();
        let secrets = RoomSecretsV1::default();

        let summary = secrets.summarize(&state, &params);
        assert_eq!(summary.current_version, 0);
        assert!(summary.version_ids.is_empty());
        assert!(summary.member_secrets.is_empty());
    }

    #[test]
    fn test_summarize_with_data() {
        let (state, params, owner_sk) = create_test_state_and_params();
        let owner_id = params.owner_id();

        let mut secrets = RoomSecretsV1 {
            current_version: 2,
            ..Default::default()
        };
        secrets.versions.push(create_version_record(1, &owner_sk));
        secrets.versions.push(create_version_record(2, &owner_sk));
        secrets
            .encrypted_secrets
            .push(create_encrypted_secret(owner_id, 1, &owner_sk));
        secrets
            .encrypted_secrets
            .push(create_encrypted_secret(owner_id, 2, &owner_sk));

        let summary = secrets.summarize(&state, &params);
        assert_eq!(summary.current_version, 2);
        assert_eq!(summary.version_ids.len(), 2);
        assert!(summary.version_ids.contains(&1));
        assert!(summary.version_ids.contains(&2));
        assert_eq!(summary.member_secrets.len(), 2);
        assert!(summary.member_secrets.contains(&(1, owner_id)));
        assert!(summary.member_secrets.contains(&(2, owner_id)));
    }

    #[test]
    fn test_delta_no_changes() {
        let (state, params, _) = create_test_state_and_params();
        let secrets = RoomSecretsV1::default();
        let summary = secrets.summarize(&state, &params);

        let delta = secrets.delta(&state, &params, &summary);
        assert!(delta.is_none());
    }

    #[test]
    fn test_delta_new_version() {
        let (state, params, owner_sk) = create_test_state_and_params();
        let owner_id = params.owner_id();

        let mut secrets = RoomSecretsV1 {
            current_version: 1,
            ..Default::default()
        };
        secrets.versions.push(create_version_record(1, &owner_sk));
        secrets
            .encrypted_secrets
            .push(create_encrypted_secret(owner_id, 1, &owner_sk));

        let old_summary = SecretsSummary {
            current_version: 0,
            version_ids: HashSet::new(),
            member_secrets: HashSet::new(),
        };

        let delta = secrets.delta(&state, &params, &old_summary).unwrap();
        assert_eq!(delta.current_version, Some(1));
        assert_eq!(delta.new_versions.len(), 1);
        assert_eq!(delta.new_encrypted_secrets.len(), 1);
    }

    #[test]
    fn test_delta_partial_update() {
        let (state, params, owner_sk) = create_test_state_and_params();
        let owner_id = params.owner_id();

        let mut secrets = RoomSecretsV1 {
            current_version: 2,
            ..Default::default()
        };
        secrets.versions.push(create_version_record(1, &owner_sk));
        secrets.versions.push(create_version_record(2, &owner_sk));
        secrets
            .encrypted_secrets
            .push(create_encrypted_secret(owner_id, 1, &owner_sk));
        secrets
            .encrypted_secrets
            .push(create_encrypted_secret(owner_id, 2, &owner_sk));

        let mut old_summary = SecretsSummary {
            current_version: 1,
            version_ids: HashSet::new(),
            member_secrets: HashSet::new(),
        };
        old_summary.version_ids.insert(1);
        old_summary.member_secrets.insert((1, owner_id));

        let delta = secrets.delta(&state, &params, &old_summary).unwrap();
        assert_eq!(delta.current_version, Some(2));
        assert_eq!(delta.new_versions.len(), 1);
        assert_eq!(delta.new_versions[0].record.version, 2);
        assert_eq!(delta.new_encrypted_secrets.len(), 1);
        assert_eq!(delta.new_encrypted_secrets[0].secret.secret_version, 2);
    }

    #[test]
    fn test_apply_delta_add_first_version() {
        let (state, params, owner_sk) = create_test_state_and_params();
        let owner_id = params.owner_id();

        let mut secrets = RoomSecretsV1::default();

        let delta = SecretsDelta {
            current_version: Some(1),
            new_versions: vec![create_version_record(1, &owner_sk)],
            new_encrypted_secrets: vec![create_encrypted_secret(owner_id, 1, &owner_sk)],
        };

        let result = secrets.apply_delta(&state, &params, &Some(delta));
        assert!(result.is_ok(), "Failed: {:?}", result.err());
        assert_eq!(secrets.current_version, 1);
        assert_eq!(secrets.versions.len(), 1);
        assert_eq!(secrets.encrypted_secrets.len(), 1);
    }

    #[test]
    fn test_apply_delta_rejects_duplicate_version() {
        let (state, params, owner_sk) = create_test_state_and_params();

        let mut secrets = RoomSecretsV1 {
            current_version: 1,
            ..Default::default()
        };
        secrets.versions.push(create_version_record(1, &owner_sk));

        let delta = SecretsDelta {
            current_version: None,
            new_versions: vec![create_version_record(1, &owner_sk)], // Duplicate!
            new_encrypted_secrets: vec![],
        };

        let result = secrets.apply_delta(&state, &params, &Some(delta));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Duplicate secret version"));
    }

    #[test]
    fn test_apply_delta_skips_secret_for_nonexistent_member() {
        let (state, params, owner_sk) = create_test_state_and_params();
        let fake_member_id = MemberId::from(&SigningKey::generate(&mut OsRng).verifying_key());

        let mut secrets = RoomSecretsV1 {
            current_version: 1,
            ..Default::default()
        };
        secrets.versions.push(create_version_record(1, &owner_sk));

        let delta = SecretsDelta {
            current_version: None,
            new_versions: vec![],
            new_encrypted_secrets: vec![create_encrypted_secret(fake_member_id, 1, &owner_sk)],
        };

        // Should succeed — secret for removed member is silently skipped
        let result = secrets.apply_delta(&state, &params, &Some(delta));
        assert!(
            result.is_ok(),
            "Should skip non-existent member, got: {:?}",
            result.err()
        );
        // The secret should not have been added
        assert!(
            !secrets
                .encrypted_secrets
                .iter()
                .any(|s| s.secret.member_id == fake_member_id),
            "Secret for non-existent member should not be added"
        );
    }

    #[test]
    fn test_apply_delta_rejects_secret_for_nonexistent_version() {
        let (state, params, owner_sk) = create_test_state_and_params();
        let owner_id = params.owner_id();

        let mut secrets = RoomSecretsV1::default();

        let delta = SecretsDelta {
            current_version: None,
            new_versions: vec![],
            new_encrypted_secrets: vec![create_encrypted_secret(owner_id, 99, &owner_sk)], // Version 99 doesn't exist!
        };

        let result = secrets.apply_delta(&state, &params, &Some(delta));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("non-existent version"));
    }

    #[test]
    fn test_apply_delta_rejects_duplicate_member_secret() {
        let (state, params, owner_sk) = create_test_state_and_params();
        let owner_id = params.owner_id();

        let mut secrets = RoomSecretsV1 {
            current_version: 1,
            ..Default::default()
        };
        secrets.versions.push(create_version_record(1, &owner_sk));
        secrets
            .encrypted_secrets
            .push(create_encrypted_secret(owner_id, 1, &owner_sk));

        let delta = SecretsDelta {
            current_version: None,
            new_versions: vec![],
            new_encrypted_secrets: vec![create_encrypted_secret(owner_id, 1, &owner_sk)], // Duplicate!
        };

        let result = secrets.apply_delta(&state, &params, &Some(delta));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Duplicate encrypted secret"));
    }

    #[test]
    fn test_apply_delta_rejects_invalid_version_transition() {
        let (state, params, owner_sk) = create_test_state_and_params();

        let mut secrets = RoomSecretsV1 {
            current_version: 2,
            ..Default::default()
        };
        secrets.versions.push(create_version_record(1, &owner_sk));
        secrets.versions.push(create_version_record(2, &owner_sk));

        let delta = SecretsDelta {
            current_version: Some(1), // Can't go backward!
            new_versions: vec![],
            new_encrypted_secrets: vec![],
        };

        let result = secrets.apply_delta(&state, &params, &Some(delta));
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("must be greater than existing version"));
    }

    #[test]
    fn test_apply_delta_rejects_nonexistent_current_version() {
        let (state, params, _owner_sk) = create_test_state_and_params();

        let mut secrets = RoomSecretsV1::default();

        let delta = SecretsDelta {
            current_version: Some(99), // Version 99 doesn't exist!
            new_versions: vec![],
            new_encrypted_secrets: vec![],
        };

        let result = secrets.apply_delta(&state, &params, &Some(delta));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("non-existent version"));
    }

    #[test]
    fn test_apply_delta_prunes_removed_member_secrets() {
        let (mut state, params, owner_sk) = create_test_state_and_params();
        let owner_id = params.owner_id();

        // Add a member
        let member_sk = SigningKey::generate(&mut OsRng);
        let member_vk = member_sk.verifying_key();
        let member_id = MemberId::from(&member_vk);

        let member = Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk,
        };
        let auth_member = AuthorizedMember::new(member, &owner_sk);
        state.members.members.push(auth_member);

        // Set up secrets with both owner and member
        let mut secrets = RoomSecretsV1 {
            current_version: 1,
            ..Default::default()
        };
        secrets.versions.push(create_version_record(1, &owner_sk));
        secrets
            .encrypted_secrets
            .push(create_encrypted_secret(owner_id, 1, &owner_sk));
        secrets
            .encrypted_secrets
            .push(create_encrypted_secret(member_id, 1, &owner_sk));

        assert_eq!(secrets.encrypted_secrets.len(), 2);

        // Remove the member
        state.members.members.clear();

        // Apply empty delta (triggers pruning)
        let delta = SecretsDelta {
            current_version: None,
            new_versions: vec![],
            new_encrypted_secrets: vec![],
        };

        let result = secrets.apply_delta(&state, &params, &Some(delta));
        assert!(result.is_ok());

        // Member's secret should be pruned, owner's should remain
        assert_eq!(secrets.encrypted_secrets.len(), 1);
        assert_eq!(secrets.encrypted_secrets[0].secret.member_id, owner_id);
    }

    #[test]
    fn test_has_complete_distribution_empty() {
        let secrets = RoomSecretsV1::default();
        let members = HashMap::new();

        assert!(secrets.has_complete_distribution(&members));
    }

    #[test]
    fn test_has_complete_distribution_complete() {
        let (_state, params, owner_sk) = create_test_state_and_params();
        let owner_id = params.owner_id();

        let mut secrets = RoomSecretsV1 {
            current_version: 1,
            ..Default::default()
        };
        secrets.versions.push(create_version_record(1, &owner_sk));
        secrets
            .encrypted_secrets
            .push(create_encrypted_secret(owner_id, 1, &owner_sk));

        let member = Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: params.owner,
        };
        let auth_member = AuthorizedMember::new(member, &owner_sk);

        let mut members = HashMap::new();
        members.insert(owner_id, &auth_member);

        assert!(secrets.has_complete_distribution(&members));
    }

    #[test]
    fn test_has_complete_distribution_incomplete() {
        let (_state, params, owner_sk) = create_test_state_and_params();
        let owner_id = params.owner_id();

        let member_sk = SigningKey::generate(&mut OsRng);
        let member_vk = member_sk.verifying_key();
        let member_id = MemberId::from(&member_vk);

        let mut secrets = RoomSecretsV1 {
            current_version: 1,
            ..Default::default()
        };
        secrets.versions.push(create_version_record(1, &owner_sk));
        secrets
            .encrypted_secrets
            .push(create_encrypted_secret(owner_id, 1, &owner_sk));
        // Missing secret for member_id!

        let member = Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk,
        };
        let auth_member = AuthorizedMember::new(member, &owner_sk);

        let mut members = HashMap::new();
        members.insert(member_id, &auth_member);

        assert!(!secrets.has_complete_distribution(&members));
    }

    /// Regression test: apply_delta should succeed when the delta contains
    /// encrypted secrets for a member that was simultaneously removed from
    /// parent_state.members (e.g. ban or max_members eviction).
    #[test]
    fn test_apply_delta_with_removed_member_secret() {
        let (mut state, params, owner_sk) = create_test_state_and_params();
        let owner_id = params.owner_id();

        // Add a member
        let member_sk = SigningKey::generate(&mut OsRng);
        let member_vk = member_sk.verifying_key();
        let member_id = MemberId::from(&member_vk);

        let member = Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk,
        };
        let auth_member = AuthorizedMember::new(member, &owner_sk);
        state.members.members.push(auth_member);

        // Set up initial secrets with version 1
        let mut secrets = RoomSecretsV1 {
            current_version: 1,
            ..Default::default()
        };
        secrets.versions.push(create_version_record(1, &owner_sk));
        secrets
            .encrypted_secrets
            .push(create_encrypted_secret(owner_id, 1, &owner_sk));
        secrets
            .encrypted_secrets
            .push(create_encrypted_secret(member_id, 1, &owner_sk));

        // Now remove the member (simulates ban)
        state.members.members.clear();

        // Delta includes a new secret version with encrypted secret for removed member
        let delta = SecretsDelta {
            current_version: Some(2),
            new_versions: vec![create_version_record(2, &owner_sk)],
            new_encrypted_secrets: vec![
                create_encrypted_secret(owner_id, 2, &owner_sk),
                create_encrypted_secret(member_id, 2, &owner_sk), // member was removed
            ],
        };

        // Previously this would error; now it should succeed
        let result = secrets.apply_delta(&state, &params, &Some(delta));
        assert!(
            result.is_ok(),
            "apply_delta should skip removed member's secret, got: {:?}",
            result.err()
        );

        // Removed member's secrets should be pruned
        assert!(
            !secrets
                .encrypted_secrets
                .iter()
                .any(|s| s.secret.member_id == member_id),
            "Removed member's secrets should be pruned"
        );

        // Owner's secrets should remain
        assert!(
            secrets
                .encrypted_secrets
                .iter()
                .any(|s| s.secret.member_id == owner_id && s.secret.secret_version == 2),
            "Owner's new secret should be present"
        );
    }
}
