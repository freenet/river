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
use river_core::key_derivation::derive_room_secret;
use river_core::room_state::member::MemberId;
use river_core::room_state::privacy::{PrivacyMode, SealedBytes};
use river_core::room_state::ChatRoomStateV1;
use std::collections::HashMap;

/// Collect every room secret this CLI holds for a private room, keyed by
/// `secret_version`. Returns an empty map for a public room.
///
/// Sources, in order of authority:
/// 1. **Owner derivation** — when `is_owner`, the owner's signing-key seed
///    deterministically derives the secret for every version the contract has
///    an `encrypted_secrets` blob for. For an owned room with no blobs yet
///    (brand-new room about to be made private), v0 is derived as well so a
///    fresh invitation can carry it.
/// 2. **Owner-signed contract blobs** — every blob in
///    `state.secrets.encrypted_secrets` addressed to this member is decrypted
///    with `self_sk` and inserted. These are authoritative; if both the owner
///    blob and `invitation_secrets` carry the same version, the blob wins.
/// 3. **Persisted `invitation_secrets`** — secrets carried in via prior
///    `Invitation` artifacts (issue freenet/river#302) for versions the
///    contract has not yet provided an owner-signed blob for.
pub fn collect_secrets_for_room(
    state: &ChatRoomStateV1,
    self_sk: &SigningKey,
    invitation_secrets: &HashMap<u32, [u8; 32]>,
    is_owner: bool,
) -> HashMap<u32, [u8; 32]> {
    if state.configuration.configuration.privacy_mode != PrivacyMode::Private {
        return HashMap::new();
    }

    let mut secrets: HashMap<u32, [u8; 32]> = HashMap::new();

    if is_owner {
        let owner_vk = self_sk.verifying_key();
        let seed = self_sk.to_bytes();
        // Versions present in the contract — there is at most one secret-
        // generation per `secret_version` u32, so dedup via the map's
        // `entry`. For a fresh private room with no blobs yet, also seed v0
        // so the first invitation carries something.
        let mut versions: Vec<u32> = state
            .secrets
            .encrypted_secrets
            .iter()
            .map(|s| s.secret.secret_version)
            .collect();
        versions.push(state.secrets.current_version);
        versions.sort_unstable();
        versions.dedup();
        for v in versions {
            secrets
                .entry(v)
                .or_insert_with(|| derive_room_secret(&seed, &owner_vk, v));
        }
    } else {
        // Non-owner: decrypt every blob addressed to this member. Owner-signed,
        // so authoritative.
        let self_id = MemberId::from(&self_sk.verifying_key());
        for blob in state
            .secrets
            .encrypted_secrets
            .iter()
            .filter(|s| s.secret.member_id == self_id)
        {
            if let Ok(secret) = decrypt_secret_from_member_blob_raw(
                &blob.secret.ciphertext,
                &blob.secret.nonce,
                &blob.secret.sender_ephemeral_public_key,
                self_sk,
            ) {
                secrets.insert(blob.secret.secret_version, secret);
            }
        }
    }

    // Fold in invitation-carried secrets for versions the authoritative sources
    // did NOT supply. The owner-signed contract blob takes precedence — mirrors
    // `RoomData::repopulate_secrets_from_state` in the UI.
    for (&version, secret) in invitation_secrets {
        secrets.entry(version).or_insert(*secret);
    }

    secrets
}

/// Sort a secrets map into the wire-format `Vec<(version, secret)>` carried
/// by `Invitation::room_secrets`. Sorted ascending by version so the encoded
/// invitation is deterministic — the encoded string is fingerprinted for
/// processed-invite dedup, so it must be stable across decode/re-encode
/// cycles. Mirrors UI's `collect_invitation_secrets` in
/// `ui/src/components/members.rs`.
pub fn secrets_to_invitation_vec(secrets: &HashMap<u32, [u8; 32]>) -> Vec<(u32, [u8; 32])> {
    let mut out: Vec<(u32, [u8; 32])> = secrets.iter().map(|(&v, &s)| (v, s)).collect();
    out.sort_unstable_by_key(|(v, _)| *v);
    out
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
    use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
    use river_core::room_state::privacy::PrivacyMode;

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

    #[test]
    fn collect_secrets_public_room_is_empty() {
        let owner = fresh_signing_key();
        let state = state_with_privacy(&owner, PrivacyMode::Public);
        let secrets = collect_secrets_for_room(&state, &owner, &HashMap::new(), true);
        assert!(secrets.is_empty(), "public room should yield no secrets");
    }

    #[test]
    fn collect_secrets_owner_derives_v0_for_fresh_private_room() {
        let owner = fresh_signing_key();
        let state = state_with_privacy(&owner, PrivacyMode::Private);
        let secrets = collect_secrets_for_room(&state, &owner, &HashMap::new(), true);
        let expected = derive_room_secret(&owner.to_bytes(), &owner.verifying_key(), 0);
        assert_eq!(secrets.get(&0), Some(&expected));
        // Sanity: the derivation is deterministic, so a second call yields
        // the same bytes — mirrors the convergence property the UI rotation
        // path depends on.
        let again = collect_secrets_for_room(&state, &owner, &HashMap::new(), true);
        assert_eq!(secrets, again);
    }

    #[test]
    fn invitation_secrets_folded_in_for_unknown_versions() {
        let owner = fresh_signing_key();
        let state = state_with_privacy(&owner, PrivacyMode::Private);
        let mut inv_secrets = HashMap::new();
        inv_secrets.insert(7, [0xABu8; 32]);
        let secrets = collect_secrets_for_room(&state, &owner, &inv_secrets, true);
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

    #[test]
    fn secrets_to_invitation_vec_is_sorted_by_version() {
        let mut secrets = HashMap::new();
        secrets.insert(5, [0x05u8; 32]);
        secrets.insert(0, [0x00u8; 32]);
        secrets.insert(2, [0x02u8; 32]);
        let vec = secrets_to_invitation_vec(&secrets);
        let versions: Vec<u32> = vec.iter().map(|(v, _)| *v).collect();
        assert_eq!(versions, vec![0, 2, 5]);
    }

    #[test]
    fn secrets_to_invitation_vec_empty_input_is_empty() {
        assert!(secrets_to_invitation_vec(&HashMap::new()).is_empty());
    }
}
