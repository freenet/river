//! CLI-side helpers for private-room secret handling, mirroring the UI's
//! `ui/src/room_data.rs` (`seal_invitee_nickname`,
//! `current_secret_from_state`, the `repopulate`-style decrypt loop) and
//! `ui/src/components/members.rs` (`collect_invitation_secrets`) — see issue
//! freenet/river#302 for the long-term consolidation plan into a shared,
//! non-WASM-compiled crate. Until that lands, the two copies MUST emit
//! byte-identical results so a UI-issued invitation and a CLI-issued
//! invitation are wire-interchangeable.

use ed25519_dalek::SigningKey;
use river_core::ecies::{
    decrypt_secret_from_member_blob_raw, encrypt_with_symmetric_key, seal_bytes,
};
use river_core::room_state::content::{
    ActionContentV1, ReplyContentV1, TextContentV1, CONTENT_TYPE_REPLY, REPLY_CONTENT_VERSION,
};
use river_core::room_state::member::MemberId;
use river_core::room_state::message::{MessageId, RoomMessageBody};
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

/// Build the `RoomMessageBody` for an outgoing chat message.
///
/// For a **public** room this is the plaintext body (unchanged behaviour).
/// For a **private** room the plaintext is encrypted under the room's
/// current-version secret with AES-256-GCM, mirroring the UI's send path in
/// `ui/src/components/conversation.rs` (`TextContentV1::encode` →
/// `encrypt_with_symmetric_key` → `RoomMessageBody::private`). The secret is
/// resolved exactly like [`seal_invitee_nickname`]: from the member's own
/// owner-signed `encrypted_secrets` blob in contract state, falling back to
/// the secret carried in the `Invitation` this member joined with
/// (`invitation_secrets`). [`collect_secrets_for_room`] folds both sources.
///
/// Returns an error (rather than silently falling back to a public body,
/// which the contract rejects in a private room with "Cannot send public
/// messages in private room") when no secret is available for the current
/// version — see [`resolve_current_secret`]. Also errors if the resulting
/// body exceeds the room's `max_message_size` — see [`guard_message_size`].
pub fn build_message_body(
    state: &ChatRoomStateV1,
    self_sk: &SigningKey,
    invitation_secrets: &HashMap<u32, [u8; 32]>,
    text: String,
) -> Result<RoomMessageBody, String> {
    let content = if state.configuration.configuration.privacy_mode != PrivacyMode::Private {
        RoomMessageBody::public(text)
    } else {
        let (secret, version) = resolve_current_secret(state, self_sk, invitation_secrets)?;
        // Mirror the UI: CBOR-encode the text content, then AES-256-GCM seal
        // it under the current room secret with a fresh random nonce.
        let content_bytes = TextContentV1::new(text).encode();
        let (ciphertext, nonce) = encrypt_with_symmetric_key(&secret, &content_bytes);
        RoomMessageBody::private_text(ciphertext, nonce, version)
    };

    guard_message_size(state, content)
}

/// Build the `RoomMessageBody` for an outgoing **action** (edit / delete /
/// reaction / remove_reaction).
///
/// For a **public** room this is the plaintext action body (unchanged
/// behaviour, identical to `RoomMessageBody::{edit,delete,reaction,…}`). For a
/// **private** room the CBOR-encoded `ActionContentV1` is encrypted under the
/// room's current-version secret with AES-256-GCM and emitted as
/// `RoomMessageBody::private_action`, mirroring the UI's action paths in
/// `ui/src/components/conversation.rs`.
///
/// Secret resolution, the no-secret error (never a silent public fallback the
/// contract would reject in a private room), the stale-version guard, and the
/// over-`max_message_size` guard are all identical to [`build_message_body`] —
/// they share [`resolve_current_secret`] and [`guard_message_size`]. The
/// caller builds the `ActionContentV1` (e.g. `ActionContentV1::edit(target,
/// new_text)`); this helper owns the sealing and the public/private decision so
/// every action call site routes through the same privacy logic.
pub fn build_action_body(
    state: &ChatRoomStateV1,
    self_sk: &SigningKey,
    invitation_secrets: &HashMap<u32, [u8; 32]>,
    action: ActionContentV1,
) -> Result<RoomMessageBody, String> {
    let content = if state.configuration.configuration.privacy_mode != PrivacyMode::Private {
        // Public action body — byte-identical to the dedicated constructors
        // (`RoomMessageBody::edit/delete/reaction/remove_reaction`), which all
        // wrap `ActionContentV1::encode()` in a `Public` body.
        use river_core::room_state::content::{ACTION_CONTENT_VERSION, CONTENT_TYPE_ACTION};
        RoomMessageBody::public_raw(CONTENT_TYPE_ACTION, ACTION_CONTENT_VERSION, action.encode())
    } else {
        let (secret, version) = resolve_current_secret(state, self_sk, invitation_secrets)?;
        let content_bytes = action.encode();
        let (ciphertext, nonce) = encrypt_with_symmetric_key(&secret, &content_bytes);
        RoomMessageBody::private_action(ciphertext, nonce, version)
    };

    guard_message_size(state, content)
}

/// Build the `RoomMessageBody` for an outgoing **reply**.
///
/// For a **public** room this is the plaintext reply body (unchanged
/// behaviour, identical to `RoomMessageBody::reply`). For a **private** room
/// the CBOR-encoded `ReplyContentV1` is encrypted under the room's
/// current-version secret with AES-256-GCM and emitted as a private body with
/// `content_type = CONTENT_TYPE_REPLY`, mirroring the UI's reply path in
/// `ui/src/components/conversation.rs` (which uses
/// `RoomMessageBody::private(CONTENT_TYPE_REPLY, REPLY_CONTENT_VERSION, …)` —
/// there is no `private_reply` convenience constructor).
///
/// Secret resolution, the no-secret error, the stale-version guard, and the
/// over-`max_message_size` guard are identical to [`build_message_body`].
#[allow(clippy::too_many_arguments)]
pub fn build_reply_body(
    state: &ChatRoomStateV1,
    self_sk: &SigningKey,
    invitation_secrets: &HashMap<u32, [u8; 32]>,
    text: String,
    target_message_id: MessageId,
    target_author_name: String,
    target_content_preview: String,
) -> Result<RoomMessageBody, String> {
    let content = if state.configuration.configuration.privacy_mode != PrivacyMode::Private {
        RoomMessageBody::reply(
            text,
            target_message_id,
            target_author_name,
            target_content_preview,
        )
    } else {
        let (secret, version) = resolve_current_secret(state, self_sk, invitation_secrets)?;
        let reply = ReplyContentV1::new(
            text,
            target_message_id,
            target_author_name,
            target_content_preview,
        );
        let content_bytes = reply.encode();
        let (ciphertext, nonce) = encrypt_with_symmetric_key(&secret, &content_bytes);
        RoomMessageBody::private(
            CONTENT_TYPE_REPLY,
            REPLY_CONTENT_VERSION,
            ciphertext,
            nonce,
            version,
        )
    };

    guard_message_size(state, content)
}

/// Resolve the room's **current-version** secret for the member holding
/// `self_sk`, for sealing an outgoing private-room body.
///
/// Returns `(secret, current_version)` or an error (rather than silently
/// falling back to a public body, which the contract rejects in a private room
/// with "Cannot send public messages in private room") when no secret is
/// available for the current version — the room owner must re-provision this
/// member's secret, or the member must rejoin via a fresh invitation that
/// carries the current version. Sealing only ever under `current_version`
/// (never a stale version other members can't read) mirrors the
/// nickname-sealing guard in [`seal_invitee_nickname`].
///
/// Shared by [`build_message_body`], [`build_action_body`], and
/// [`build_reply_body`] so all four message kinds (text / action / reply) make
/// the identical secret-resolution decision — a divergence here would leak one
/// kind of private content as an unsealed public body.
fn resolve_current_secret(
    state: &ChatRoomStateV1,
    self_sk: &SigningKey,
    invitation_secrets: &HashMap<u32, [u8; 32]>,
) -> Result<([u8; 32], u32), String> {
    let version = state.secrets.current_version;
    let secrets = collect_secrets_for_room(state, self_sk, invitation_secrets);
    let secret = secrets.get(&version).copied().ok_or_else(|| {
        format!(
            "private room: no secret available for the current version (v{version}). \
             Sealing the message would be impossible. The room owner must share the \
             current room secret with this member (re-key / re-provision), or this \
             member must rejoin via a fresh invitation carrying v{version}."
        )
    })?;
    Ok((secret, version))
}

/// Fail loudly on an over-`max_message_size` body instead of letting the
/// contract drop it silently.
///
/// The contract enforces `max_message_size` by *silently dropping* the message
/// in `MessagesV1::apply_delta` (a `retain`, not an `Err`), so without this
/// guard a too-long message would report success while never being delivered.
/// The limit is measured against the body's `content_len()` — for a private
/// room that is the AES-256-GCM **ciphertext** length (raw content + a 16-byte
/// authentication tag + CBOR framing), so content that fits as public can
/// exceed the limit once sealed. Mirrors the UI's pre-send guard in
/// `ui/src/components/conversation.rs`. Shared by every `build_*` helper so the
/// guard can never drift between message kinds.
fn guard_message_size(
    state: &ChatRoomStateV1,
    content: RoomMessageBody,
) -> Result<RoomMessageBody, String> {
    let max = state.configuration.configuration.max_message_size;
    let len = content.content_len();
    if len > max {
        return Err(format!(
            "message too large: {len} encoded bytes exceeds the room's \
             max_message_size of {max} bytes.{}",
            if content.is_private() {
                " Private-room messages are AES-256-GCM sealed, which adds a \
                 16-byte authentication tag plus CBOR framing over the raw content."
            } else {
                ""
            }
        ));
    }
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use river_core::ecies::{
        decrypt_with_symmetric_key, encrypt_secret_for_member, generate_room_secret,
    };
    use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
    use river_core::room_state::content::{ActionContentV1, ReplyContentV1, CONTENT_TYPE_REPLY};
    use river_core::room_state::message::MessageId;
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
    fn build_message_body_public_room_is_plaintext() {
        let owner = fresh_signing_key();
        let state = state_with_privacy(&owner, PrivacyMode::Public);
        let body = build_message_body(&state, &owner, &HashMap::new(), "hello".to_string())
            .expect("public room always builds a body");
        match body {
            RoomMessageBody::Public { data, .. } => {
                let decoded = TextContentV1::decode(&data).expect("valid text content");
                assert_eq!(decoded.text, "hello");
            }
            RoomMessageBody::Private { .. } => panic!("public room must not seal the body"),
        }
    }

    #[test]
    fn build_message_body_private_room_seals_and_roundtrips_via_invitation_secret() {
        let owner = fresh_signing_key();
        let state = state_with_privacy(&owner, PrivacyMode::Private); // current_version = 0
        let secret = [0x42u8; 32];
        let mut inv = HashMap::new();
        inv.insert(0u32, secret);

        let body = build_message_body(&state, &owner, &inv, "secret hi".to_string())
            .expect("invitation-carried secret seals the body");

        match body {
            RoomMessageBody::Private {
                ciphertext,
                nonce,
                secret_version,
                ..
            } => {
                assert_eq!(secret_version, 0, "must seal under the current version");
                let plaintext = decrypt_with_symmetric_key(&secret, &ciphertext, &nonce)
                    .expect("the sealed body decrypts under the room secret");
                let decoded = TextContentV1::decode(&plaintext).expect("valid text content");
                assert_eq!(decoded.text, "secret hi");
            }
            RoomMessageBody::Public { .. } => panic!("private room must seal the body"),
        }
    }

    #[test]
    fn build_message_body_private_room_seals_via_contract_blob() {
        // The member's secret lives only in an owner-signed contract blob
        // (the steady-state path, once the owner has provisioned the member).
        let owner = fresh_signing_key();
        let mut state = state_with_privacy(&owner, PrivacyMode::Private);
        let secret = [0x7eu8; 32];
        state
            .secrets
            .encrypted_secrets
            .push(owner_blob(&owner, &owner.verifying_key(), 0, secret));

        let body = build_message_body(&state, &owner, &HashMap::new(), "from blob".to_string())
            .expect("contract-blob secret seals the body");

        match body {
            RoomMessageBody::Private {
                ciphertext,
                nonce,
                secret_version,
                ..
            } => {
                assert_eq!(secret_version, 0);
                let plaintext = decrypt_with_symmetric_key(&secret, &ciphertext, &nonce)
                    .expect("decrypts under the blob-carried secret");
                assert_eq!(TextContentV1::decode(&plaintext).unwrap().text, "from blob");
            }
            RoomMessageBody::Public { .. } => panic!("private room must seal the body"),
        }
    }

    #[test]
    fn build_message_body_private_room_errors_without_secret() {
        let owner = fresh_signing_key();
        let state = state_with_privacy(&owner, PrivacyMode::Private);
        // A member with neither a contract blob nor an invitation secret for
        // the current version must error — never silently send a public body
        // that the contract would reject ("Cannot send public messages...").
        let err = build_message_body(
            &state,
            &fresh_signing_key(),
            &HashMap::new(),
            "nope".to_string(),
        )
        .expect_err("must refuse to send when no secret is available");
        assert!(
            err.contains("no secret available"),
            "error should explain the missing secret, got: {err}"
        );
    }

    #[test]
    fn build_message_body_private_room_errors_when_secret_lacks_current_version() {
        // Room rotated to v1 but the member only holds v0 — sealing under v0
        // would be unreadable by members at v1, so refuse (mirrors the
        // nickname-sealing guard).
        let owner = fresh_signing_key();
        let mut state = state_with_privacy(&owner, PrivacyMode::Private);
        state.secrets.current_version = 1;
        let mut inv = HashMap::new();
        inv.insert(0u32, [0x42u8; 32]);
        let err = build_message_body(&state, &fresh_signing_key(), &inv, "stale".to_string())
            .expect_err("must refuse to seal under a non-current version");
        assert!(
            err.contains("v1"),
            "error should name the current version: {err}"
        );
    }

    #[test]
    fn build_message_body_errors_when_body_exceeds_max_message_size() {
        // The contract silently DROPS an over-size message (a `retain` in
        // `MessagesV1::apply_delta`), so the helper must refuse loudly rather
        // than let a send report success while delivering nothing.
        let owner = fresh_signing_key();
        let mut state = state_with_privacy(&owner, PrivacyMode::Private);
        // Tiny limit so a normal-length message overflows once sealed.
        let mut config = state.configuration.configuration.clone();
        config.max_message_size = 4;
        state.configuration = AuthorizedConfigurationV1::new(config, &owner);
        let mut inv = HashMap::new();
        inv.insert(0u32, [0x42u8; 32]);

        let err = build_message_body(
            &state,
            &owner,
            &inv,
            "this is definitely longer than four bytes".to_string(),
        )
        .expect_err("over-size body must be rejected, not silently dropped");
        assert!(
            err.contains("too large") && err.contains("max_message_size"),
            "error should explain the size limit, got: {err}"
        );
    }

    /// End-to-end contract acceptance: a body produced by `build_message_body`
    /// must be accepted by the room contract's own `apply_delta` — the same
    /// validation `send_message` runs locally before transmitting. This closes
    /// the gap the unit tests above leave open (they only check the sealing
    /// math against a state whose `secrets.versions` is empty, which the
    /// contract would reject). Requires no node.
    #[test]
    fn build_message_body_output_is_accepted_by_contract_apply_delta() {
        use freenet_scaffold::ComposableState;
        use river_core::room_state::message::{AuthorizedMessageV1, MessageV1};
        use river_core::room_state::privacy::RoomCipherSpec;
        use river_core::room_state::secret::{
            AuthorizedSecretVersionRecord, RoomSecretsV1, SecretVersionRecordV1,
        };
        use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};

        let owner = fresh_signing_key();
        let owner_vk = owner.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let mut state = state_with_privacy(&owner, PrivacyMode::Private);

        // Provision a genuine v0 secret: an owner-signed version record (so the
        // contract's `secret_version ∈ versions` check passes) plus the
        // owner's own owner-signed `encrypted_secrets` blob.
        let secret = [0x33u8; 32];
        state.secrets = RoomSecretsV1 {
            current_version: 0,
            versions: vec![AuthorizedSecretVersionRecord::new(
                SecretVersionRecordV1 {
                    version: 0,
                    cipher_spec: RoomCipherSpec::Aes256Gcm,
                    created_at: std::time::SystemTime::now(),
                },
                &owner,
            )],
            encrypted_secrets: vec![owner_blob(&owner, &owner_vk, 0, secret)],
        };

        let content = build_message_body(&state, &owner, &HashMap::new(), "ci ping".to_string())
            .expect("seals under v0");

        // The owner is always an accepted author, so no member entry is needed.
        let message = MessageV1 {
            room_owner: owner_id,
            author: owner_id,
            content,
            time: std::time::SystemTime::now(),
        };
        let auth = AuthorizedMessageV1::new(message, &owner);
        let delta = ChatRoomStateV1Delta {
            recent_messages: Some(vec![auth]),
            ..Default::default()
        };
        let params = ChatRoomParametersV1 { owner: owner_vk };

        let mut applied = state.clone();
        applied
            .apply_delta(&state, &params, &Some(delta))
            .expect("the contract accepts the sealed private message");
        assert_eq!(
            applied.recent_messages.messages.len(),
            1,
            "the sealed message must be retained, not silently dropped"
        );
        assert!(
            applied.recent_messages.messages[0]
                .message
                .content
                .is_private(),
            "the accepted body must be the sealed (private) form"
        );
    }

    // ------------------------------------------------------------------
    // build_action_body — edit / delete / reaction / remove_reaction (#351)
    // ------------------------------------------------------------------

    /// A fresh MessageId to target with an action/reply in tests.
    fn target_id() -> MessageId {
        use river_core::room_state::message::{AuthorizedMessageV1, MessageV1};
        let owner = fresh_signing_key();
        let msg = MessageV1 {
            room_owner: MemberId::from(&owner.verifying_key()),
            author: MemberId::from(&owner.verifying_key()),
            content: RoomMessageBody::public("target".to_string()),
            time: std::time::SystemTime::now(),
        };
        AuthorizedMessageV1::new(msg, &owner).id()
    }

    /// Public room → the action body is the plaintext form, byte-identical to
    /// the dedicated `RoomMessageBody::edit` constructor. Pins that routing
    /// `edit` through `build_action_body` does not change the public wire
    /// bytes (so existing public-room behaviour is unaffected).
    #[test]
    fn build_action_body_public_room_matches_dedicated_constructor() {
        let owner = fresh_signing_key();
        let state = state_with_privacy(&owner, PrivacyMode::Public);
        let tgt = target_id();

        let via_helper = build_action_body(
            &state,
            &owner,
            &HashMap::new(),
            ActionContentV1::edit(tgt.clone(), "new text".to_string()),
        )
        .expect("public room always builds a body");
        let direct = RoomMessageBody::edit(tgt, "new text".to_string());
        assert_eq!(
            via_helper, direct,
            "public action body must be byte-identical to the dedicated constructor"
        );
        assert!(via_helper.is_public(), "public room must not seal the body");
    }

    /// Private room → each of the four action kinds seals under the current
    /// version and round-trips back to the original `ActionContentV1`. This is
    /// the core #351 guarantee: action content is never emitted in the clear
    /// in a private room.
    #[test]
    fn build_action_body_private_room_seals_and_roundtrips_all_action_kinds() {
        let owner = fresh_signing_key();
        let state = state_with_privacy(&owner, PrivacyMode::Private); // current_version = 0
        let secret = [0x42u8; 32];
        let mut inv = HashMap::new();
        inv.insert(0u32, secret);
        let tgt = target_id();

        let cases = vec![
            ActionContentV1::edit(tgt.clone(), "edited".to_string()),
            ActionContentV1::delete(tgt.clone()),
            ActionContentV1::reaction(tgt.clone(), "👍".to_string()),
            ActionContentV1::remove_reaction(tgt.clone(), "👍".to_string()),
        ];

        for action in cases {
            let expected = action.clone();
            let body = build_action_body(&state, &owner, &inv, action)
                .expect("invitation-carried secret seals the action body");
            match body {
                RoomMessageBody::Private {
                    content_type,
                    ciphertext,
                    nonce,
                    secret_version,
                    ..
                } => {
                    use river_core::room_state::content::CONTENT_TYPE_ACTION;
                    assert_eq!(
                        content_type, CONTENT_TYPE_ACTION,
                        "sealed action keeps its content_type"
                    );
                    assert_eq!(secret_version, 0, "must seal under the current version");
                    let plaintext = decrypt_with_symmetric_key(&secret, &ciphertext, &nonce)
                        .expect("the sealed action decrypts under the room secret");
                    let decoded =
                        ActionContentV1::decode(&plaintext).expect("valid action content");
                    assert_eq!(
                        decoded, expected,
                        "decrypted action must equal the original (kind {})",
                        expected.action_type
                    );
                }
                RoomMessageBody::Public { .. } => {
                    panic!("private room must seal the action body")
                }
            }
        }
    }

    /// Private room, secret only in the owner-signed contract blob → still
    /// seals (the steady-state path once the owner has provisioned the member).
    #[test]
    fn build_action_body_private_room_seals_via_contract_blob() {
        let owner = fresh_signing_key();
        let mut state = state_with_privacy(&owner, PrivacyMode::Private);
        let secret = [0x7eu8; 32];
        state
            .secrets
            .encrypted_secrets
            .push(owner_blob(&owner, &owner.verifying_key(), 0, secret));
        let tgt = target_id();

        let body = build_action_body(
            &state,
            &owner,
            &HashMap::new(),
            ActionContentV1::reaction(tgt, "🎉".to_string()),
        )
        .expect("contract-blob secret seals the action body");

        match body {
            RoomMessageBody::Private {
                ciphertext,
                nonce,
                secret_version,
                ..
            } => {
                assert_eq!(secret_version, 0);
                let plaintext = decrypt_with_symmetric_key(&secret, &ciphertext, &nonce)
                    .expect("decrypts under the blob-carried secret");
                let decoded = ActionContentV1::decode(&plaintext).unwrap();
                assert_eq!(decoded.reaction_payload().unwrap().emoji, "🎉");
            }
            RoomMessageBody::Public { .. } => panic!("private room must seal the body"),
        }
    }

    /// Private room, no secret anywhere → error, never a silent public body
    /// (which the contract rejects with "Cannot send public messages...").
    #[test]
    fn build_action_body_private_room_errors_without_secret() {
        let owner = fresh_signing_key();
        let state = state_with_privacy(&owner, PrivacyMode::Private);
        let tgt = target_id();
        let err = build_action_body(
            &state,
            &fresh_signing_key(),
            &HashMap::new(),
            ActionContentV1::delete(tgt),
        )
        .expect_err("must refuse to send an action when no secret is available");
        assert!(
            err.contains("no secret available"),
            "error should explain the missing secret, got: {err}"
        );
    }

    /// Private room rotated past the held version → error (won't seal under a
    /// non-current version other members can't read).
    #[test]
    fn build_action_body_private_room_errors_when_secret_lacks_current_version() {
        let owner = fresh_signing_key();
        let mut state = state_with_privacy(&owner, PrivacyMode::Private);
        state.secrets.current_version = 1;
        let mut inv = HashMap::new();
        inv.insert(0u32, [0x42u8; 32]);
        let tgt = target_id();
        let err = build_action_body(
            &state,
            &fresh_signing_key(),
            &inv,
            ActionContentV1::edit(tgt, "stale".to_string()),
        )
        .expect_err("must refuse to seal under a non-current version");
        assert!(
            err.contains("v1"),
            "error should name the current version: {err}"
        );
    }

    /// Over-`max_message_size` sealed action body → error, not silent drop.
    #[test]
    fn build_action_body_errors_when_body_exceeds_max_message_size() {
        let owner = fresh_signing_key();
        let mut state = state_with_privacy(&owner, PrivacyMode::Private);
        let mut config = state.configuration.configuration.clone();
        config.max_message_size = 4;
        state.configuration = AuthorizedConfigurationV1::new(config, &owner);
        let mut inv = HashMap::new();
        inv.insert(0u32, [0x42u8; 32]);
        let tgt = target_id();

        let err = build_action_body(
            &state,
            &owner,
            &inv,
            ActionContentV1::edit(tgt, "this is definitely longer than four bytes".to_string()),
        )
        .expect_err("over-size action body must be rejected, not silently dropped");
        assert!(
            err.contains("too large") && err.contains("max_message_size"),
            "error should explain the size limit, got: {err}"
        );
    }

    // ------------------------------------------------------------------
    // build_reply_body (#351)
    // ------------------------------------------------------------------

    /// Public room → the reply body is byte-identical to the dedicated
    /// `RoomMessageBody::reply` constructor.
    #[test]
    fn build_reply_body_public_room_matches_dedicated_constructor() {
        let owner = fresh_signing_key();
        let state = state_with_privacy(&owner, PrivacyMode::Public);
        let tgt = target_id();

        let via_helper = build_reply_body(
            &state,
            &owner,
            &HashMap::new(),
            "my reply".to_string(),
            tgt.clone(),
            "Alice".to_string(),
            "original".to_string(),
        )
        .expect("public room always builds a body");
        let direct = RoomMessageBody::reply(
            "my reply".to_string(),
            tgt,
            "Alice".to_string(),
            "original".to_string(),
        );
        assert_eq!(
            via_helper, direct,
            "public reply body must be byte-identical to the dedicated constructor"
        );
    }

    /// Private room → the reply seals under the current version (content_type
    /// = CONTENT_TYPE_REPLY) and round-trips back, INCLUDING the target author
    /// name and content preview, which must not leak in the clear.
    #[test]
    fn build_reply_body_private_room_seals_and_roundtrips() {
        let owner = fresh_signing_key();
        let state = state_with_privacy(&owner, PrivacyMode::Private);
        let secret = [0x42u8; 32];
        let mut inv = HashMap::new();
        inv.insert(0u32, secret);
        let tgt = target_id();

        let body = build_reply_body(
            &state,
            &owner,
            &inv,
            "secret reply".to_string(),
            tgt.clone(),
            "Sensitive Name".to_string(),
            "sensitive preview".to_string(),
        )
        .expect("invitation-carried secret seals the reply body");

        match body {
            RoomMessageBody::Private {
                content_type,
                ciphertext,
                nonce,
                secret_version,
                ..
            } => {
                assert_eq!(
                    content_type, CONTENT_TYPE_REPLY,
                    "sealed reply must carry CONTENT_TYPE_REPLY"
                );
                assert_eq!(secret_version, 0, "must seal under the current version");
                let plaintext = decrypt_with_symmetric_key(&secret, &ciphertext, &nonce)
                    .expect("the sealed reply decrypts under the room secret");
                let decoded = ReplyContentV1::decode(&plaintext).expect("valid reply content");
                assert_eq!(decoded.text, "secret reply");
                assert_eq!(decoded.target_message_id, tgt);
                assert_eq!(
                    decoded.target_author_name, "Sensitive Name",
                    "the target author name must be sealed, not leaked"
                );
                assert_eq!(
                    decoded.target_content_preview, "sensitive preview",
                    "the target content preview must be sealed, not leaked"
                );
            }
            RoomMessageBody::Public { .. } => panic!("private room must seal the reply body"),
        }
    }

    /// Private room, no secret → error (never a public reply body).
    #[test]
    fn build_reply_body_private_room_errors_without_secret() {
        let owner = fresh_signing_key();
        let state = state_with_privacy(&owner, PrivacyMode::Private);
        let tgt = target_id();
        let err = build_reply_body(
            &state,
            &fresh_signing_key(),
            &HashMap::new(),
            "nope".to_string(),
            tgt,
            "Alice".to_string(),
            "original".to_string(),
        )
        .expect_err("must refuse to send a reply when no secret is available");
        assert!(
            err.contains("no secret available"),
            "error should explain the missing secret, got: {err}"
        );
    }

    /// Private room rotated past the held version → reply errors.
    #[test]
    fn build_reply_body_private_room_errors_when_secret_lacks_current_version() {
        let owner = fresh_signing_key();
        let mut state = state_with_privacy(&owner, PrivacyMode::Private);
        state.secrets.current_version = 1;
        let mut inv = HashMap::new();
        inv.insert(0u32, [0x42u8; 32]);
        let tgt = target_id();
        let err = build_reply_body(
            &state,
            &fresh_signing_key(),
            &inv,
            "stale".to_string(),
            tgt,
            "Alice".to_string(),
            "original".to_string(),
        )
        .expect_err("must refuse to seal a reply under a non-current version");
        assert!(
            err.contains("v1"),
            "error should name the current version: {err}"
        );
    }

    /// Over-`max_message_size` sealed reply body → error, not silent drop.
    #[test]
    fn build_reply_body_errors_when_body_exceeds_max_message_size() {
        let owner = fresh_signing_key();
        let mut state = state_with_privacy(&owner, PrivacyMode::Private);
        let mut config = state.configuration.configuration.clone();
        config.max_message_size = 4;
        state.configuration = AuthorizedConfigurationV1::new(config, &owner);
        let mut inv = HashMap::new();
        inv.insert(0u32, [0x42u8; 32]);
        let tgt = target_id();

        let err = build_reply_body(
            &state,
            &owner,
            &inv,
            "this is definitely longer than four bytes".to_string(),
            tgt,
            "Alice".to_string(),
            "original".to_string(),
        )
        .expect_err("over-size reply body must be rejected, not silently dropped");
        assert!(
            err.contains("too large") && err.contains("max_message_size"),
            "error should explain the size limit, got: {err}"
        );
    }

    /// End-to-end contract acceptance: action and reply bodies produced by the
    /// new helpers must be accepted by the room contract's own `apply_delta`
    /// (the same validation the action/reply send paths run locally before
    /// transmitting). Closes the gap the sealing-math unit tests leave open —
    /// they seal against a state with an empty `secrets.versions`, which the
    /// contract would reject. Requires no node. Mirrors
    /// `build_message_body_output_is_accepted_by_contract_apply_delta`.
    #[test]
    fn build_action_and_reply_bodies_accepted_by_contract_apply_delta() {
        use freenet_scaffold::ComposableState;
        use river_core::room_state::message::{AuthorizedMessageV1, MessageV1};
        use river_core::room_state::privacy::RoomCipherSpec;
        use river_core::room_state::secret::{
            AuthorizedSecretVersionRecord, RoomSecretsV1, SecretVersionRecordV1,
        };
        use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};

        let owner = fresh_signing_key();
        let owner_vk = owner.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let mut state = state_with_privacy(&owner, PrivacyMode::Private);

        // Provision a genuine v0 secret (owner-signed version record + owner's
        // own owner-signed encrypted_secrets blob).
        let secret = [0x33u8; 32];
        state.secrets = RoomSecretsV1 {
            current_version: 0,
            versions: vec![AuthorizedSecretVersionRecord::new(
                SecretVersionRecordV1 {
                    version: 0,
                    cipher_spec: RoomCipherSpec::Aes256Gcm,
                    created_at: std::time::SystemTime::now(),
                },
                &owner,
            )],
            encrypted_secrets: vec![owner_blob(&owner, &owner_vk, 0, secret)],
        };

        // First seal & apply a plain text message so it exists as a reply/edit
        // target inside the recent window.
        let text_body = build_message_body(&state, &owner, &HashMap::new(), "original".to_string())
            .expect("seals the text body under v0");
        let text_msg = MessageV1 {
            room_owner: owner_id,
            author: owner_id,
            content: text_body,
            time: std::time::SystemTime::now(),
        };
        let text_auth = AuthorizedMessageV1::new(text_msg, &owner);
        let target_msg_id = text_auth.id();
        let params = ChatRoomParametersV1 { owner: owner_vk };
        let mut applied = state.clone();
        applied
            .apply_delta(
                &state,
                &params,
                &Some(ChatRoomStateV1Delta {
                    recent_messages: Some(vec![text_auth]),
                    ..Default::default()
                }),
            )
            .expect("contract accepts the sealed text message");

        // Now seal an action (reaction) and a reply targeting that message and
        // confirm both are accepted and retained as private bodies.
        let action_body = build_action_body(
            &applied,
            &owner,
            &HashMap::new(),
            ActionContentV1::reaction(target_msg_id.clone(), "👍".to_string()),
        )
        .expect("seals the action body under v0");
        let reply_body = build_reply_body(
            &applied,
            &owner,
            &HashMap::new(),
            "a sealed reply".to_string(),
            target_msg_id,
            "owner".to_string(),
            "original".to_string(),
        )
        .expect("seals the reply body under v0");

        let action_auth = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner_id,
                author: owner_id,
                content: action_body,
                time: std::time::SystemTime::now(),
            },
            &owner,
        );
        let reply_auth = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner_id,
                author: owner_id,
                content: reply_body,
                time: std::time::SystemTime::now(),
            },
            &owner,
        );

        let before = applied.clone();
        applied
            .apply_delta(
                &before,
                &params,
                &Some(ChatRoomStateV1Delta {
                    recent_messages: Some(vec![action_auth, reply_auth]),
                    ..Default::default()
                }),
            )
            .expect("the contract accepts the sealed action and reply");

        // Both the action message and the reply message are retained in
        // recent_messages (the contract keeps action messages alongside
        // computing `actions_state` on top of them), each as a sealed private
        // body — never silently dropped, never downgraded to public.
        use river_core::room_state::content::CONTENT_TYPE_ACTION;
        let reply = applied
            .recent_messages
            .messages
            .iter()
            .find(|m| m.message.content.content_type() == CONTENT_TYPE_REPLY)
            .expect("the sealed reply must be retained, not silently dropped");
        assert!(
            reply.message.content.is_private(),
            "the accepted reply body must be the sealed (private) form"
        );
        let action = applied
            .recent_messages
            .messages
            .iter()
            .find(|m| m.message.content.content_type() == CONTENT_TYPE_ACTION)
            .expect("the sealed action must be retained, not silently dropped");
        assert!(
            action.message.content.is_private(),
            "the accepted action body must be the sealed (private) form"
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
