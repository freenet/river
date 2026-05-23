//! CLI-side helpers for private-room secret handling, mirroring the UI's
//! `ui/src/room_data.rs` (`seal_invitee_nickname`,
//! `current_secret_from_state`, the `repopulate`-style decrypt loop) and
//! `ui/src/components/members.rs` (`collect_invitation_secrets`) — see issue
//! freenet/river#302 for the long-term consolidation plan into a shared,
//! non-WASM-compiled crate. Until that lands, the two copies MUST emit
//! byte-identical results so a UI-issued invitation and a CLI-issued
//! invitation are wire-interchangeable.

use ed25519_dalek::SigningKey;
use river_core::ecies::{decrypt_secret_from_member_blob_raw, seal_bytes};
use river_core::room_state::member::MemberId;
use river_core::room_state::privacy::{PrivacyMode, SealedBytes};
use river_core::room_state::ChatRoomStateV1;
use std::collections::HashMap;
use tracing::warn;

/// Collect every room secret this CLI holds for a private room, keyed by
/// `secret_version`. Returns an empty map for a public room.
///
/// Sources, in order of authority:
/// 1. **Owner-signed contract blobs.** Every blob in
///    `state.secrets.encrypted_secrets` addressed to this member is decrypted
///    with `self_sk` and inserted. The owner addresses an
///    `AuthorizedEncryptedSecretForMember` to *every* member, including
///    themselves, so this branch works uniformly for owners and non-owners —
///    we do NOT special-case `is_owner`. (An earlier draft of this function
///    derived owner secrets via [`river_core::key_derivation::derive_room_secret`],
///    but the UI's room-creation path seeds v0 from
///    `river_core::ecies::generate_room_secret()` — a random value — NOT from
///    derivation. Deriving here would produce the wrong v0 for any room that
///    was created via the UI, then later inherited by a CLI owner. The
///    deterministic derivation in `derive_room_secret` is used only by the
///    *rotation* paths, where its convergence property is needed; the initial
///    secret has no such requirement.)
/// 2. **Persisted `invitation_secrets`.** Secrets carried in via prior
///    `Invitation` artifacts (issue freenet/river#302) for versions the
///    contract has not yet provided an owner-signed blob for. Folded in
///    second so an owner-signed blob takes precedence on the same version —
///    mirrors `RoomData::repopulate_secrets_from_state` in the UI.
pub fn collect_secrets_for_room(
    state: &ChatRoomStateV1,
    self_sk: &SigningKey,
    invitation_secrets: &HashMap<u32, [u8; 32]>,
) -> HashMap<u32, [u8; 32]> {
    if state.configuration.configuration.privacy_mode != PrivacyMode::Private {
        return HashMap::new();
    }

    let mut secrets: HashMap<u32, [u8; 32]> = HashMap::new();

    // Decrypt every contract blob addressed to this member — owner-signed,
    // so authoritative. Surfaces a decrypt failure as a `warn!` rather than
    // a silent swallow: a decrypt failure at this branch means the
    // sender_ephemeral / member_id pairing didn't match `self_sk`, which
    // would indicate a real key-mismatch bug or contract-state corruption.
    let self_id = MemberId::from(&self_sk.verifying_key());
    for blob in state
        .secrets
        .encrypted_secrets
        .iter()
        .filter(|s| s.secret.member_id == self_id)
    {
        match decrypt_secret_from_member_blob_raw(
            &blob.secret.ciphertext,
            &blob.secret.nonce,
            &blob.secret.sender_ephemeral_public_key,
            self_sk,
        ) {
            Ok(secret) => {
                secrets.insert(blob.secret.secret_version, secret);
            }
            Err(e) => {
                warn!(
                    "Failed to decrypt owner-signed secret for self at v{}: {} \
                     (likely a key-mismatch bug — the blob is addressed to this \
                     member but the ECIES envelope did not decrypt under self_sk)",
                    blob.secret.secret_version, e
                );
            }
        }
    }

    for (&version, secret) in invitation_secrets {
        secrets.entry(version).or_insert(*secret);
    }

    secrets
}

/// Collect a secrets map into the wire-format `Vec<(version, secret)>` carried
/// by `Invitation::room_secrets`, sorted ascending by version so the encoded
/// invitation is deterministic — the encoded string is fingerprinted for
/// processed-invite dedup, so it must be stable across decode/re-encode
/// cycles. Mirrors UI's `collect_invitation_secrets` in
/// `ui/src/components/members.rs` — keep the names in step.
pub fn collect_invitation_secrets(secrets: &HashMap<u32, [u8; 32]>) -> Vec<(u32, [u8; 32])> {
    let mut out: Vec<(u32, [u8; 32])> = secrets.iter().map(|(&v, &s)| (v, s)).collect();
    out.sort_unstable_by_key(|(v, _)| *v);
    out
}

/// Merge a freshly-accepted `Invitation`'s `room_secrets` into the CLI's
/// previously-persisted `invitation_secrets` map for the same room.
///
/// New invitation entries WIN on version collision — matches the UI's
/// `extend()` semantics at
/// `ui/src/components/app/freenet_api/response_handler/get_response.rs`,
/// where "`extend` covers both the freshly-inserted entry and a pre-existing
/// one (a re-accepted invite)". Pre-existing entries the new invitation
/// does NOT carry are preserved — a re-accept of an older invitation must
/// NOT drop newer versions the CLI already holds (skeptical-review-round-2
/// finding H1 on PR #303).
pub fn merge_invitation_secrets(
    mut existing: HashMap<u32, [u8; 32]>,
    incoming: &[(u32, [u8; 32])],
) -> HashMap<u32, [u8; 32]> {
    for (v, s) in incoming.iter().copied() {
        existing.insert(v, s);
    }
    existing
}

/// Compute the `SealedBytes` for an invitee's chosen nickname at join time.
///
/// For a public room → `Some(SealedBytes::public(...))`.
///
/// For a private room → prefer the secret from the freshly-fetched network
/// `state` (owner-signed, authoritative); fall back to a secret supplied by
/// the invitation artifact (issue freenet/river#302), so a brand-new invitee
/// can seal at join WITHOUT waiting for the chat-delegate back-fill. Returns
/// `None` for a private room when no secret is available at the room's
/// `current_secret_version` from either source — the caller then defers
/// `member_info` rather than leaking a plaintext nickname into a private
/// room. Mirrors the UI's `seal_invitee_nickname` in `ui/src/room_data.rs`.
pub fn seal_invitee_nickname(
    state: &ChatRoomStateV1,
    self_sk: &SigningKey,
    invitation_secrets: &HashMap<u32, [u8; 32]>,
    preferred_nickname: &str,
) -> Option<SealedBytes> {
    if state.configuration.configuration.privacy_mode != PrivacyMode::Private {
        return Some(SealedBytes::public(preferred_nickname.as_bytes().to_vec()));
    }
    let (secret, version) = current_secret_from_state(state, self_sk).or_else(|| {
        let version = state.secrets.current_version;
        invitation_secrets
            .get(&version)
            .map(|secret| (*secret, version))
    })?;
    Some(seal_bytes(preferred_nickname.as_bytes(), &secret, version))
}

/// Decrypt the room's current-version secret out of a raw network
/// `ChatRoomStateV1`, for the member who holds `self_sk`. Returns `None` for
/// a public room, when the blob for the current version has not been issued
/// for this member yet, or when decryption fails. Mirrors the UI's
/// `current_secret_from_state` in `ui/src/room_data.rs`.
fn current_secret_from_state(
    state: &ChatRoomStateV1,
    self_sk: &SigningKey,
) -> Option<([u8; 32], u32)> {
    let member_id = MemberId::from(&self_sk.verifying_key());
    let version = state.secrets.current_version;
    let blob = state
        .secrets
        .encrypted_secrets
        .iter()
        .find(|s| s.secret.member_id == member_id && s.secret.secret_version == version)?;
    let secret = decrypt_secret_from_member_blob_raw(
        &blob.secret.ciphertext,
        &blob.secret.nonce,
        &blob.secret.sender_ephemeral_public_key,
        self_sk,
    )
    .ok()?;
    Some((secret, version))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use river_core::ecies::{encrypt_secret_for_member, generate_room_secret};
    use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
    use river_core::room_state::privacy::PrivacyMode;
    use river_core::room_state::secret::{
        AuthorizedEncryptedSecretForMember, EncryptedSecretForMemberV1,
    };

    fn fresh_signing_key() -> SigningKey {
        SigningKey::from_bytes(&rand::Rng::gen::<[u8; 32]>(&mut rand::thread_rng()))
    }

    fn state_with_privacy(owner: &SigningKey, privacy: PrivacyMode) -> ChatRoomStateV1 {
        let mut state = ChatRoomStateV1::default();
        let config = Configuration {
            owner_member_id: owner.verifying_key().into(),
            privacy_mode: privacy,
            ..Configuration::default()
        };
        state.configuration = AuthorizedConfigurationV1::new(config, owner);
        state
    }

    /// Build an owner-signed `AuthorizedEncryptedSecretForMember` blob
    /// addressed to `recipient_vk`, carrying `secret` at `version`. Mirrors
    /// the UI's room-creation path (which uses `generate_room_secret` →
    /// `encrypt_secret_for_member` → `AuthorizedEncryptedSecretForMember`),
    /// so tests built on top exercise the actual contract-blob shape rather
    /// than a synthetic one.
    fn owner_blob(
        owner_sk: &SigningKey,
        recipient_vk: &ed25519_dalek::VerifyingKey,
        version: u32,
        secret: [u8; 32],
    ) -> AuthorizedEncryptedSecretForMember {
        let (ciphertext, nonce, ephemeral) = encrypt_secret_for_member(&secret, recipient_vk);
        let inner = EncryptedSecretForMemberV1 {
            member_id: (*recipient_vk).into(),
            secret_version: version,
            ciphertext,
            nonce,
            sender_ephemeral_public_key: ephemeral.to_bytes(),
            provider: owner_sk.verifying_key().into(),
        };
        AuthorizedEncryptedSecretForMember::new(inner, owner_sk)
    }

    #[test]
    fn collect_secrets_public_room_is_empty() {
        let owner = fresh_signing_key();
        let state = state_with_privacy(&owner, PrivacyMode::Public);
        let secrets = collect_secrets_for_room(&state, &owner, &HashMap::new());
        assert!(secrets.is_empty(), "public room should yield no secrets");
    }

    /// Owner decrypts the OWNER-ADDRESSED contract blob even though the
    /// initial secret is RANDOM (`generate_room_secret()` in the UI), not
    /// derived from the owner seed. This pins the fix for the P1 codex
    /// finding on PR #303: earlier drafts called `derive_room_secret` for
    /// owners, which would have produced the wrong v0 for any UI-created
    /// private room a CLI owner later acted on.
    #[test]
    fn owner_recovers_random_v0_from_contract_blob_not_via_derivation() {
        let owner = fresh_signing_key();
        let mut state = state_with_privacy(&owner, PrivacyMode::Private);
        let actual_v0 = generate_room_secret();
        state.secrets.encrypted_secrets.push(owner_blob(
            &owner,
            &owner.verifying_key(),
            0,
            actual_v0,
        ));
        state.secrets.current_version = 0;

        let secrets = collect_secrets_for_room(&state, &owner, &HashMap::new());
        assert_eq!(
            secrets.get(&0),
            Some(&actual_v0),
            "owner must recover the actual random v0 from its own contract blob"
        );
    }

    /// Non-owner branch (the load-bearing path when a non-owner CLI member
    /// invites someone else): the member's own blob in
    /// `encrypted_secrets` decrypts under their `self_sk`. Testing-reviewer
    /// finding #3 (`collect_secrets_for_room` non-owner branch untested).
    #[test]
    fn non_owner_decrypts_own_blob_from_state() {
        let owner = fresh_signing_key();
        let non_owner = fresh_signing_key();
        let mut state = state_with_privacy(&owner, PrivacyMode::Private);
        let secret_v0 = generate_room_secret();
        state.secrets.encrypted_secrets.push(owner_blob(
            &owner,
            &non_owner.verifying_key(),
            0,
            secret_v0,
        ));
        // Also include the owner's own blob — the non-owner MUST ignore it
        // and only decrypt the one addressed to themselves.
        let owner_secret_v0 = generate_room_secret();
        state.secrets.encrypted_secrets.push(owner_blob(
            &owner,
            &owner.verifying_key(),
            0,
            owner_secret_v0,
        ));
        state.secrets.current_version = 0;

        let secrets = collect_secrets_for_room(&state, &non_owner, &HashMap::new());
        assert_eq!(
            secrets.get(&0),
            Some(&secret_v0),
            "non-owner must recover only their own addressed blob's secret"
        );
        // Sanity: the non-owner did NOT recover the owner's blob.
        assert_ne!(
            secrets.get(&0),
            Some(&owner_secret_v0),
            "non-owner must NOT recover the secret from a blob addressed to the owner"
        );
    }

    /// Owner-signed contract blob takes precedence over a wrong
    /// invitation-carried secret at the same version. Testing-reviewer
    /// finding #2; matches UI's `repopulate_secrets_contract_blob_overwrites_stale_invitation_secret`.
    #[test]
    fn contract_blob_wins_over_stale_invitation_secret_at_same_version() {
        let owner = fresh_signing_key();
        let mut state = state_with_privacy(&owner, PrivacyMode::Private);
        let authoritative_v0 = generate_room_secret();
        state.secrets.encrypted_secrets.push(owner_blob(
            &owner,
            &owner.verifying_key(),
            0,
            authoritative_v0,
        ));
        state.secrets.current_version = 0;

        // Fold in a WRONG invitation secret at the same version — must be
        // shadowed by the owner-signed blob.
        let mut inv = HashMap::new();
        inv.insert(0u32, [0xDEu8; 32]);
        let secrets = collect_secrets_for_room(&state, &owner, &inv);
        assert_eq!(
            secrets.get(&0),
            Some(&authoritative_v0),
            "owner-signed blob must win over a stale/wrong invitation secret \
             at the same version (otherwise a malicious or out-of-date inviter \
             can permanently shadow the authentic secret)"
        );
    }

    #[test]
    fn invitation_secrets_folded_in_for_unknown_versions() {
        let owner = fresh_signing_key();
        let state = state_with_privacy(&owner, PrivacyMode::Private);
        let mut inv_secrets = HashMap::new();
        inv_secrets.insert(7, [0xABu8; 32]);
        let secrets = collect_secrets_for_room(&state, &owner, &inv_secrets);
        assert_eq!(secrets.get(&7), Some(&[0xABu8; 32]));
    }

    #[test]
    fn seal_invitee_nickname_public_room_returns_public_bytes() {
        let owner = fresh_signing_key();
        let state = state_with_privacy(&owner, PrivacyMode::Public);
        let sealed = seal_invitee_nickname(&state, &owner, &HashMap::new(), "alice")
            .expect("public room always seals");
        assert!(sealed.is_public());
        assert_eq!(sealed.as_public_bytes(), Some(b"alice".as_ref()));
    }

    #[test]
    fn seal_invitee_nickname_private_room_uses_invitation_secret() {
        let owner = fresh_signing_key();
        let state = state_with_privacy(&owner, PrivacyMode::Private);
        let mut inv = HashMap::new();
        inv.insert(0u32, [0x42u8; 32]);
        let sealed = seal_invitee_nickname(&state, &owner, &inv, "alice")
            .expect("invitation-carried secret seals the nickname");
        assert!(sealed.is_private());
        assert_eq!(sealed.secret_version(), Some(0));
        assert_eq!(sealed.declared_len(), b"alice".len());
    }

    #[test]
    fn seal_invitee_nickname_private_room_returns_none_when_no_secret() {
        let owner = fresh_signing_key();
        let state = state_with_privacy(&owner, PrivacyMode::Private);
        // Empty invitation_secrets AND no contract blob for self → defer.
        let sealed = seal_invitee_nickname(&state, &fresh_signing_key(), &HashMap::new(), "alice");
        assert!(
            sealed.is_none(),
            "private room with no secret must defer rather than leak plaintext"
        );
    }

    /// Rotation between invitation creation and accept: the invitation
    /// carries only v0, but the room has rotated to current_version = 1.
    /// `seal_invitee_nickname` MUST return None even though the
    /// invitation_secrets map is non-empty — sealing under v0 when the
    /// current version is v1 would produce a SealedBytes at v0 that the
    /// other members can't unseal at v1. Testing-reviewer finding #1;
    /// matches UI's `seal_invitee_nickname_none_when_invitation_lacks_current_version`.
    #[test]
    fn seal_invitee_nickname_returns_none_when_invitation_lacks_current_version() {
        let owner = fresh_signing_key();
        let mut state = state_with_privacy(&owner, PrivacyMode::Private);
        state.secrets.current_version = 1; // Room rotated to v1
        let mut inv = HashMap::new();
        inv.insert(0u32, [0x42u8; 32]); // Invitation carries v0 only
        let sealed = seal_invitee_nickname(&state, &fresh_signing_key(), &inv, "alice");
        assert!(
            sealed.is_none(),
            "must defer when invitation_secrets lacks current_version — \
             sealing under v0 when room is at v1 yields a SealedBytes nobody can unseal"
        );
    }

    #[test]
    fn collect_invitation_secrets_is_sorted_by_version() {
        let mut secrets = HashMap::new();
        secrets.insert(5, [0x05u8; 32]);
        secrets.insert(0, [0x00u8; 32]);
        secrets.insert(2, [0x02u8; 32]);
        let vec = collect_invitation_secrets(&secrets);
        let versions: Vec<u32> = vec.iter().map(|(v, _)| *v).collect();
        assert_eq!(versions, vec![0, 2, 5]);
    }

    #[test]
    fn collect_invitation_secrets_empty_input_is_empty() {
        assert!(collect_invitation_secrets(&HashMap::new()).is_empty());
    }

    /// `merge_invitation_secrets` preserves pre-existing entries the new
    /// invitation doesn't carry — a re-accept of an older invitation must
    /// NOT silently drop newer versions the CLI already holds. Round-2
    /// skeptical-review finding H1 on PR #303.
    #[test]
    fn merge_invitation_secrets_preserves_existing_entries_new_does_not_carry() {
        let mut existing = HashMap::new();
        existing.insert(0, [0xAAu8; 32]);
        existing.insert(1, [0xBBu8; 32]); // v1 is in storage but NOT in the new invitation
        let incoming = vec![(0, [0xAAu8; 32])]; // re-accept of an older invite carrying only v0
        let merged = merge_invitation_secrets(existing, &incoming);
        assert_eq!(merged.get(&0), Some(&[0xAAu8; 32]));
        assert_eq!(
            merged.get(&1),
            Some(&[0xBBu8; 32]),
            "pre-existing v1 must NOT be dropped on re-accept of an older invitation"
        );
        assert_eq!(merged.len(), 2);
    }

    /// `merge_invitation_secrets` new-invitation entry WINS on collision —
    /// matches UI's `extend()` semantics where the right-hand-side entry
    /// overwrites. This is the right shape so a freshly-received owner-
    /// rotated invitation (carrying a newer secret) can replace any local
    /// stale entry at the same version.
    #[test]
    fn merge_invitation_secrets_new_wins_on_collision() {
        let mut existing = HashMap::new();
        existing.insert(0, [0x00u8; 32]); // stale local v0
        let incoming = vec![(0, [0xFFu8; 32])]; // newer invitation v0
        let merged = merge_invitation_secrets(existing, &incoming);
        assert_eq!(
            merged.get(&0),
            Some(&[0xFFu8; 32]),
            "new invitation entry must overwrite a stale local entry at the same version"
        );
    }

    /// Empty merge cases — both directions degenerate cleanly.
    #[test]
    fn merge_invitation_secrets_empty_cases() {
        let from_empty = merge_invitation_secrets(HashMap::new(), &[(0, [0x42u8; 32])]);
        assert_eq!(from_empty.get(&0), Some(&[0x42u8; 32]));

        let mut prior = HashMap::new();
        prior.insert(7, [0x07u8; 32]);
        let no_incoming = merge_invitation_secrets(prior.clone(), &[]);
        assert_eq!(no_incoming, prior);
    }

    /// Source-grep pin: `accept_invitation` MUST call
    /// `seal_invitee_nickname` so the deferred-member_info branch stays
    /// wired up. Without this, a refactor that drops the call and
    /// reverts to `SealedBytes::public(...)` would silently leak the
    /// nickname into a private room. Mirrors the UI's
    /// `seal_invitee_nickname_call_site_pinned` (UI side, in
    /// `ui/src/components/app/freenet_api/response_handler/get_response.rs`).
    /// Testing-reviewer finding #4.
    #[test]
    fn accept_invitation_calls_seal_invitee_nickname() {
        let api_src = include_str!("api.rs");
        assert!(
            api_src.contains("crate::private_room::seal_invitee_nickname("),
            "cli/src/api.rs must call `crate::private_room::seal_invitee_nickname` — \
             if you renamed the helper or refactored the accept path, update this pin \
             AND verify the deferred-member_info logic is still in place to avoid \
             leaking a plaintext nickname into a private room."
        );
        // Also pin the deferred-member_info shape: `member_info_delta` is a
        // local that `accept_invitation` builds from the `seal_invitee_nickname`
        // result. If a refactor turns it into an unconditional `Some(...)`
        // the pin should fail.
        assert!(
            api_src.contains("let member_info_delta = sealed_nickname.map("),
            "cli/src/api.rs must derive `member_info_delta` from the Option<SealedBytes> \
             returned by `seal_invitee_nickname`; do NOT make it unconditional."
        );
    }
}
